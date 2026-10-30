//! Saved stream destinations: names and server URLs in
//! `stream-destinations.json` in the app data dir, stream keys in the OS
//! credential store ([`super::keystore`]). At most 10; deleting a
//! destination deletes its key. Keys never reach the webview: the UI lists
//! destinations and starts a stream by id.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::keystore::SecretStore;
use super::rtmp::{DestinationError, RtmpDestination};

pub const DESTINATIONS_FILE: &str = "stream-destinations.json";
pub const MAX_DESTINATIONS: usize = 10;
pub const DESTINATION_NAME_MAX_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedDestination {
    pub id: String,
    pub name: String,
    pub server_url: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DestinationsFile {
    #[serde(default)]
    destinations: Vec<SavedDestination>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DestinationsError {
    #[error(transparent)]
    Invalid(#[from] DestinationError),
    #[error("You can save up to {MAX_DESTINATIONS} destinations. Delete one first.")]
    Full,
    #[error("That destination no longer exists")]
    NotFound,
    #[error("The stream key for this destination is missing. Enter it again.")]
    MissingKey,
    #[error("{0}")]
    Store(String),
}

pub struct DestinationStore {
    path: PathBuf,
    secrets: Box<dyn SecretStore>,
}

fn new_id() -> String {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        bytes = nanos.to_le_bytes();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn entry_name(name: &str, destination: &RtmpDestination) -> String {
    match name.trim() {
        "" => default_name(destination.server_url()),
        name => name.chars().take(DESTINATION_NAME_MAX_CHARS).collect(),
    }
}

fn default_name(server_url: &str) -> String {
    server_url
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', ':']).next().unwrap_or(rest))
        .unwrap_or(server_url)
        .to_string()
}

impl DestinationStore {
    pub fn new(app_data_dir: &Path, secrets: Box<dyn SecretStore>) -> Self {
        Self { path: app_data_dir.join(DESTINATIONS_FILE), secrets }
    }

    fn read(&self) -> DestinationsFile {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn write(&self, file: &DestinationsFile) -> Result<(), DestinationsError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DestinationsError::Store(e.to_string()))?;
        }
        let json = serde_json::to_string_pretty(file)
            .map_err(|e| DestinationsError::Store(e.to_string()))?;
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, json).map_err(|e| DestinationsError::Store(e.to_string()))?;
        std::fs::rename(&temp, &self.path).map_err(|e| DestinationsError::Store(e.to_string()))
    }

    pub fn list(&self) -> Vec<SavedDestination> {
        self.read().destinations
    }

    /// Whether `save` would find room for this destination: it replaces an
    /// entry with the same server URL and name, otherwise needs a free slot.
    /// Checked before going live; the save itself waits for the server to
    /// accept the stream, so a wrong key is never stored.
    pub fn ensure_room(&self, name: &str, destination: &RtmpDestination) -> Result<(), DestinationsError> {
        let name = entry_name(name, destination);
        let file = self.read();
        let replaces = file
            .destinations
            .iter()
            .any(|saved| saved.server_url == destination.server_url() && saved.name == name);
        if !replaces && file.destinations.len() >= MAX_DESTINATIONS {
            return Err(DestinationsError::Full);
        }
        Ok(())
    }

    /// Save a destination, or replace the key of the one with the same
    /// server URL and name. The key is written first, then the file; if the
    /// file can't be written the key is rolled back (the previous one
    /// restored, or a new one deleted), so the two never disagree.
    pub fn save(
        &self,
        name: &str,
        server_url: &str,
        stream_key: &str,
    ) -> Result<SavedDestination, DestinationsError> {
        let destination = RtmpDestination::new(server_url, stream_key)?;
        let name = entry_name(name, &destination);
        let mut file = self.read();
        let existing = file
            .destinations
            .iter()
            .find(|saved| saved.server_url == destination.server_url() && saved.name == name)
            .cloned();
        let previous_key = existing.as_ref().map(|saved| self.secrets.get(&saved.id));
        let saved = match existing {
            Some(saved) => saved,
            None => {
                if file.destinations.len() >= MAX_DESTINATIONS {
                    return Err(DestinationsError::Full);
                }
                let saved = SavedDestination {
                    id: new_id(),
                    name,
                    server_url: destination.server_url().to_string(),
                };
                file.destinations.push(saved.clone());
                saved
            }
        };
        self.secrets
            .set(&saved.id, destination.stream_key())
            .map_err(DestinationsError::Store)?;
        if let Err(error) = self.write(&file) {
            let rollback = match previous_key {
                Some(Ok(Some(previous))) => self.secrets.set(&saved.id, &previous),
                Some(Ok(None)) | None => self.secrets.delete(&saved.id),
                Some(Err(read_error)) => Err(read_error),
            };
            if let Err(rollback_error) = rollback {
                log::warn!(
                    "streaming: couldn't roll back the stream key for {}: {rollback_error}",
                    saved.id
                );
            }
            return Err(error);
        }
        Ok(saved)
    }

    pub fn delete(&self, id: &str) -> Result<(), DestinationsError> {
        let mut file = self.read();
        let before = file.destinations.len();
        file.destinations.retain(|saved| saved.id != id);
        if file.destinations.len() == before {
            return Err(DestinationsError::NotFound);
        }
        // The file first: once the entry is gone nothing can reach the key,
        // so a key that then fails to delete is only an orphan, never a
        // listed destination without its key.
        self.write(&file)?;
        if let Err(error) = self.secrets.delete(id) {
            log::warn!("streaming: deleted destination {id}, but not its stream key: {error}");
        }
        Ok(())
    }

    /// The full destination, key included, for starting a stream.
    pub fn resolve(&self, id: &str) -> Result<RtmpDestination, DestinationsError> {
        let saved = self
            .list()
            .into_iter()
            .find(|saved| saved.id == id)
            .ok_or(DestinationsError::NotFound)?;
        let key = self
            .secrets
            .get(&saved.id)
            .map_err(DestinationsError::Store)?
            .ok_or(DestinationsError::MissingKey)?;
        Ok(RtmpDestination::new(&saved.server_url, &key)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::keystore::FakeStore;
    use std::sync::Arc;

    struct Shared(Arc<FakeStore>);
    impl SecretStore for Shared {
        fn set(&self, account: &str, secret: &str) -> Result<(), String> {
            self.0.set(account, secret)
        }
        fn get(&self, account: &str) -> Result<Option<String>, String> {
            self.0.get(account)
        }
        fn delete(&self, account: &str) -> Result<(), String> {
            self.0.delete(account)
        }
    }

    fn store() -> (DestinationStore, Arc<FakeStore>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("petal-dest-{}", new_id()));
        let fake = Arc::new(FakeStore::default());
        (DestinationStore::new(&dir, Box::new(Shared(fake.clone()))), fake, dir)
    }

    #[test]
    fn keys_go_to_the_secret_store_never_the_file() {
        let (store, fake, dir) = store();
        let saved = store.save("Twitch", "rtmp://live.twitch.tv/app", "live_secret").unwrap();
        let raw = std::fs::read_to_string(dir.join(DESTINATIONS_FILE)).unwrap();
        assert!(raw.contains("live.twitch.tv"));
        assert!(!raw.contains("live_secret"));
        assert_eq!(fake.items.lock().unwrap().get(&saved.id).map(String::as_str), Some("live_secret"));
        assert_eq!(store.resolve(&saved.id).unwrap().stream_key(), "live_secret");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resaving_replaces_the_key_and_default_names_use_the_host() {
        let (store, fake, dir) = store();
        let first = store.save("", "rtmps://a.rtmps.youtube.com:443/live2", "k1").unwrap();
        assert_eq!(first.name, "a.rtmps.youtube.com");
        let second = store.save("", "rtmps://a.rtmps.youtube.com:443/live2", "k2").unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(store.list().len(), 1);
        assert_eq!(fake.items.lock().unwrap().get(&first.id).map(String::as_str), Some("k2"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn at_most_ten_and_delete_removes_the_key() {
        let (store, fake, dir) = store();
        let ids: Vec<String> = (0..MAX_DESTINATIONS)
            .map(|i| store.save(&format!("d{i}"), "rtmp://example.com/app", "k").unwrap().id)
            .collect();
        assert_eq!(
            store.save("one more", "rtmp://example.com/app", "k"),
            Err(DestinationsError::Full)
        );
        let destination = RtmpDestination::new("rtmp://example.com/app", "k2").unwrap();
        assert_eq!(store.ensure_room("one more", &destination), Err(DestinationsError::Full));
        // Re-saving an existing entry replaces its key, so it always fits.
        assert_eq!(store.ensure_room("d3", &destination), Ok(()));
        store.delete(&ids[0]).unwrap();
        assert!(!fake.items.lock().unwrap().contains_key(&ids[0]));
        assert_eq!(store.list().len(), MAX_DESTINATIONS - 1);
        assert_eq!(store.delete(&ids[0]), Err(DestinationsError::NotFound));
        assert!(store.save("one more", "rtmp://example.com/app", "k").is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_key_is_reported() {
        let (store, fake, dir) = store();
        let saved = store.save("x", "rtmp://example.com/app", "k").unwrap();
        fake.items.lock().unwrap().clear();
        assert_eq!(store.resolve(&saved.id).err(), Some(DestinationsError::MissingKey));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Makes the next file write fail: a directory where the temp file goes.
    fn block_writes(dir: &Path) {
        std::fs::create_dir_all(dir.join(DESTINATIONS_FILE).with_extension("json.tmp")).unwrap();
    }

    fn unblock_writes(dir: &Path) {
        std::fs::remove_dir(dir.join(DESTINATIONS_FILE).with_extension("json.tmp")).unwrap();
    }

    #[test]
    fn a_failed_file_write_rolls_the_key_back() {
        let (store, fake, dir) = store();
        // A new destination: its key is deleted again.
        block_writes(&dir);
        assert!(matches!(
            store.save("Twitch", "rtmp://live.twitch.tv/app", "live_1"),
            Err(DestinationsError::Store(_))
        ));
        assert!(fake.items.lock().unwrap().is_empty());
        assert!(store.list().is_empty());
        // Replacing one: the previous key comes back.
        unblock_writes(&dir);
        let saved = store.save("Twitch", "rtmp://live.twitch.tv/app", "live_1").unwrap();
        block_writes(&dir);
        assert!(store.save("Twitch", "rtmp://live.twitch.tv/app", "live_2").is_err());
        assert_eq!(store.resolve(&saved.id).unwrap().stream_key(), "live_1");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn delete_removes_the_entry_first_and_only_logs_a_stuck_key() {
        let (store, fake, dir) = store();
        let saved = store.save("x", "rtmp://example.com/app", "k").unwrap();
        // The file can't be written: nothing changes, the key included.
        block_writes(&dir);
        assert!(matches!(store.delete(&saved.id), Err(DestinationsError::Store(_))));
        assert_eq!(store.list().len(), 1);
        assert!(fake.items.lock().unwrap().contains_key(&saved.id));
        unblock_writes(&dir);
        // The keychain refuses: the destination is still deleted.
        fake.fail_delete.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(store.delete(&saved.id), Ok(()));
        assert!(store.list().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_destinations_are_not_saved() {
        let (store, _fake, dir) = store();
        assert!(matches!(
            store.save("x", "http://example.com/app", "k"),
            Err(DestinationsError::Invalid(DestinationError::Scheme))
        ));
        assert!(store.list().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
