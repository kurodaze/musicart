//! In-memory caches that forget after a while, so a repeat doesn't ask the network again.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long YouTube's stream links are kept. They last about 6 hours, and a track still fetches
/// from its link while it plays, so 3 is safe.
pub const LINKS_KEPT_FOR: Duration = Duration::from_secs(3 * 3600);
/// How long everything else is kept. What a search found, matches, track details and keys don't
/// expire, they only drift.
pub const KEPT_FOR: Duration = Duration::from_secs(24 * 3600);
/// At most this many of each kind.
pub const KEPT: usize = 1000;

/// Values kept for `ttl`, at most `cap` of them, the oldest dropped first.
pub struct Recent<K, V> {
    ttl: Duration,
    cap: usize,
    map: Mutex<HashMap<K, (Instant, V)>>,
}

impl<K: Hash + Eq + Clone, V: Clone> Recent<K, V> {
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Recent {
            ttl,
            cap,
            map: Mutex::default(),
        }
    }

    pub fn get<Q: Hash + Eq + ?Sized>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        let map = self.map.lock().unwrap();
        let (at, value) = map.get(key)?;
        (at.elapsed() < self.ttl).then(|| value.clone())
    }

    pub fn put(&self, key: K, value: V) {
        let mut map = self.map.lock().unwrap();
        map.retain(|_, (at, _)| at.elapsed() < self.ttl);
        if map.len() >= self.cap
            && !map.contains_key(&key)
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
        map.insert(key, (Instant::now(), value));
    }

    /// Drops every value `gone` picks.
    pub fn remove_where(&self, gone: impl Fn(&V) -> bool) {
        self.map
            .lock()
            .unwrap()
            .retain(|_, (_, value)| !gone(value));
    }

    pub fn remove<Q: Hash + Eq + ?Sized>(&self, key: &Q)
    where
        K: Borrow<Q>,
    {
        self.map.lock().unwrap().remove(key);
    }
}
