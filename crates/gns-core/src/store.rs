//! Storage abstractions. `gns-store` provides the SQLite implementation.

use crate::error::HostError;
use crate::ids::EntryId;
use crate::transcript::TranscriptEntry;

/// Append-only transcript with in-place updates by id.
pub trait TranscriptStore: Send + Sync {
    /// Append an entry, returning its sequence number.
    fn append_entry(&self, entry: &TranscriptEntry) -> Result<i64, HostError>;
    /// Replace an entry by id (no-op when missing).
    fn update_entry(&self, entry: &TranscriptEntry) -> Result<bool, HostError>;
    /// Fetch one entry.
    fn entry(&self, id: &EntryId) -> Result<Option<TranscriptEntry>, HostError>;
    /// Newest `limit` entries in chronological order.
    fn tail(&self, limit: usize) -> Result<Vec<TranscriptEntry>, HostError>;
    /// Every entry in chronological order.
    fn all(&self) -> Result<Vec<TranscriptEntry>, HostError>;
    /// Number of entries.
    fn len(&self) -> Result<usize, HostError>;
    /// Whether the transcript is empty.
    fn is_empty(&self) -> Result<bool, HostError> {
        Ok(self.len()? == 0)
    }
}

/// String key-value store.
pub trait KvStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, HostError>;
    fn set(&self, key: &str, value: &str) -> Result<(), HostError>;
    fn delete(&self, key: &str) -> Result<(), HostError>;
    /// Atomically replace `expected` with `value`; returns whether it happened.
    fn compare_and_set(&self, key: &str, expected: &str, value: &str) -> Result<bool, HostError>;
}

/// JSON convenience helpers on any [`KvStore`].
pub trait KvJson: KvStore {
    fn get_json<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>, HostError> {
        match self.get(key)? {
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
            None => Ok(None),
        }
    }
    fn set_json<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<(), HostError> {
        self.set(key, &serde_json::to_string(value)?)
    }
}

impl<K: KvStore + ?Sized> KvJson for K {}
