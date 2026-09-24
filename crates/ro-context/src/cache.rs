//! Context cache management.
//!
//! Caches context data for repos to avoid repeated fetches.

use std::collections::HashMap;

/// In-memory cache for context data.
#[derive(Debug, Clone, Default)]
pub struct ContextCache {
    entries: HashMap<String, serde_json::Value>,
}

impl ContextCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.entries.get(key)
    }

    pub fn insert(&mut self, key: String, value: serde_json::Value) {
        self.entries.insert(key, value);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_get_and_insert() {
        let mut cache = ContextCache::new();
        cache.insert("test".into(), serde_json::json!({"a": 1}));
        assert_eq!(cache.len(), 1);
        assert!(cache.get("test").is_some());
        assert!(cache.get("missing").is_none());
    }

    #[test]
    fn empty_cache_is_empty() {
        let cache = ContextCache::new();
        assert!(cache.is_empty());
    }
}
