//! Delta snapshot store for Saxo Bank WebSocket streaming.
//!
//! Saxo's streaming API sends delta-compressed JSON: an initial snapshot (full state),
//! then subsequent messages with only changed fields. This store merges deltas into
//! snapshots so consumers always see complete current state.

use std::collections::HashMap;

use parking_lot::Mutex;

use serde_json::Value;

use super::error::SaxoError;

/// Maintains per-subscription JSON snapshots and applies delta updates.
pub struct SnapshotStore {
    snapshots: Mutex<HashMap<String, Value>>,
}

impl SnapshotStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            snapshots: Mutex::new(HashMap::new()),
        }
    }

    /// Store an initial full snapshot for a subscription.
    ///
    /// Overwrites any existing snapshot for this reference ID.
    pub fn set_snapshot(&self, reference_id: &str, snapshot: Value) {
        self.snapshots
            .lock()
            .insert(reference_id.to_string(), snapshot);
    }

    /// Apply a delta update and return the merged full state.
    ///
    /// Returns `NoSnapshotForRef` if no snapshot exists for the reference ID.
    pub fn apply_delta(&self, reference_id: &str, delta: &Value) -> Result<Value, SaxoError> {
        let mut snapshots = self.snapshots.lock();
        let snapshot = snapshots
            .get_mut(reference_id)
            .ok_or_else(|| SaxoError::NoSnapshotForRef(reference_id.to_string()))?;

        json_deep_merge(snapshot, delta);
        Ok(snapshot.clone())
    }

    /// Read the current snapshot for a subscription without modifying it.
    #[allow(dead_code)]
    pub fn get(&self, reference_id: &str) -> Option<Value> {
        self.snapshots.lock().get(reference_id).cloned()
    }

    /// Remove a subscription's snapshot. Returns `true` if it existed.
    pub fn remove(&self, reference_id: &str) -> bool {
        self.snapshots.lock().remove(reference_id).is_some()
    }

    /// Count active subscriptions.
    pub fn len(&self) -> usize {
        self.snapshots.lock().len()
    }

    /// Check if the store has no active subscriptions.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.snapshots.lock().is_empty()
    }
}

impl Default for SnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Recursively merge `delta` into `base`.
///
/// - Both objects: iterate delta keys, recursively merge each value into base.
/// - Everything else (arrays, scalars, null, type mismatches): replace base with delta.
fn json_deep_merge(base: &mut Value, delta: &Value) {
    match (base.is_object(), delta.is_object()) {
        (true, true) => {
            let base_obj = base.as_object_mut().expect("checked is_object");
            for (key, delta_val) in delta.as_object().expect("checked is_object") {
                match base_obj.get_mut(key) {
                    Some(base_val) => json_deep_merge(base_val, delta_val),
                    None => {
                        base_obj.insert(key.clone(), delta_val.clone());
                    }
                }
            }
        }
        _ => {
            *base = delta.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn apply_delta_adds_new_field() {
        let store = SnapshotStore::new();
        store.set_snapshot("ref1", json!({"price": 1.23}));

        let result = store.apply_delta("ref1", &json!({"volume": 100})).unwrap();

        assert_eq!(result, json!({"price": 1.23, "volume": 100}));
    }

    #[test]
    fn apply_delta_modifies_existing_field() {
        let store = SnapshotStore::new();
        store.set_snapshot("ref1", json!({"price": 1.23, "bid": 1.22}));

        let result = store.apply_delta("ref1", &json!({"price": 1.25})).unwrap();

        assert_eq!(result["price"], json!(1.25));
        assert_eq!(result["bid"], json!(1.22), "untouched field preserved");
    }

    #[test]
    fn apply_delta_nested_object_merge() {
        let store = SnapshotStore::new();
        store.set_snapshot(
            "ref1",
            json!({"quote": {"bid": 1.22, "ask": 1.24}, "status": "ok"}),
        );

        let result = store
            .apply_delta("ref1", &json!({"quote": {"bid": 1.23}}))
            .unwrap();

        assert_eq!(
            result,
            json!({"quote": {"bid": 1.23, "ask": 1.24}, "status": "ok"}),
            "nested merge: only bid changed, ask preserved"
        );
    }

    #[test]
    fn apply_delta_missing_snapshot_returns_error() {
        let store = SnapshotStore::new();

        let err = store
            .apply_delta("unknown", &json!({"price": 1.0}))
            .unwrap_err();

        assert!(
            matches!(err, SaxoError::NoSnapshotForRef(ref id) if id == "unknown"),
            "expected NoSnapshotForRef, got: {err:?}"
        );
    }

    #[test]
    fn multiple_refs_independent() {
        let store = SnapshotStore::new();
        store.set_snapshot("ref1", json!({"price": 1.0}));
        store.set_snapshot("ref2", json!({"price": 2.0}));

        store.apply_delta("ref1", &json!({"price": 1.5})).unwrap();

        assert_eq!(store.get("ref1").unwrap()["price"], json!(1.5));
        assert_eq!(
            store.get("ref2").unwrap()["price"],
            json!(2.0),
            "ref2 untouched by ref1 delta"
        );
        assert_eq!(store.len(), 2);
    }
}
