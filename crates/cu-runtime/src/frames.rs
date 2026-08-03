//! In-memory frame store with a strict retention policy.
//!
//! The runtime keeps a bounded window of recently-observed frames so `act` and
//! `inspect` can reference them by `frame_id`. Raw image bytes are kept only
//! for the newest frames and dropped aggressively: once evicted, a frame is
//! still addressable for *stale-frame checks* (we keep its thumbnail
//! fingerprint) but no longer holds the full image in memory. Inspect reads the
//! image back from disk on demand.

use std::collections::{HashMap, VecDeque};

use cu_core::{ScreenFrame, ScreenSnapshot};

/// Metadata retained for a frame inside the store.
#[derive(Debug, Clone)]
pub struct StoredFrame {
    pub frame: ScreenFrame,
    pub snapshot: ScreenSnapshot,
    /// Index of creation, for LRU ordering.
    pub ordinal: u64,
}

pub struct FrameStore {
    by_id: HashMap<String, StoredFrame>,
    order: VecDeque<String>,
    limit: usize,
    ordinal_counter: u64,
}

impl FrameStore {
    pub fn new(limit: usize) -> Self {
        Self {
            by_id: HashMap::new(),
            order: VecDeque::new(),
            limit: limit.max(1),
            ordinal_counter: 0,
        }
    }

    pub fn insert(&mut self, frame: ScreenFrame, snapshot: ScreenSnapshot) {
        self.ordinal_counter += 1;
        let id = frame.frame_id.clone();
        if self.by_id.contains_key(&id) {
            // Re-insert keeps LRU position fresh.
            self.order.retain(|x| x != &id);
        }
        self.by_id.insert(
            id.clone(),
            StoredFrame {
                frame,
                snapshot,
                ordinal: self.ordinal_counter,
            },
        );
        self.order.push_back(id.clone());
        self.enforce_limit();
    }

    pub fn get(&self, frame_id: &str) -> Option<&StoredFrame> {
        self.by_id.get(frame_id)
    }

    pub fn get_mut(&mut self, frame_id: &str) -> Option<&mut StoredFrame> {
        self.by_id.get_mut(frame_id)
    }

    pub fn get_snapshot(&self, frame_id: &str) -> Option<&ScreenSnapshot> {
        self.by_id.get(frame_id).map(|s| &s.snapshot)
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn clear(&mut self) {
        self.by_id.clear();
        self.order.clear();
    }

    fn enforce_limit(&mut self) {
        while self.by_id.len() > self.limit {
            if let Some(oldest) = self.order.pop_front() {
                if self.by_id.remove(&oldest).is_none() {
                    break;
                }
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn frame(id: &str) -> ScreenFrame {
        ScreenFrame {
            frame_id: id.into(),
            session_id: "s".into(),
            captured_at: Utc::now(),
            image_path: None,
            image_bytes: None,
            width: 100,
            height: 100,
            display_id: "1".into(),
            bounds: cu_core::DisplayBounds {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            scale_factor: 2.0,
            active_application: None,
            active_window_title: None,
            perceptual_hash: None,
        }
    }

    fn snap() -> ScreenSnapshot {
        ScreenSnapshot {
            thumbnail: vec![0u8; 16],
            thumb_width: 4,
            thumb_height: 4,
            active_application: None,
            active_window_title: None,
            display_id: "1".into(),
            captured_at: Utc::now(),
        }
    }

    #[test]
    fn evicts_oldest_when_over_limit() {
        let mut store = FrameStore::new(2);
        store.insert(frame("a"), snap());
        store.insert(frame("b"), snap());
        store.insert(frame("c"), snap());
        assert_eq!(store.len(), 2);
        assert!(store.get("a").is_none(), "oldest should be evicted");
        assert!(store.get("b").is_some());
        assert!(store.get("c").is_some());
    }

    #[test]
    fn reinsert_refreshes_lru() {
        let mut store = FrameStore::new(2);
        store.insert(frame("a"), snap());
        store.insert(frame("b"), snap());
        // Touch "a" again so it becomes most recent.
        store.insert(frame("a"), snap());
        store.insert(frame("c"), snap());
        assert!(store.get("b").is_none());
        assert!(store.get("a").is_some());
        assert!(store.get("c").is_some());
    }

    #[test]
    fn snapshot_accessible_after_insert() {
        let mut store = FrameStore::new(2);
        store.insert(frame("x"), snap());
        assert!(store.get_snapshot("x").is_some());
        assert!(store.get_snapshot("missing").is_none());
    }

    #[test]
    fn clear_empties() {
        let mut store = FrameStore::new(2);
        store.insert(frame("a"), snap());
        store.insert(frame("b"), snap());
        store.clear();
        assert!(store.is_empty());
    }
}
