use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;

#[cfg(not(test))]
use directories::ProjectDirs;
use keyring_core::Entry;
use log::warn;
use serde::{ Deserialize, Serialize };

pub mod history;
#[cfg(all(target_os = "macos", not(test)))]
mod sealed_password;

const KEYRING_SERVICE: &str = "snack";
const CONFIG_FILE: &str = "config.toml";
const HISTORY_FILE: &str = "history.sqlite";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedConfig
{
    // Failed loads are never writable defaults. Only explicit recovery clears this.
    #[serde(skip)]
    pub load_error: Option<String>,
    #[serde(default)]
    pub jid: Option<String>,
    #[serde(default)]
    pub pending_credential_removals: Vec<String>,
    #[serde(default)]
    pub rooms: Vec<String>,
    #[serde(default)]
    pub room_account: Option<String>,
    #[serde(default)]
    pub account_rooms: HashMap<String, Vec<String>>,
    // Unsent message text, keyed by room/contact JID, so each conversation
    // keeps its own in-progress draft across room switches and restarts.
    #[serde(default)]
    pub drafts: HashMap<String, String>,
    #[serde(default)]
    pub draft_account: Option<String>,
    #[serde(default)]
    pub account_drafts: HashMap<String, HashMap<String, String>>,
}

impl SavedConfig
{
    // Legacy files can identify an owner through the remembered login. Migrate
    // this before forgetting credentials can remove that ownership evidence.
    fn migrate_legacy_ownership(&mut self)
    {
        if self.draft_account.is_none() {
            self.draft_account = self.jid.as_deref().map(bare_account);
        }
        if self.room_account.is_none() {
            self.room_account = self.draft_account.as_deref().or(self.jid.as_deref()).map(bare_account);
        }
        self.draft_account = self.draft_account.as_deref().map(bare_account);
        self.room_account = self.room_account.as_deref().map(bare_account);
    }

    // The empty key is reserved for legacy data whose owner cannot be known.
    // Account JIDs are validated before this method is called, so that bucket
    // can never be activated by a login or used to auto-join rooms.
    pub fn activate_account(&mut self, account: &str) -> bool
    {
        self.migrate_legacy_ownership();
        let account = bare_account(account);
        let drafts_changed = self.draft_account.as_deref() != Some(account.as_str());
        if drafts_changed {
            let previous = self.draft_account.take().unwrap_or_default();
            self.account_drafts.insert(previous, std::mem::take(&mut self.drafts));
            self.drafts = self.account_drafts.remove(&account).unwrap_or_default();
        }
        if self.room_account.as_deref() != Some(account.as_str()) {
            let previous = self.room_account.take().unwrap_or_default();
            self.account_rooms.insert(previous, std::mem::take(&mut self.rooms));
            self.rooms = self.account_rooms.remove(&account).unwrap_or_default();
        }
        self.draft_account = Some(account.clone());
        self.room_account = Some(account);
        drafts_changed
    }
}

fn bare_account(jid: &str) -> String
{
    jid.split('/').next().unwrap_or(jid).to_string()
}

// A backend that was unavailable at startup must be retried later. Cache
// successful registration only, and serialize it across credential operations.
fn initialize_keyring(state: &Mutex<bool>, register: impl FnOnce() -> Result<(), String>) -> Result<(), String>
{
    let mut initialized = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !*initialized {
        register()?;
        *initialized = true;
    }
    Ok(())
}

#[cfg(not(test))]
fn init_keyring()
{
    static INITIALIZED: Mutex<bool> = Mutex::new(false);
    if let Err(error) = initialize_keyring(&INITIALIZED, register_default_store) {
        warn!("Failed to register keyring backend; saved passwords will not work: {}", error);
    }
}

#[cfg(all(not(test), target_os = "macos"))]
fn register_default_store() -> Result<(), String>
{
    let store = apple_native_keyring_store::keychain::Store::new()
        .map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    return Ok(());
}

#[cfg(all(not(test), target_os = "windows"))]
fn register_default_store() -> Result<(), String>
{
    let store = windows_native_keyring_store::Store::new()
        .map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    return Ok(());
}

#[cfg(all(not(test), target_os = "linux"))]
fn register_default_store() -> Result<(), String>
{
    let store = zbus_secret_service_keyring_store::Store::new()
        .map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    return Ok(());
}

#[cfg(all(not(test), not(any(target_os = "macos", target_os = "windows", target_os = "linux"))))]
fn register_default_store() -> Result<(), String>
{
    return Err("no keyring backend for this platform".to_string());
}

// Tests that exercise conversation switches and sends also save drafts. Give
// every test thread its own temporary config so none can touch the user's data.
#[cfg(test)]
struct TestConfigDir(PathBuf);

#[cfg(test)]
impl Drop for TestConfigDir
{
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

#[cfg(test)]
std::thread_local!
{
    static TEST_CONFIG_DIR: TestConfigDir = TestConfigDir(std::env::temp_dir().join(format!(
        "snack-tests-{}-{:?}-{}", std::process::id(), std::thread::current().id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos(),
    )));
}

#[cfg(test)]
pub(crate) fn config_path() -> Option<PathBuf>
{
    Some(TEST_CONFIG_DIR.with(|dir| dir.0.join(CONFIG_FILE)))
}

#[cfg(not(test))]
pub(crate) fn config_path() -> Option<PathBuf>
{
    return ProjectDirs::from("org", "snack", "snack")
        .map(|d| d.config_dir().join(CONFIG_FILE));
}

// Only call this for dedicated application directories, never a generic
// parent such as /tmp. Existing installations may have inherited a loose umask.
pub(crate) fn private_directory(path: &std::path::Path) -> io::Result<()>
{
    create_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(super) fn create_directory(path: &std::path::Path) -> io::Result<()>
{
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

// The persistent chat-log database lives alongside other app data.
pub fn history_path() -> Option<PathBuf>
{
    #[cfg(test)]
    { return config_path().map(|path| path.with_file_name(HISTORY_FILE)); }
    #[cfg(not(test))]
    { return ProjectDirs::from("org", "snack", "snack").map(|d| d.data_dir().join(HISTORY_FILE)); }
}

// Open (creating if needed) the persistent chat-log store.
pub fn open_history(account: &str, legacy_owner: Option<&str>) -> Option<history::History>
{
    let legacy = history_path()?;
    let open = || -> Result<history::History, String> {
        if let Some(parent) = legacy.parent() { private_directory(parent).map_err(|e| e.to_string())?; }
        let h = history::History::open(&legacy)?;
        if let Some(owner) = legacy_owner { h.claim_account(owner.split('/').next().unwrap_or(owner))?; }
        if h.claim_account(account)? { return Ok(h); }
        let h = history::History::open(&account_history_path(&legacy, account))?;
        if !h.claim_account(account)? { return Err("history belongs to a different account".into()); }
        Ok(h)
    };
    match open() {
        Ok(h) => Some(h),
        Err(e) => { warn!("Failed to open chat history: {}", e); None }
    }
}

fn account_history_path(legacy: &std::path::Path, account: &str) -> PathBuf
{
    use sha2::{Digest, Sha256};
    // Preserve existing short-account paths. Long JIDs can exceed the filesystem
    // component limit when hex encoded; leave room for SQLite's journal suffixes.
    let name: String = account.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let filename = format!("history-{name}.sqlite");
    if filename.len() <= 247
    {
        return legacy.with_file_name(filename);
    }
    // An older version may already have created a near-limit database. Continue
    // using it so an upgrade never silently hides existing messages or outbox.
    if filename.len() <= 255
    {
        let previous = legacy.with_file_name(&filename);
        if previous.exists() { return previous; }
    }
    let digest: String = Sha256::digest(account.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    return legacy.with_file_name(format!("history-sha256-{digest}.sqlite"));
}

pub fn load() -> SavedConfig
{
    let result = (|| -> Result<SavedConfig, String> {
        let path = config_path().ok_or("Could not resolve the configuration directory")?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() => return Err(format!("Settings {} are not a regular file", path.display())),
            Ok(_) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(SavedConfig::default()),
            Err(error) => return Err(format!("Could not inspect settings {}: {error}", path.display())),
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            path.parent().map(private_directory).transpose()
                .and_then(|_| fs::set_permissions(&path, fs::Permissions::from_mode(0o600)))
                .map_err(|error| format!("Could not protect settings {}: {error}", path.display()))?;
        }
        let contents = fs::read_to_string(&path).map_err(|error| format!("Could not read settings {}: {error}", path.display()))?;
        // Do not include draft contents from TOML source snippets in diagnostics.
        let mut cfg = toml::from_str::<SavedConfig>(&contents)
            .map_err(|error| format!("Could not parse settings {}: {}", path.display(), error.message()))?;
        cfg.migrate_legacy_ownership();
        Ok(cfg)
    })();
    match result {
        Ok(cfg) => cfg,
        Err(error) => {
            warn!("{}", error);
            SavedConfig { load_error: Some(error), ..SavedConfig::default() }
        }
    }
}

// Explicit user recovery: preserve original bytes (even invalid UTF-8) before
// installing writable defaults. A failed backup leaves the source untouched.
pub fn recover_configuration(cfg: &SavedConfig) -> Result<(SavedConfig, Option<PathBuf>), String>
{
    if cfg.load_error.is_none() { return Err("Settings do not need recovery".into()); }
    let path = config_path().ok_or("Could not resolve the configuration directory")?;
    let parent = path.parent().ok_or("Settings have no parent directory")?;
    let original = match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() => return Err("Settings are not a regular file; repair the path and retry loading".into()),
        Ok(_) => Some(fs::read(&path).map_err(|error| format!("Could not read settings for backup; original was kept: {error}"))?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("Could not inspect original settings: {error}")),
    };
    let backup = if let Some(bytes) = original {
        private_directory(parent).map_err(|error| error.to_string())?;
        let backup = parent.join(format!("config-recovery-{}.toml", uuid::Uuid::new_v4()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&backup).map_err(|error| format!("Could not create backup; original was kept: {error}"))?;
        let written = file.write_all(&bytes).and_then(|_| file.sync_all());
        drop(file);
        if let Err(error) = written {
            let _ = fs::remove_file(&backup);
            return Err(format!("Could not finish backup; original was kept: {error}"));
        }
        #[cfg(unix)]
        fs::File::open(parent).and_then(|directory| directory.sync_all())
            .map_err(|error| format!("Could not sync backup {}; original was kept: {error}", backup.display()))?;
        if fs::read(&path).map_err(|error| format!("Could not recheck settings; backup is {}: {error}", backup.display()))? != bytes {
            return Err(format!("Settings changed during recovery; original was kept and backup is {}. Retry loading settings", backup.display()));
        }
        Some(backup)
    } else { None };
    let fresh = SavedConfig::default();
    save(&fresh).map_err(|error| format!("Could not install new settings; backup is {}: {error}",
        backup.as_deref().map(|path| path.display().to_string()).unwrap_or_else(|| "not needed (original file missing)".into())))?;
    Ok((fresh, backup))
}

pub fn save(cfg: &SavedConfig) -> Result<(), String>
{
    if cfg.load_error.is_some() { return Err("Settings were not loaded; recover them before saving. Original settings were kept.".into()); }
    let path = config_path().ok_or_else(|| "no config directory".to_string())?;

    let serialized = toml::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    save_config_file(&path, serialized.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(all(test, unix))]
std::thread_local! {
    static FAIL_NEXT_DIRECTORY_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(all(test, unix))]
pub(crate) fn fail_next_directory_sync() {
    FAIL_NEXT_DIRECTORY_SYNC.with(|fail| fail.set(true));
}

#[cfg(unix)]
fn sync_config_directory(parent: &std::path::Path) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_DIRECTORY_SYNC.with(|fail| fail.replace(false)) {
        return Err(io::Error::other("injected settings directory sync failure"));
    }
    fs::File::open(parent)?.sync_all()
}

// Replace the config only after a complete write has reached disk. In-place
// truncation could erase saved accounts and drafts on a crash or write error.
pub(crate) fn save_config_file(path: &std::path::Path, bytes: &[u8]) -> io::Result<()>
{
    static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "config has no parent"))?;
    private_directory(parent)?;
    let (mut file, temporary) = loop
    {
        let seq = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary = parent.join(format!(".config-{}-{}.tmp", std::process::id(), seq));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary)
        {
            Ok(file) => break (file, temporary),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    struct TemporaryFile(PathBuf);
    impl Drop for TemporaryFile { fn drop(&mut self) { let _ = fs::remove_file(&self.0); } }
    let temporary = TemporaryFile(temporary);
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary.0, path)?;
    // A complete replacement is visible now, but saving is still uncertain
    // if its directory entry cannot be synced. Surface that error so callers
    // retain the draft/cleanup intent and can retry rather than closing.
    #[cfg(unix)]
    sync_config_directory(parent)?;
    Ok(())
}

#[cfg(not(test))]
fn password_entry(jid: &str) -> keyring_core::Result<Entry>
{
    init_keyring();
    Entry::new(KEYRING_SERVICE, jid)
}

// Each test thread has an isolated mock store, including tests that exercise
// the real login/removal handlers. Never register or access the OS keychain.
#[cfg(test)]
std::thread_local! {
    static TEST_PASSWORDS: std::cell::RefCell<HashMap<String, std::sync::Arc<keyring_core::mock::Cred>>> = Default::default();
}

#[cfg(test)]
fn password_entry(jid: &str) -> keyring_core::Result<Entry>
{
    TEST_PASSWORDS.with(|entries| {
        let credential = entries.borrow_mut().entry(jid.to_string()).or_insert_with(|| std::sync::Arc::new(keyring_core::mock::Cred {
            specifiers: (KEYRING_SERVICE.into(), jid.into()), inner: Default::default(),
        })).clone();
        Ok(Entry::new_with_credential(credential))
    })
}

#[cfg(test)]
pub(crate) fn fail_next_password_operation(jid: &str, error: keyring_core::Error)
{
    let entry = password_entry(jid).unwrap();
    entry.as_any().downcast_ref::<keyring_core::mock::Cred>().unwrap().set_error(error);
}

fn load_keyring_password(jid: &str) -> Result<Option<String>, String>
{
    match password_entry(jid).and_then(|entry| entry.get_password()) {
        Ok(password) => Ok(Some(password)),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(not(all(target_os = "macos", not(test))))]
fn save_keyring_password(jid: &str, password: &str) -> Result<(), String>
{
    let entry = password_entry(jid).map_err(|e| e.to_string())?;
    return entry.set_password(password).map_err(|e| e.to_string());
}

fn delete_keyring_password(jid: &str) -> Result<(), String>
{
    let entry = password_entry(jid).map_err(|e| e.to_string())?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

// On macOS, saved passwords are sealed to a Secure Enclave key and opened with
// Touch ID. The login keychain trusts an unsigned build only by its exact hash,
// so it would ask for the keychain password after every rebuild.
#[cfg(all(target_os = "macos", not(test)))]
fn sealed_password_path(jid: &str) -> Result<PathBuf, String>
{
    use sha2::{Digest, Sha256};
    let dir = config_path().and_then(|path| path.parent().map(|parent| parent.join("credentials")))
        .ok_or("Cannot locate Snack's configuration directory")?;
    let digest = Sha256::digest(bare_account(jid).as_bytes());
    let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(dir.join(format!("{name}.sealed")))
}

#[cfg(all(target_os = "macos", not(test)))]
pub fn load_password(jid: &str) -> Result<Option<String>, String>
{
    let path = sealed_password_path(jid)?;
    match fs::read(&path) {
        Ok(sealed) => {
            log::debug!("Opening the saved password for {jid} with Touch ID");
            return match sealed_password::open(&sealed, &format!("unlock the saved password for {}", bare_account(jid))) {
                Ok(password) => Ok(Some(password)),
                Err(sealed_password::OpenError::Cancelled) => Err("Unlocking the saved password was cancelled.".into()),
                Err(sealed_password::OpenError::Failed(error)) => Err(format!("Could not unlock the saved password: {error}")),
            };
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Could not read the saved password: {error}")),
    }
    // Move a password saved in the keychain by an earlier version. The old
    // item is left in place; forgetting the login removes it.
    let Some(password) = load_keyring_password(jid)? else { return Ok(None); };
    match sealed_password::seal(&password).and_then(|sealed| save_config_file(&path, &sealed).map_err(|e| e.to_string())) {
        Ok(()) => log::info!("Moved the saved password for {jid} from the keychain to Touch ID protection"),
        Err(error) => log::warn!("Keeping the saved password in the keychain: {error}"),
    }
    Ok(Some(password))
}

#[cfg(all(target_os = "macos", not(test)))]
pub fn save_password(jid: &str, password: &str) -> Result<(), String>
{
    let sealed = sealed_password::seal(password)?;
    save_config_file(&sealed_password_path(jid)?, &sealed).map_err(|e| format!("Could not save the password: {e}"))
}

#[cfg(all(target_os = "macos", not(test)))]
pub fn delete_password(jid: &str) -> Result<(), String>
{
    match fs::remove_file(sealed_password_path(jid)?) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Could not delete the saved password: {error}")),
    }
    delete_keyring_password(jid)
}

#[cfg(not(all(target_os = "macos", not(test))))]
pub fn load_password(jid: &str) -> Result<Option<String>, String> { load_keyring_password(jid) }

#[cfg(not(all(target_os = "macos", not(test))))]
pub fn save_password(jid: &str, password: &str) -> Result<(), String> { save_keyring_password(jid, password) }

#[cfg(not(all(target_os = "macos", not(test))))]
pub fn delete_password(jid: &str) -> Result<(), String> { delete_keyring_password(jid) }

// Persist the user's intent before touching the credential. A failed cleanup
// must not leave a remembered login or disappear on application restart.
pub fn remove_auto_login(cfg: &mut SavedConfig) -> Result<(), String>
{
    cfg.migrate_legacy_ownership();
    if let Some(jid) = cfg.jid.take() {
        if !cfg.pending_credential_removals.contains(&jid) { cfg.pending_credential_removals.push(jid); }
    }
    retry_credential_removals(cfg)
}

pub fn retry_credential_removals(cfg: &mut SavedConfig) -> Result<(), String>
{
    if cfg.jid.as_ref().is_some_and(|jid| cfg.pending_credential_removals.contains(jid)) { cfg.jid = None; }
    save(cfg).map_err(|error| format!("Could not save disabled auto-login; it may still be enabled after restart. No password cleanup was attempted. {error}"))?;
    let mut errors = Vec::new();
    let original = cfg.pending_credential_removals.clone();
    let before = original.len();
    cfg.pending_credential_removals.retain(|jid| match delete_password(jid) {
        Ok(()) => false,
        Err(error) => { errors.push(format!("{jid}: {error}")); true },
    });
    if cfg.pending_credential_removals.len() != before {
        if let Err(error) = save(cfg) {
            cfg.pending_credential_removals = original;
            errors.push(format!("Could not save password cleanup progress: {error}"));
        }
    }
    if errors.is_empty() { Ok(()) }
    else { Err(format!("{}Password cleanup is incomplete. Unlock your keyring and retry. {}",
        if cfg.jid.is_none() { "Auto-login is disabled. " } else { "" }, errors.join("; "))) }
}

pub fn remember_login(cfg: &mut SavedConfig, jid: &str, password: Option<&str>) -> Result<(), String>
{
    // Finish old cleanup and persist its cancellation before saving a new
    // password. Failed persistence leaves the old intent available to retry.
    if cfg.pending_credential_removals.iter().any(|pending| pending == jid) {
        delete_password(jid).map_err(|error| format!("Could not finish previous password removal; saved login was not changed. {error}"))?;
    }
    let mut prepared = cfg.clone();
    prepared.pending_credential_removals.retain(|pending| pending != jid);
    prepared.jid = None;
    save(&prepared).map_err(|error| format!("Could not prepare saved login: {error}"))?;
    *cfg = prepared;
    if let Some(password) = password {
        if let Err(error) = save_password(jid, password) {
            // A failed update can leave the previous secret (or an uncertain
            // new write) behind. Keep that entry tracked for explicit cleanup.
            cfg.pending_credential_removals.push(jid.to_string());
            let progress = save(cfg).err().map(|error| format!(" Could not save cleanup intent: {error}")).unwrap_or_default();
            return Err(format!("Could not save password; auto-login remains disabled. A password may still be stored; retry password cleanup. {error}{progress}"));
        }
    }
    cfg.jid = Some(jid.to_string());
    save(cfg).map_err(|error| format!("Could not save auto-login settings: {error}"))
}

#[cfg(test)]
mod tests
{
    use super::*;

    #[cfg(unix)]
    #[test]
    fn uncertain_directory_sync_does_not_start_password_cleanup()
    {
        let jid = "directory-sync@example";
        let mut cfg = SavedConfig { jid: Some(jid.into()), ..Default::default() };
        save_password(jid, "keep until removal intent is saved").unwrap();
        save(&cfg).unwrap();
        fail_next_directory_sync();
        let error = remove_auto_login(&mut cfg).unwrap_err();
        assert!(error.contains("injected settings directory sync failure"));
        assert!(cfg.jid.is_none());
        assert_eq!(cfg.pending_credential_removals, [jid]);
        assert_eq!(load_password(jid).unwrap().as_deref(), Some("keep until removal intent is saved"));
        retry_credential_removals(&mut cfg).unwrap();
        assert!(cfg.pending_credential_removals.is_empty());
        assert!(load_password(jid).unwrap().is_none());
        let reopened = load();
        assert!(reopened.jid.is_none());
        assert!(reopened.pending_credential_removals.is_empty());
    }

    #[test]
    fn unreadable_settings_are_not_replaced_by_default_configuration()
    {
        let path = config_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = b"jid = 'me@example'\n[drafts]\nroom = 'unfinished";
        fs::write(&path, original).unwrap();
        let cfg = load();
        assert!(save(&cfg).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn explicit_settings_recovery_preserves_exact_original_bytes()
    {
        let path = config_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for original in [b"[drafts]\nroom = 'unfinished".as_slice(), b"invalid UTF8: \xff".as_slice()] {
            fs::write(&path, original).unwrap();
            let cfg = load();
            assert!(cfg.load_error.is_some());
            let (fresh, backup) = recover_configuration(&cfg).unwrap();
            let backup = backup.unwrap();
            assert_eq!(fs::read(&backup).unwrap(), original);
            assert!(fresh.load_error.is_none());
            assert!(load().load_error.is_none());
            assert!(load().drafts.is_empty());
            #[cfg(unix)] {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(fs::metadata(backup).unwrap().permissions().mode() & 0o777, 0o600);
            }
            assert!(recover_configuration(&fresh).is_err());
        }
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("config-recovery-")).count(), 2);
    }

    #[test]
    fn settings_recovery_refuses_non_files_without_replacing_them()
    {
        let path = config_path().unwrap();
        fs::create_dir_all(&path).unwrap();
        let cfg = load();
        assert!(cfg.load_error.is_some());
        assert!(save(&cfg).is_err());
        assert!(recover_configuration(&cfg).is_err());
        assert!(path.is_dir());
        fs::remove_dir(&path).unwrap();
        assert!(load().load_error.is_none());
        let (fresh, backup) = recover_configuration(&cfg).unwrap();
        assert!(fresh.load_error.is_none());
        assert!(backup.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn settings_load_and_recovery_do_not_follow_a_symlink_or_chmod_its_target()
    {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let path = config_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = path.with_file_name("unrelated-file");
        fs::write(&target, b"unrelated content").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&target, &path).unwrap();
        let cfg = load();
        assert!(cfg.load_error.is_some());
        assert!(save(&cfg).is_err());
        assert!(recover_configuration(&cfg).is_err());
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"unrelated content");
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);
    }

    #[test]
    fn keyring_initialization_recovers_after_an_unavailable_backend()
    {
        let state = Mutex::new(false);
        let mut attempts = 0;
        assert!(initialize_keyring(&state, || { attempts += 1; Err("backend unavailable".into()) }).is_err());
        initialize_keyring(&state, || { attempts += 1; Ok(()) }).unwrap();
        initialize_keyring(&state, || { attempts += 1; panic!("registered backend must be reused") }).unwrap();
        assert_eq!(attempts, 2);
    }

    #[test]
    fn saved_password_loading_distinguishes_absence_from_keyring_failure()
    {
        let jid = "locked-read@example";
        assert!(load_password(jid).unwrap().is_none());
        save_password(jid, "test secret").unwrap();
        fail_next_password_operation(jid, keyring_core::Error::NoStorageAccess(Box::new(io::Error::other("locked"))));
        assert!(load_password(jid).is_err());
        assert_eq!(load_password(jid).unwrap().as_deref(), Some("test secret"));
    }

    #[test]
    fn password_removal_does_not_hide_keyring_failures()
    {
        let jid = "removal@example";
        assert!(delete_password(jid).is_ok());
        save_password(jid, "test secret").unwrap();
        fail_next_password_operation(jid, keyring_core::Error::NoStorageAccess(Box::new(io::Error::new(io::ErrorKind::PermissionDenied, "locked"))));
        assert!(delete_password(jid).is_err());
        assert_eq!(load_password(jid).unwrap().as_deref(), Some("test secret"));
        delete_password(jid).unwrap();
        assert!(load_password(jid).unwrap().is_none());
    }

    #[test]
    fn failed_credential_cleanup_is_persistent_and_successful_retry_keeps_drafts()
    {
        let jid = "cleanup@example";
        let mut cfg = SavedConfig::default();
        cfg.jid = Some(jid.into());
        cfg.drafts.insert("room@example".into(), "unsent".into());
        save_password(jid, "secret").unwrap();
        fail_next_password_operation(jid, keyring_core::Error::PlatformFailure(Box::new(io::Error::other("unavailable"))));
        assert!(remove_auto_login(&mut cfg).is_err());
        let mut reopened = load();
        assert!(reopened.jid.is_none());
        assert_eq!(reopened.pending_credential_removals, [jid]);
        assert_eq!(reopened.draft_account.as_deref(), Some(jid));
        assert_eq!(reopened.drafts["room@example"], "unsent");
        assert_eq!(load_password(jid).unwrap().as_deref(), Some("secret"));
        retry_credential_removals(&mut reopened).unwrap();
        assert!(load_password(jid).unwrap().is_none());
        assert!(load().pending_credential_removals.is_empty());
        assert_eq!(load().drafts["room@example"], "unsent");
    }

    #[test]
    fn partial_cleanup_keeps_other_saved_login_and_retries_only_failed_entries()
    {
        let mut cfg = SavedConfig::default();
        cfg.jid = Some("current@example".into());
        cfg.pending_credential_removals = vec!["locked@example".into(), "ready@example".into()];
        for jid in ["current@example", "locked@example", "ready@example"] { save_password(jid, "secret").unwrap(); }
        fail_next_password_operation("locked@example", keyring_core::Error::NoStorageAccess(Box::new(io::Error::other("locked"))));
        assert!(retry_credential_removals(&mut cfg).is_err());
        assert_eq!(cfg.jid.as_deref(), Some("current@example"));
        assert_eq!(load().pending_credential_removals, ["locked@example"]);
        assert!(load_password("ready@example").unwrap().is_none());
        retry_credential_removals(&mut cfg).unwrap();
        assert_eq!(load().jid.as_deref(), Some("current@example"));
        assert_eq!(load_password("current@example").unwrap().as_deref(), Some("secret"));
        assert!(load_password("locked@example").unwrap().is_none());
    }

    #[test]
    fn failed_config_write_does_not_delete_or_replace_a_password()
    {
        let jid = "config-failure@example";
        let mut cfg = SavedConfig::default();
        cfg.jid = Some(jid.into());
        save_password(jid, "original").unwrap();
        let path = config_path().unwrap();
        fs::create_dir_all(&path).unwrap(); // A directory prevents atomic replacement.
        assert!(remove_auto_login(&mut cfg).is_err());
        assert_eq!(load_password(jid).unwrap().as_deref(), Some("original"));
        assert_eq!(cfg.pending_credential_removals, [jid]);
        // Finishing the old removal is safe, but a failed config write must
        // prevent saving the replacement password under a persisted tombstone.
        assert!(remember_login(&mut cfg, jid, Some("replacement")).is_err());
        assert!(load_password(jid).unwrap().is_none());
        assert_eq!(cfg.pending_credential_removals, [jid]);
        fs::remove_dir(&path).unwrap();
        retry_credential_removals(&mut cfg).unwrap();
        assert!(cfg.pending_credential_removals.is_empty());
    }

    #[test]
    fn remembering_again_finishes_old_cleanup_before_saving_a_new_password()
    {
        let jid = "replacement@example";
        let mut cfg = SavedConfig::default();
        cfg.pending_credential_removals.push(jid.into());
        save(&cfg).unwrap();
        save_password(jid, "old").unwrap();
        fail_next_password_operation(jid, keyring_core::Error::NoStorageAccess(Box::new(io::Error::other("locked"))));
        assert!(remember_login(&mut cfg, jid, Some("new")).is_err());
        assert_eq!(load_password(jid).unwrap().as_deref(), Some("old"));
        assert_eq!(load().pending_credential_removals, [jid]);
        remember_login(&mut cfg, jid, Some("new")).unwrap();
        let mut reopened = load();
        assert_eq!(reopened.jid.as_deref(), Some(jid));
        assert!(reopened.pending_credential_removals.is_empty());
        retry_credential_removals(&mut reopened).unwrap();
        assert_eq!(load_password(jid).unwrap().as_deref(), Some("new"));
    }

    #[test]
    fn failed_password_save_does_not_enable_auto_login_with_an_old_secret()
    {
        let jid = "save-failure@example";
        let mut cfg = SavedConfig::default();
        cfg.jid = Some(jid.into());
        save(&cfg).unwrap();
        save_password(jid, "old").unwrap();
        fail_next_password_operation(jid, keyring_core::Error::NoStorageAccess(Box::new(io::Error::other("locked"))));
        assert!(remember_login(&mut cfg, jid, Some("new")).is_err());
        assert!(cfg.jid.is_none());
        assert!(load().jid.is_none());
        assert_eq!(load_password(jid).unwrap().as_deref(), Some("old"));
        assert_eq!(load().pending_credential_removals, [jid]);
        retry_credential_removals(&mut cfg).unwrap();
        assert!(load_password(jid).unwrap().is_none());
    }

    #[test]
    fn long_account_names_have_bounded_distinct_history_paths()
    {
        let legacy = config_path().unwrap().with_file_name(HISTORY_FILE);
        let first = format!("{}@example.invalid", "a".repeat(1000));
        let second = format!("{}@example.invalid", "b".repeat(1000));
        let path = account_history_path(&legacy, &first);
        assert_eq!(path, account_history_path(&legacy, &first));
        assert_ne!(path, account_history_path(&legacy, &second));
        assert!(path.file_name().unwrap().len() + "-journal".len() <= 255);
        let h = history::History::open(&path).unwrap();
        assert!(h.claim_account(&first).unwrap());
        assert!(!h.claim_account(&second).unwrap());
        drop(h);
        assert!(history::History::open(&path).unwrap().claim_account(&first).unwrap());
        assert_eq!(account_history_path(&legacy, "a@b"), legacy.with_file_name("history-614062.sqlite"));
    }

    #[test]
    fn preserves_existing_near_limit_history_filename()
    {
        let legacy = config_path().unwrap().with_file_name(HISTORY_FILE);
        let account = format!("{}@b", "a".repeat(116));
        let name: String = account.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let previous = legacy.with_file_name(format!("history-{name}.sqlite"));
        assert_ne!(account_history_path(&legacy, &account), previous);
        fs::create_dir_all(previous.parent().unwrap()).unwrap();
        fs::write(&previous, b"existing database").unwrap();
        assert_eq!(account_history_path(&legacy, &account), previous);
    }

    #[cfg(unix)]
    #[test]
    fn existing_configuration_and_app_directory_permissions_are_tightened()
    {
        use std::os::unix::fs::PermissionsExt;
        let path = config_path().unwrap();
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&path, "[drafts]\nalice = 'private draft'\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let config = load();
        assert_eq!(config.drafts["alice"], "private draft");
        assert_eq!(fs::metadata(parent).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        save(&config).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn configuration_round_trip_uses_an_isolated_directory()
    {
        let path = config_path().unwrap();
        assert!(path.starts_with(std::env::temp_dir()));
        let mut config = SavedConfig::default();
        config.jid = Some("test@example.invalid".into());
        config.draft_account = config.jid.clone();
        config.drafts.insert("room@example.invalid".into(), "unsent draft".into());
        save(&config).unwrap();
        let restored = load();
        assert_eq!(restored.jid, config.jid);
        assert_eq!(restored.draft_account, config.draft_account);
        assert_eq!(restored.drafts, config.drafts);
    }

    #[test]
    fn account_switches_preserve_rooms_and_drafts_across_reopen_and_resource_changes()
    {
        let mut config = SavedConfig::default();
        config.jid = Some("first@example/old-device".into());
        config.rooms.push("first-room@example".into());
        config.drafts.insert("shared@example".into(), "first draft".into());
        save(&config).unwrap();
        let mut config = load();
        assert_eq!(config.room_account.as_deref(), Some("first@example"));
        assert_eq!(config.draft_account.as_deref(), Some("first@example"));
        config.jid = None; // Forgetting credentials must not forget data ownership.
        assert!(config.activate_account("second@example/device"));
        assert!(config.rooms.is_empty());
        assert!(config.drafts.is_empty());
        config.rooms.push("second-room@example".into());
        config.drafts.insert("shared@example".into(), "second draft".into());
        save(&config).unwrap();
        let mut config = load();
        assert!(config.activate_account("first@example/new-device"));
        assert_eq!(config.rooms, vec!["first-room@example"]);
        assert_eq!(config.drafts["shared@example"], "first draft");
        assert!(!config.activate_account("first@example/another-device"));
        assert!(config.activate_account("second@example"));
        assert_eq!(config.rooms, vec!["second-room@example"]);
        assert_eq!(config.drafts["shared@example"], "second draft");
    }

    #[test]
    fn ownerless_legacy_data_is_preserved_but_never_assigned_to_a_new_login()
    {
        let mut config = SavedConfig::default();
        config.rooms.push("unknown-room@example".into());
        config.drafts.insert("shared@example".into(), "unknown draft".into());
        assert!(config.activate_account("first@example"));
        assert!(config.rooms.is_empty());
        assert!(config.drafts.is_empty());
        save(&config).unwrap();
        let mut config = load();
        assert!(config.activate_account("second@example"));
        assert!(config.rooms.is_empty());
        assert!(config.drafts.is_empty());
        assert_eq!(config.account_rooms[""], vec!["unknown-room@example"]);
        assert_eq!(config.account_drafts[""]["shared@example"], "unknown draft");
    }

    #[test]
    fn old_configurations_default_the_new_persistence_fields()
    {
        let config: SavedConfig = toml::from_str("jid = 'test@example.invalid'\nrooms = ['room@example.invalid']\n").unwrap();
        assert!(config.drafts.is_empty());
        assert!(config.account_drafts.is_empty());
        assert!(config.draft_account.is_none());
    }
    #[test]
    fn readers_never_observe_a_partially_replaced_configuration()
    {
        use std::sync::{Arc, Barrier, atomic::{AtomicBool, Ordering}};
        let path = config_path().unwrap();
        let mut config = SavedConfig::default();
        config.drafts.insert("room@example.invalid".into(), "a".repeat(32_000));
        save(&config).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(Barrier::new(2));
        let reader_stop = stop.clone();
        let reader_ready = ready.clone();
        let reader = std::thread::spawn(move || {
            let mut reads = 0;
            reader_ready.wait();
            loop {
                let contents = fs::read_to_string(&path).unwrap();
                let parsed: SavedConfig = toml::from_str(&contents).unwrap();
                let draft = &parsed.drafts["room@example.invalid"];
                assert_eq!(draft.len(), 32_000);
                assert!(draft.chars().all(|ch| ch == 'a') || draft.chars().all(|ch| ch == 'b'));
                reads += 1;
                if reader_stop.load(Ordering::Acquire) { return reads; }
            }
        });
        ready.wait();
        for i in 0..20 {
            config.drafts.insert("room@example.invalid".into(), if i % 2 == 0 { "b" } else { "a" }.repeat(32_000));
            save(&config).unwrap();
        }
        stop.store(true, Ordering::Release);
        assert!(reader.join().unwrap() > 0);
    }

    #[test]
    fn failed_config_replacement_removes_temporary_files()
    {
        let path = config_path().unwrap();
        fs::create_dir_all(&path).unwrap();
        assert!(save_config_file(&path, b"complete").is_err());
        assert!(path.is_dir());
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn saved_drafts_are_not_readable_by_other_users()
    {
        use std::os::unix::fs::PermissionsExt;
        save(&SavedConfig::default()).unwrap();
        assert_eq!(fs::metadata(config_path().unwrap()).unwrap().permissions().mode() & 0o777, 0o600);
    }

}
