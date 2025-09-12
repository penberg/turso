use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::collections::{BTreeMap, HashSet};
use std::hash::Hash;
use parking_lot::{RwLock, Mutex};
use smallvec::SmallVec;

/// Page ID - logical identifier for tree nodes
pub type PageId = u64;

/// Special page IDs
const INVALID_PAGE_ID: PageId = 0;
const ROOT_PAGE_ID: PageId = 1;
const FIRST_LEAF_PAGE_ID: PageId = 2;

/// Node types in the BwTree
#[derive(Debug)]
pub enum Node<K, V> {
    /// Leaf base page with sorted key-value pairs
    LeafBase(LeafBase<K, V>),
    /// Insert delta record
    InsertDelta(InsertDelta<K, V>),
    /// Delete delta record  
    DeleteDelta(DeleteDelta<K, V>),
    /// Split delta record
    SplitDelta(SplitDelta<K, V>),
}

/// Leaf base page containing sorted key-value pairs
#[derive(Debug)]
pub struct LeafBase<K, V> {
    /// Sorted key-value pairs - using SmallVec for common case of few values per key
    pub entries: Vec<(K, SmallVec<[V; 2]>)>,
    /// High key for this page (exclusive upper bound)
    pub high_key: Option<K>,
    /// Page ID of right sibling
    pub right_sibling: PageId,
}

/// Insert delta record
#[derive(Debug)]
pub struct InsertDelta<K, V> {
    /// Key being inserted
    pub key: K,
    /// Value being inserted
    pub value: V,
    /// Pointer to the next record in delta chain
    pub next: *mut Node<K, V>,
}

/// Delete delta record
#[derive(Debug)]
pub struct DeleteDelta<K, V> {
    /// Key being deleted
    pub key: K,
    /// Value being deleted (for multi-value support)
    pub value: V,
    /// Pointer to the next record in delta chain
    pub next: *mut Node<K, V>,
}

/// Split delta record
#[derive(Debug)]
pub struct SplitDelta<K, V> {
    /// Split key
    pub split_key: K,
    /// Page ID of the new right sibling
    pub right_sibling_pid: PageId,
    /// Pointer to the next record in delta chain
    pub next: *mut Node<K, V>,
}

/// Simple memory pool for delta records - reduces allocation overhead
#[derive(Debug)]
struct DeltaPool<K, V> {
    insert_pool: Vec<Box<Node<K, V>>>,
    delete_pool: Vec<Box<Node<K, V>>>,
}

impl<K, V> DeltaPool<K, V> {
    fn new() -> Self {
        Self {
            insert_pool: Vec::with_capacity(64), // Pre-allocate for common case
            delete_pool: Vec::with_capacity(32),
        }
    }
    
    fn get_insert_node(&mut self) -> Option<Box<Node<K, V>>> {
        self.insert_pool.pop()
    }
    
    fn get_delete_node(&mut self) -> Option<Box<Node<K, V>>> {
        self.delete_pool.pop()
    }
    
    fn return_node(&mut self, node: Box<Node<K, V>>) {
        match *node {
            Node::InsertDelta(_) if self.insert_pool.len() < 64 => {
                self.insert_pool.push(node);
            }
            Node::DeleteDelta(_) if self.delete_pool.len() < 32 => {
                self.delete_pool.push(node);
            }
            _ => {
                // Drop node if pools are full or wrong type
                drop(node);
            }
        }
    }
}

/// BwTree main structure
#[derive(Debug)]
pub struct BwTree<K, V> {
    /// Mapping table: maps PageId to physical pointers - using BTreeMap for better cache locality
    mapping_table: RwLock<BTreeMap<PageId, AtomicPtr<Node<K, V>>>>,
    /// Root page ID
    root_pid: AtomicU64,
    /// Next available page ID
    next_pid: AtomicU64,
    /// Maximum delta chain length before consolidation
    max_delta_chain_length: usize,
    /// Memory pool for delta records to reduce allocations
    #[allow(dead_code)] // Will be used in future optimizations
    delta_pool: Mutex<DeltaPool<K, V>>,
}

impl<K, V> BwTree<K, V>
where
    K: Clone + Ord + Hash,
    V: Clone + PartialEq + Eq + Hash,
{
    pub fn new() -> Self {
        let mut mapping_table = BTreeMap::new();
        
        // Create initial root page
        let root_page = Box::into_raw(Box::new(Node::LeafBase(LeafBase {
            entries: Vec::new(),
            high_key: None,
            right_sibling: INVALID_PAGE_ID,
        })));
        
        mapping_table.insert(ROOT_PAGE_ID, AtomicPtr::new(root_page));
        
        Self {
            mapping_table: RwLock::new(mapping_table),
            root_pid: AtomicU64::new(ROOT_PAGE_ID),
            next_pid: AtomicU64::new(FIRST_LEAF_PAGE_ID),
            max_delta_chain_length: 8,  // Match reference implementation threshold
            delta_pool: Mutex::new(DeltaPool::new()),
        }
    }

    /// Insert a value for a key. BwTree supports multiple values per key.
    pub fn insert(&self, key: K, value: V) -> bool {
        loop {
            // Find the target page for insertion
            let target_pid = self.find_leaf_page(&key);
            
            // Create insert delta record
            let old_node = {
                let mapping_table = self.mapping_table.read();
                if let Some(atomic_ptr) = mapping_table.get(&target_pid) {
                    atomic_ptr.load(Ordering::Acquire)
                } else {
                    // Page doesn't exist, retry
                    continue;
                }
            };
            
            let insert_delta = Box::into_raw(Box::new(Node::InsertDelta(InsertDelta {
                key: key.clone(),
                value: value.clone(),
                next: old_node,
            })));
            
            // Try to CAS the mapping table entry
            {
                let mapping_table = self.mapping_table.read();
                if let Some(atomic_ptr) = mapping_table.get(&target_pid) {
                    match atomic_ptr.compare_exchange_weak(
                        old_node,
                        insert_delta,
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => {
                            // Success! Check if we need consolidation
                            if self.needs_consolidation(insert_delta) {
                                self.consolidate_page(target_pid);
                            }
                            return true;
                        }
                        Err(_) => {
                            // CAS failed, clean up and retry
                            unsafe { Box::from_raw(insert_delta) };
                            continue;
                        }
                    }
                } else {
                    // Page disappeared, clean up and retry
                    unsafe { Box::from_raw(insert_delta) };
                    continue;
                }
            }
        }
    }
    
    /// Get all values for a specific key
    pub fn get(&self, key: &K) -> Vec<V> {
        let leaf_pid = self.find_leaf_page(key);
        
        let node_ptr = {
            let mapping_table = self.mapping_table.read();
            if let Some(atomic_ptr) = mapping_table.get(&leaf_pid) {
                atomic_ptr.load(Ordering::Acquire)
            } else {
                return Vec::new();
            }
        };
        
        if node_ptr.is_null() {
            return Vec::new();
        }
        
        // Collect all deltas first, then process in reverse order
        let mut delta_stack = Vec::new();
        let mut current = node_ptr;
        let mut base_values = Vec::new();
        
        unsafe {
            while !current.is_null() {
                match &*current {
                    Node::LeafBase(base) => {
                        // Search in base page entries
                        if let Ok(idx) = base.entries.binary_search_by(|(k, _)| k.cmp(key)) {
                            base_values.extend(base.entries[idx].1.iter().cloned());
                        }
                        break;
                    },
                    Node::InsertDelta(delta) => {
                        if delta.key == *key {
                            delta_stack.push(("insert", delta.value.clone()));
                        }
                        current = delta.next;
                    },
                    Node::DeleteDelta(delta) => {
                        if delta.key == *key {
                            delta_stack.push(("delete", delta.value.clone()));
                        }
                        current = delta.next;
                    },
                    Node::SplitDelta(delta) => {
                        current = delta.next;
                    },
                }
            }
        }
        
        // Start with base values
        let mut values = base_values;
        
        // Apply deltas in reverse order (oldest first)
        for (op, value) in delta_stack.iter().rev() {
            match *op {
                "insert" => values.push(value.clone()),
                "delete" => values.retain(|v| v != value),
                _ => {}
            }
        }
        
        values
    }

    /// Delete a specific value for a key  
    pub fn delete(&self, key: &K, value: &V) -> bool
    {
        loop {
            let leaf_pid = self.find_leaf_page(key);
            
            let old_node = {
                let mapping_table = self.mapping_table.read();
                if let Some(atomic_ptr) = mapping_table.get(&leaf_pid) {
                    atomic_ptr.load(Ordering::Acquire)
                } else {
                    return false;
                }
            };
            
            // Check if the key-value pair exists
            if !self.key_value_exists(key, value, old_node) {
                return false;
            }
            
            let delete_delta = Box::into_raw(Box::new(Node::DeleteDelta(DeleteDelta {
                key: key.clone(),
                value: value.clone(),
                next: old_node,
            })));
            
            {
                let mapping_table = self.mapping_table.read();
                if let Some(atomic_ptr) = mapping_table.get(&leaf_pid) {
                    match atomic_ptr.compare_exchange_weak(
                        old_node,
                        delete_delta,
                        Ordering::Release,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => {
                            if self.needs_consolidation(delete_delta) {
                                self.consolidate_page(leaf_pid);
                            }
                            return true;
                        },
                        Err(_) => {
                            unsafe { Box::from_raw(delete_delta) };
                            continue; // Retry
                        }
                    }
                } else {
                    unsafe { Box::from_raw(delete_delta) };
                    return false;
                }
            }
        }
    }
    
    /// Check if a key-value pair exists in the page
    fn key_value_exists(&self, key: &K, value: &V, node_ptr: *mut Node<K, V>) -> bool
    {
        if node_ptr.is_null() {
            return false;
        }
        
        let mut current = node_ptr;
        let mut found_inserts = Vec::new();
        let mut found_deletes = Vec::new();
        
        unsafe {
            while !current.is_null() {
                match &*current {
                    Node::LeafBase(base) => {
                        if let Ok(idx) = base.entries.binary_search_by(|(k, _)| k.cmp(key)) {
                            found_inserts.extend(base.entries[idx].1.iter());
                        }
                        break;
                    },
                    Node::InsertDelta(delta) => {
                        if delta.key == *key {
                            found_inserts.push(&delta.value);
                        }
                        current = delta.next;
                    },
                    Node::DeleteDelta(delta) => {
                        if delta.key == *key {
                            found_deletes.push(&delta.value);
                        }
                        current = delta.next;
                    },
                    Node::SplitDelta(delta) => {
                        current = delta.next;
                    },
                }
            }
        }
        
        // Check if value exists after applying all deltas
        found_inserts.contains(&value) && !found_deletes.contains(&value)
    }
    
    /// Find the leaf page that should contain the given key
    fn find_leaf_page(&self, _key: &K) -> PageId {
        // Simplified: always return root for now
        // In full implementation, would traverse internal nodes
        self.root_pid.load(Ordering::Acquire)
    }
    
    /// Check if page needs consolidation based on delta chain length
    fn needs_consolidation(&self, node_ptr: *mut Node<K, V>) -> bool {
        let mut chain_length = 0;
        let mut current = node_ptr;
        
        unsafe {
            while !current.is_null() && chain_length < self.max_delta_chain_length {
                match &*current {
                    Node::LeafBase(_) => break,
                    Node::InsertDelta(delta) => current = delta.next,
                    Node::DeleteDelta(delta) => current = delta.next,
                    Node::SplitDelta(delta) => current = delta.next,
                }
                chain_length += 1;
            }
        }
        
        chain_length >= self.max_delta_chain_length
    }
    
    /// Consolidate delta chain into a new base page - optimized version
    fn consolidate_page(&self, pid: PageId) {
        // Get current page pointer with single lock acquisition
        let mapping_table = self.mapping_table.read();
        let current_node = if let Some(atomic_ptr) = mapping_table.get(&pid) {
            atomic_ptr.load(Ordering::Acquire)
        } else {
            return; // Page doesn't exist
        };
        
        if current_node.is_null() {
            return;
        }
        
        // Apply all deltas to create consolidated entries
        let consolidated_entries = self.apply_deltas(current_node);
        
        // Skip consolidation if page is empty after applying deltas
        if consolidated_entries.is_empty() {
            return;
        }
        
        // Create new base page
        let new_base = Box::into_raw(Box::new(Node::LeafBase(LeafBase {
            entries: consolidated_entries,
            high_key: None, // Simplified
            right_sibling: INVALID_PAGE_ID,
        })));
        
        // Try to atomically replace the old chain with new base page
        if let Some(atomic_ptr) = mapping_table.get(&pid) {
            match atomic_ptr.compare_exchange_weak(
                current_node,
                new_base,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // Success! Old chain will be garbage collected
                    // In real implementation, would use epoch-based GC
                    self.schedule_cleanup(current_node);
                }
                Err(_) => {
                    // CAS failed, someone else modified the page
                    unsafe { Box::from_raw(new_base) };
                }
            }
        } else {
            // Page disappeared
            unsafe { Box::from_raw(new_base) };
        }
    }
    
    /// Apply all delta records - ULTRA OPTIMIZED with deduplication and NO COLLECT()
    fn apply_deltas(&self, node_ptr: *mut Node<K, V>) -> Vec<(K, SmallVec<[V; 2]>)> {
        unsafe {
            // Fast path: base-only case
            if let Node::LeafBase(base) = &*node_ptr {
                return base.entries.clone();
            }
            
            let mut unique_deltas = Vec::new();
            let mut seen_kv = HashSet::new();
            let mut current = node_ptr;
            let mut base_entries = None;
            let mut estimated_size = 0;
            
            // Pass 1: Collect unique deltas with deduplication (reference BwTree style)
            while !current.is_null() {
                match &*current {
                    Node::LeafBase(base) => {
                        base_entries = Some(&base.entries);
                        estimated_size = base.entries.len();
                        break;
                    }
                    Node::InsertDelta(delta) => {
                        let kv = (delta.key.clone(), delta.value.clone());
                        if seen_kv.insert(kv.clone()) {
                            unique_deltas.push(('I', kv));
                        }
                        current = delta.next;
                    }
                    Node::DeleteDelta(delta) => {
                        let kv = (delta.key.clone(), delta.value.clone());
                        if seen_kv.insert(kv.clone()) {
                            unique_deltas.push(('D', kv));
                        }
                        current = delta.next;
                    }
                    Node::SplitDelta(delta) => {
                        current = delta.next;
                    }
                }
            }
            
            // Reverse to get chronological order (oldest first)
            unique_deltas.reverse();
            
            // Pass 2: Build result directly with minimal allocations
            let mut result_map = BTreeMap::new();
            
            // Start with base entries
            if let Some(base) = base_entries {
                for (key, values) in base {
                    result_map.insert(key.clone(), values.clone());
                }
            }
            
            // Apply unique deltas in chronological order
            for (op, (key, value)) in unique_deltas {
                match op {
                    'I' => {
                        result_map.entry(key)
                            .or_insert_with(SmallVec::new)
                            .push(value);
                    }
                    'D' => {
                        if let Some(values) = result_map.get_mut(&key) {
                            values.retain(|v| v != &value);
                        }
                    }
                    _ => unreachable!(),
                }
            }
            
            // Build final result - NO COLLECT()! Direct Vec construction
            let mut result = Vec::with_capacity(estimated_size);
            for (key, values) in result_map {
                if !values.is_empty() {
                    result.push((key, values));
                }
            }
            result
        }
    }
    
    /// Get the depth of delta chain for pre-allocation
    fn get_delta_chain_depth(&self, mut node_ptr: *mut Node<K, V>) -> usize {
        let mut depth = 0;
        unsafe {
            while !node_ptr.is_null() {
                match &*node_ptr {
                    Node::LeafBase(_) => break,
                    Node::InsertDelta(delta) => {
                        depth += 1;
                        node_ptr = delta.next;
                    }
                    Node::DeleteDelta(delta) => {
                        depth += 1;
                        node_ptr = delta.next;
                    }
                    Node::SplitDelta(delta) => {
                        node_ptr = delta.next;
                    }
                }
            }
        }
        depth
    }
    
    /// Schedule cleanup of old delta chain (simplified)
    fn schedule_cleanup(&self, _old_chain: *mut Node<K, V>) {
        // TODO: In a real implementation, this would use epoch-based
        // garbage collection to safely reclaim memory
    }

    /// Get all keys (for iteration) - optimized to avoid redundant work
    pub fn keys(&self) -> Vec<K> {
        // Use the mapping table's BTreeMap ordering
        let mapping_table = self.mapping_table.read();
        let mut result_keys = BTreeMap::new();
        
        for (_, atomic_ptr) in mapping_table.iter() {
            let node_ptr = atomic_ptr.load(Ordering::Acquire);
            if !node_ptr.is_null() {
                // Directly consolidate this page to get accurate keys
                let consolidated = self.apply_deltas(node_ptr);
                for (key, values) in consolidated {
                    if !values.is_empty() {
                        result_keys.insert(key, ());
                    }
                }
            }
        }
        
        result_keys.into_keys().collect()
    }

    /// Get number of key-value pairs
    pub fn len(&self) -> usize {
        let mut count = 0;
        let keys = self.keys();
        
        for key in keys {
            let values = self.get(&key);
            count += values.len();
        }
        
        count
    }

    /// Remove all values for a key (used when no versions remain)
    pub fn remove(&self, key: &K) -> bool {
        // Get all values first
        let values = self.get(key);
        if values.is_empty() {
            return false;
        }
        
        // Delete each value
        let mut success = true;
        for value in values {
            if !self.delete(key, &value) {
                success = false;
            }
        }
        
        success
    }
    
    /// Check if tree is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

unsafe impl<K: Send, V: Send> Send for BwTree<K, V> {}
unsafe impl<K: Sync, V: Sync> Sync for BwTree<K, V> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_insert_and_get() {
        let tree = BwTree::new();
        
        // Test single insert and get
        assert!(tree.insert(1, "one".to_string()));
        let values = tree.get(&1);
        assert_eq!(values, vec!["one".to_string()]);
        
        // Test non-existent key
        let empty = tree.get(&999);
        assert!(empty.is_empty());
    }

    #[test]
    fn test_multiple_values_per_key() {
        let tree = BwTree::new();
        
        // Insert multiple values for same key
        assert!(tree.insert(1, "one".to_string()));
        assert!(tree.insert(1, "uno".to_string()));
        assert!(tree.insert(1, "eins".to_string()));
        
        let values = tree.get(&1);
        assert_eq!(values.len(), 3);
        assert!(values.contains(&"one".to_string()));
        assert!(values.contains(&"uno".to_string()));
        assert!(values.contains(&"eins".to_string()));
    }

    #[test]
    fn test_delete_operations() {
        let tree = BwTree::new();
        
        // Setup test data
        tree.insert(1, "one".to_string());
        tree.insert(1, "uno".to_string());
        tree.insert(2, "two".to_string());
        
        // Delete specific value
        assert!(tree.delete(&1, &"uno".to_string()));
        let values = tree.get(&1);
        assert_eq!(values, vec!["one".to_string()]);
        
        // Try to delete non-existent value
        assert!(!tree.delete(&1, &"missing".to_string()));
        
        // Delete remaining value
        assert!(tree.delete(&1, &"one".to_string()));
        let values = tree.get(&1);
        assert!(values.is_empty());
    }

    #[test]
    fn test_tree_operations() {
        let tree = BwTree::new();
        assert_eq!(tree.len(), 0);
        assert!(tree.is_empty());
        
        // Add some data
        tree.insert(3, "three".to_string());
        tree.insert(1, "one".to_string());
        tree.insert(2, "two".to_string());
        
        assert_eq!(tree.len(), 3);
        assert!(!tree.is_empty());
        
        let keys = tree.keys();
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&1));
        assert!(keys.contains(&2));
        assert!(keys.contains(&3));
    }
}

// Iterator support (simplified)
pub struct BwTreeIterator<K, V> {
    keys: Vec<K>,
    values: Vec<V>,
    position: usize,
}

impl<K, V> BwTree<K, V> 
where
    K: Clone + Ord + Hash,
    V: Clone + PartialEq + Eq + Hash,
{
    /// Iterate over all key-value pairs - optimized to reuse consolidated data
    pub fn iter(&self) -> BwTreeIterator<K, V> {
        let mut keys = Vec::new();
        let mut values = Vec::new();
        
        // Consolidate all pages once and iterate
        let mapping_table = self.mapping_table.read();
        for (_, atomic_ptr) in mapping_table.iter() {
            let node_ptr = atomic_ptr.load(Ordering::Acquire);
            if !node_ptr.is_null() {
                let consolidated = self.apply_deltas(node_ptr);
                for (key, key_values) in consolidated {
                    for value in key_values {
                        keys.push(key.clone());
                        values.push(value);
                    }
                }
            }
        }
        
        BwTreeIterator {
            keys,
            values,
            position: 0,
        }
    }
    
    /// Range scan over keys - optimized to filter during consolidation
    pub fn range<R>(&self, range: R) -> BwTreeRangeIterator<K, V>
    where
        R: std::ops::RangeBounds<K>,
    {
        let mut keys = Vec::new();
        let mut values = Vec::new();
        
        // Consolidate all pages once and filter during iteration
        let mapping_table = self.mapping_table.read();
        for (_, atomic_ptr) in mapping_table.iter() {
            let node_ptr = atomic_ptr.load(Ordering::Acquire);
            if !node_ptr.is_null() {
                let consolidated = self.apply_deltas(node_ptr);
                for (key, key_values) in consolidated {
                    if range.contains(&key) {
                        for value in key_values {
                            keys.push(key.clone());
                            values.push(value);
                        }
                    }
                }
            }
        }
        
        BwTreeRangeIterator {
            keys,
            values,
            position: 0,
        }
    }

    /// Find the first key greater than or equal to the given key - optimized
    pub fn lower_bound(&self, key: &K) -> Option<K> {
        // Use BTreeMap's range functionality for efficient bound searches
        let mapping_table = self.mapping_table.read();
        let mut candidate_keys = BTreeMap::new();
        
        for (_, atomic_ptr) in mapping_table.iter() {
            let node_ptr = atomic_ptr.load(Ordering::Acquire);
            if !node_ptr.is_null() {
                let consolidated = self.apply_deltas(node_ptr);
                for (k, values) in consolidated {
                    if !values.is_empty() && k >= *key {
                        candidate_keys.insert(k, ());
                    }
                }
            }
        }
        
        candidate_keys.into_keys().next()
    }

    /// Find the first key greater than the given key - optimized
    pub fn upper_bound(&self, key: &K) -> Option<K> {
        // Use BTreeMap's range functionality for efficient bound searches
        let mapping_table = self.mapping_table.read();
        let mut candidate_keys = BTreeMap::new();
        
        for (_, atomic_ptr) in mapping_table.iter() {
            let node_ptr = atomic_ptr.load(Ordering::Acquire);
            if !node_ptr.is_null() {
                let consolidated = self.apply_deltas(node_ptr);
                for (k, values) in consolidated {
                    if !values.is_empty() && k > *key {
                        candidate_keys.insert(k, ());
                    }
                }
            }
        }
        
        candidate_keys.into_keys().next()
    }
}

impl<K: Clone, V: Clone> Iterator for BwTreeIterator<K, V> {
    type Item = (K, V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.position < self.keys.len() {
            let key = self.keys[self.position].clone();
            let value = self.values[self.position].clone();
            self.position += 1;
            Some((key, value))
        } else {
            None
        }
    }
}

/// Iterator over key ranges
pub struct BwTreeRangeIterator<K, V> {
    keys: Vec<K>,
    values: Vec<V>,
    position: usize,
}


impl<K: Clone, V: Clone> Iterator for BwTreeRangeIterator<K, V> {
    type Item = (K, V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.position < self.keys.len() {
            let key = self.keys[self.position].clone();
            let value = self.values[self.position].clone();
            self.position += 1;
            Some((key, value))
        } else {
            None
        }
    }
}