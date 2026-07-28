use std::collections::{HashMap, VecDeque};

use anyhow::{Result, ensure};

#[derive(Debug)]
pub struct SeenQueries {
    entries: HashMap<(String, String), u64>,
    order: VecDeque<(String, String)>,
    capacity: usize,
}

impl SeenQueries {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn insert(
        &mut self,
        signer: String,
        query_id: String,
        expires_at_secs: u64,
        now_secs: u64,
    ) -> Result<bool> {
        self.remove_expired(now_secs);
        let key = (signer, query_id);
        if self.entries.contains_key(&key) {
            return Ok(false);
        }
        ensure!(self.capacity > 0, "seen-query capacity is zero");
        if self.entries.len() == self.capacity
            && let Some(evicted) = self.order.pop_front()
        {
            self.entries.remove(&evicted);
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, expires_at_secs);
        Ok(true)
    }

    fn remove_expired(&mut self, now_secs: u64) {
        self.entries.retain(|_, expiry| *expiry >= now_secs);
        self.order.retain(|key| self.entries.contains_key(key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_deduplicates_expires_and_evicts_with_a_bound() {
        let mut seen = SeenQueries::new(2);
        assert!(seen.insert("a".into(), "1".into(), 10, 1).unwrap());
        assert!(!seen.insert("a".into(), "1".into(), 10, 1).unwrap());
        assert!(seen.insert("b".into(), "2".into(), 10, 1).unwrap());
        assert!(seen.insert("c".into(), "3".into(), 10, 1).unwrap());
        assert_eq!(seen.entries.len(), 2);
        assert!(seen.insert("a".into(), "1".into(), 20, 11).unwrap());
        assert_eq!(seen.entries.len(), 1);
    }
}
