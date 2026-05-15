use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use directories::ProjectDirs;
use keyring_core::Entry;
use log::warn;
use serde::{ Deserialize, Serialize };

const KEYRING_SERVICE: &str = "snack";
const CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedConfig
{
    #[serde(default)]
    pub jid: Option<String>,
    #[serde(default)]
    pub rooms: Vec<String>,
}

pub fn init_keyring()
{
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(||
    {
        if let Err(e) = register_default_store()
        {
            warn!("Failed to register keyring backend; saved passwords will not work: {}", e);
        }
    });
}

#[cfg(target_os = "macos")]
fn register_default_store() -> Result<(), String>
{
    let store = apple_native_keyring_store::keychain::Store::new()
        .map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    return Ok(());
}

#[cfg(target_os = "windows")]
fn register_default_store() -> Result<(), String>
{
    let store = windows_native_keyring_store::Store::new()
        .map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    return Ok(());
}

#[cfg(target_os = "linux")]
fn register_default_store() -> Result<(), String>
{
    let store = zbus_secret_service_keyring_store::Store::new()
        .map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    return Ok(());
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn register_default_store() -> Result<(), String>
{
    return Err("no keyring backend for this platform".to_string());
}

fn config_path() -> Option<PathBuf>
{
    return ProjectDirs::from("org", "snack", "snack")
        .map(|d| d.config_dir().join(CONFIG_FILE));
}

/// Returns the application data directory used for non-secret state
/// (currently: OMEMO sessions and pre-keys).
pub fn data_dir() -> Option<PathBuf>
{
    return ProjectDirs::from("org", "snack", "snack")
        .map(|d| d.data_dir().to_path_buf());
}

pub fn load() -> SavedConfig
{
    let Some(path) = config_path() else
    {
        warn!("Could not resolve config directory");
        return SavedConfig::default();
    };

    let contents = match fs::read_to_string(&path)
    {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return SavedConfig::default(),
        Err(e) =>
        {
            warn!("Failed to read config {}: {}", path.display(), e);
            return SavedConfig::default();
        }
    };

    return match toml::from_str::<SavedConfig>(&contents)
    {
        Ok(cfg) => cfg,
        Err(e) =>
        {
            warn!("Failed to parse config {}: {}", path.display(), e);
            SavedConfig::default()
        }
    };
}

pub fn save(cfg: &SavedConfig) -> Result<(), String>
{
    let path = config_path().ok_or_else(|| "no config directory".to_string())?;

    if let Some(parent) = path.parent()
    {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let serialized = toml::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    fs::write(&path, serialized.as_bytes()).map_err(|e| e.to_string())?;

    return Ok(());
}

pub fn load_password(jid: &str) -> Option<String>
{
    let entry = Entry::new(KEYRING_SERVICE, jid).ok()?;
    return match entry.get_password()
    {
        Ok(pw) => Some(pw),
        Err(e) =>
        {
            log::debug!("No saved password for {}: {}", jid, e);
            None
        }
    };
}

pub fn save_password(jid: &str, password: &str) -> Result<(), String>
{
    let entry = Entry::new(KEYRING_SERVICE, jid).map_err(|e| e.to_string())?;
    return entry.set_password(password).map_err(|e| e.to_string());
}

pub fn delete_password(jid: &str) -> Result<(), String>
{
    let entry = Entry::new(KEYRING_SERVICE, jid).map_err(|e| e.to_string())?;
    return match entry.delete_credential()
    {
        Ok(()) => Ok(()),
        Err(e) =>
        {
            // Treat "not found" as success.
            log::debug!("delete_password({}): {}", jid, e);
            Ok(())
        }
    };
}
