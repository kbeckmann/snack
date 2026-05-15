//! On-disk OMEMO state. Stores per-account identity, pre-keys, sessions,
//! trust state and last-seen device lists under the user's data dir.

use serde::{ Deserialize, Serialize };
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{ Path, PathBuf };

use super::identity::{
    IdentityKeyPair, OneTimePreKey, SignedPreKey,
    StoredIdentity, StoredOneTimePreKey, StoredSignedPreKey,
};
use super::session::Session;

#[derive(Debug, thiserror::Error)]
pub enum StoreError
{
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Per-(jid, device_id) session keyed string in stored maps.
pub fn session_key(jid: &str, device_id: u32) -> String
{
    return format!("{}|{}", jid, device_id);
}

/// Trust state for a peer identity key.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Trust
{
    /// Trust-on-first-use: the first time we see this device we accept it.
    Tofu,
    /// User has verified the fingerprint out-of-band.
    Verified,
    /// User has explicitly rejected this device.
    Rejected,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct StoredOmemo
{
    pub identity: Option<StoredIdentity>,
    pub signed_pre_key: Option<StoredSignedPreKey>,
    pub one_time_pre_keys: Vec<StoredOneTimePreKey>,
    /// Map keyed by `session_key(jid, device_id)` -> Session.
    pub sessions: HashMap<String, Session>,
    /// Trust of remote identity public keys, keyed by `session_key()`.
    pub trust: HashMap<String, (Trust, [u8; 32])>,
    /// Last device list we received per peer JID.
    pub peer_device_lists: HashMap<String, Vec<u32>>,
    /// Bundles we have published recently. Each entry caches the local
    /// device's signed pre-key id so we know whether to rotate.
    pub published_signed_pre_key_id: Option<u32>,
}

/// Wrapper around `StoredOmemo` that knows where to persist itself.
pub struct OmemoStore
{
    path: PathBuf,
    data: StoredOmemo,
}

impl OmemoStore
{
    /// Open (or create) the OMEMO state file for the given JID.
    pub fn open(base_data_dir: &Path, jid: &str) -> Result<Self, StoreError>
    {
        let mut path = base_data_dir.to_path_buf();
        path.push("omemo");
        if !path.exists()
        {
            fs::create_dir_all(&path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            }
        }
        path.push(format!("{}.json", sanitise(jid)));

        let data = if path.exists()
        {
            let bytes = fs::read(&path)?;
            serde_json::from_slice::<StoredOmemo>(&bytes).unwrap_or_default()
        }
        else
        {
            StoredOmemo::default()
        };

        return Ok(Self { path, data });
    }

    pub fn save(&self) -> Result<(), StoreError>
    {
        let bytes = serde_json::to_vec_pretty(&self.data)?;
        fs::write(&self.path, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        }
        return Ok(());
    }

    pub fn identity(&self) -> Option<IdentityKeyPair>
    {
        return self.data.identity.clone().map(Into::into);
    }

    pub fn set_identity(&mut self, identity: &IdentityKeyPair)
    {
        self.data.identity = Some(identity.into());
    }

    pub fn signed_pre_key(&self) -> Option<SignedPreKey>
    {
        return self.data.signed_pre_key.clone().map(Into::into);
    }

    pub fn set_signed_pre_key(&mut self, spk: &SignedPreKey)
    {
        self.data.signed_pre_key = Some(spk.into());
        self.data.published_signed_pre_key_id = Some(spk.id);
    }

    pub fn one_time_pre_keys(&self) -> Vec<OneTimePreKey>
    {
        return self.data.one_time_pre_keys.iter().cloned().map(Into::into).collect();
    }

    pub fn set_one_time_pre_keys(&mut self, otpks: &[OneTimePreKey])
    {
        self.data.one_time_pre_keys = otpks.iter().map(|p| p.into()).collect();
    }

    pub fn take_one_time_pre_key(&mut self, id: u32) -> Option<OneTimePreKey>
    {
        let pos = self.data.one_time_pre_keys.iter().position(|p| p.id == id)?;
        let stored = self.data.one_time_pre_keys.remove(pos);
        return Some(stored.into());
    }

    pub fn session(&self, jid: &str, device_id: u32) -> Option<Session>
    {
        return self.data.sessions.get(&session_key(jid, device_id)).cloned();
    }

    pub fn put_session(&mut self, jid: &str, device_id: u32, session: Session)
    {
        self.data.sessions.insert(session_key(jid, device_id), session);
    }

    pub fn remove_session(&mut self, jid: &str, device_id: u32)
    {
        self.data.sessions.remove(&session_key(jid, device_id));
    }

    pub fn trust(&self, jid: &str, device_id: u32) -> Option<(Trust, [u8; 32])>
    {
        return self.data.trust.get(&session_key(jid, device_id)).copied();
    }

    pub fn set_trust(&mut self, jid: &str, device_id: u32, trust: Trust, identity_pub: [u8; 32])
    {
        self.data.trust.insert(session_key(jid, device_id), (trust, identity_pub));
    }

    pub fn peer_device_list(&self, jid: &str) -> Option<Vec<u32>>
    {
        return self.data.peer_device_lists.get(jid).cloned();
    }

    pub fn set_peer_device_list(&mut self, jid: &str, devices: Vec<u32>)
    {
        self.data.peer_device_lists.insert(jid.to_string(), devices);
    }
}

fn sanitise(jid: &str) -> String
{
    // Replace anything that isn't [a-zA-Z0-9.@_-] with '_'.
    return jid.chars().map(|c|
    {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '@' | '_' | '-') { c } else { '_' }
    }).collect();
}

#[cfg(test)]
mod tests
{
    use super::*;
    use crate::omemo::identity::IdentityKeyPair;

    #[test]
    fn open_and_persist()
    {
        let dir = tempdir();
        let mut store = OmemoStore::open(&dir, "alice@example.com").unwrap();

        let id = IdentityKeyPair::generate();
        store.set_identity(&id);
        store.save().unwrap();

        let store2 = OmemoStore::open(&dir, "alice@example.com").unwrap();
        let id2 = store2.identity().unwrap();
        assert_eq!(id.device_id, id2.device_id);
        assert_eq!(id.public_bytes(), id2.public_bytes());
    }

    #[test]
    fn trust_round_trip()
    {
        let dir = tempdir();
        let mut store = OmemoStore::open(&dir, "alice@example.com").unwrap();
        store.set_trust("bob@example.com", 9, Trust::Verified, [3u8; 32]);
        store.save().unwrap();

        let store2 = OmemoStore::open(&dir, "alice@example.com").unwrap();
        assert_eq!(store2.trust("bob@example.com", 9), Some((Trust::Verified, [3u8; 32])));
    }

    fn tempdir() -> PathBuf
    {
        let mut p = std::env::temp_dir();
        p.push(format!("snack-omemo-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        return p;
    }
}
