use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

/// A simplified weighted set (ZSet) for incremental computation
/// Each element has a weight: +1 for additions, -1 for deletions, 0 for no change
#[derive(Clone, Debug)]
pub struct ZSet<T> {
    data: HashMap<T, i32>,
}

impl<T: Hash + Eq + Clone> PartialEq for ZSet<T> {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data
    }
}

impl<T: Hash + Eq + Clone> ZSet<T> {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    pub fn insert(&mut self, item: T, weight: i32) {
        let current = self.data.get(&item).copied().unwrap_or(0);
        let new_weight = current + weight;
        if new_weight == 0 {
            self.data.remove(&item);
        } else {
            self.data.insert(item, new_weight);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&T, i32)> {
        self.data.iter().map(|(k, &v)| (k, v))
    }

    /// Merge another ZSet into this one
    pub fn merge(&mut self, other: &ZSet<T>) {
        for (item, weight) in other.iter() {
            self.insert(item.clone(), weight);
        }
    }

    /// Create a ZSet from a vector of items with positive weights
    pub fn from_items(items: Vec<T>) -> Self {
        let mut zset = Self::new();
        for item in items {
            zset.insert(item, 1);
        }
        zset
    }

    /// Get all items with positive weights
    pub fn to_vec(&self) -> Vec<T> {
        self.data
            .iter()
            .filter(|(_, &weight)| weight > 0)
            .map(|(item, _)| item.clone())
            .collect()
    }
}

/// Represents a stream of changes (deltas) over time
#[derive(Clone, Debug)]
pub struct Stream<T> {
    current: ZSet<T>,
}

impl<T: Hash + Eq + Clone> Stream<T> {
    pub fn new() -> Self {
        Self {
            current: ZSet::new(),
        }
    }

    pub fn from_zset(zset: ZSet<T>) -> Self {
        Self { current: zset }
    }

    /// Apply a delta (change) to the stream
    pub fn apply_delta(&mut self, delta: &ZSet<T>) {
        self.current.merge(delta);
    }

    /// Get the current state of the stream

    /// Get the current state as a vector of items (only positive weights)
    pub fn to_vec(&self) -> Vec<T> {
        self.current.to_vec()
    }

    /// Push a new item to the stream
    pub fn push(&mut self, item: T) {
        self.current.insert(item, 1);
    }

    /// Filter and map the stream using DBSP functional composition
    pub fn filter_map<U, F>(&self, f: F) -> Stream<U>
    where
        U: Hash + Eq + Clone,
        F: Fn(&T) -> Option<(U, i32)>,
    {
        let mut result = Stream::new();
        for (item, weight) in self.current.iter() {
            if let Some((mapped, new_weight)) = f(item) {
                result.current.insert(mapped, weight * new_weight);
            }
        }
        result
    }

    /// Map the stream to a different type
    pub fn map<U, F>(&self, f: F) -> Stream<U>
    where
        U: Hash + Eq + Clone,
        F: Fn(&T) -> U,
    {
        let mut result = Stream::new();
        for (item, weight) in self.current.iter() {
            let mapped = f(item);
            result.current.insert(mapped, weight);
        }
        result
    }

    /// Filter the stream
    pub fn filter<F>(&self, predicate: F) -> Stream<T>
    where
        F: Fn(&T) -> bool,
    {
        let mut result = Stream::new();
        for (item, weight) in self.current.iter() {
            if predicate(item) {
                result.current.insert(item.clone(), weight);
            }
        }
        result
    }
}
