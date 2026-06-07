use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use directories::ProjectDirs;
use keyring_core::Entry;
use log::warn;
use serde::{ Deserialize, Serialize };

pub mod history;

const KEYRING_SERVICE: &str = "snack";
const CONFIG_FILE: &str = "config.toml";
const HISTORY_FILE: &str = "history.sqlite";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedConfig
{
    #[serde(default)]
    pub jid: Option<String>,
    #[serde(default)]
    pub rooms: Vec<String>,
    // Unsent message text, keyed by room/contact JID, so each conversation
    // keeps its own in-progress draft across room switches and restarts.
    #[serde(default)]
    pub drafts: HashMap<String, String>,
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

// The persistent chat-log database lives alongside other app data.
pub fn history_path() -> Option<PathBuf>
{
    return ProjectDirs::from("org", "snack", "snack")
        .map(|d| d.data_dir().join(HISTORY_FILE));
}

// Open (creating if needed) the persistent chat-log store.
pub fn open_history() -> Option<history::History>
{
    let path = history_path()?;
    return match history::History::open(&path)
    {
        Ok(h) => Some(h),
        Err(e) =>
        {
            warn!("Failed to open chat history DB at {}: {}", path.display(), e);
            None
        }
    };
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
