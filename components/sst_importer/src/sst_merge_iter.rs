// Copyright 2025 TiKV Project Authors. Licensed under Apache-2.0.

use std::cmp::Ordering;

use engine_rocks::RocksSstIterator;
use engine_traits::{Iterator, Result as EngineResult};

pub struct BinaryIterator<'a> {
    /// Vector of SST file iterators
    sst_iters: Vec<RocksSstIterator<'a>>,
    /// Binary min-heap storing iterator indices
    entry_cache: Vec<usize>,
    /// Cached current keys for each iterator to avoid repeated key() calls
    current_keys: Vec<Option<Vec<u8>>>,
}

impl<'a> BinaryIterator<'a> {
    /// Creates a new BinaryIterator from a vector of SST iterators
    ///
    /// # Arguments
    /// * `sst_iters` - Vector of SST file iterators, must be positioned at
    ///   valid entries
    ///
    /// # Returns
    /// * `Ok(BinaryIterator)` - Successfully created iterator
    /// * `Err(EngineError)` - If any iterator operation fails
    pub fn new(mut sst_iters: Vec<RocksSstIterator<'a>>) -> EngineResult<Self> {
        let len = sst_iters.len();
        if len == 0 {
            return Ok(BinaryIterator {
                sst_iters,
                entry_cache: Vec::new(),
                current_keys: Vec::new(),
            });
        }

        let mut current_keys = Vec::with_capacity(len);
        let mut entry_cache = Vec::new();

        // Initialize each iterator and build the heap
        for (i, iter) in sst_iters.iter_mut().enumerate() {
            if iter.valid()? {
                // Cache the current key to avoid repeated key() calls
                current_keys.push(Some(iter.key().to_vec()));
                entry_cache.push(i);
            } else {
                current_keys.push(None);
            }
        }

        let mut binary_iter = BinaryIterator {
            sst_iters,
            entry_cache,
            current_keys,
        };

        // Build the min-heap
        binary_iter.heapify();
        Ok(binary_iter)
    }

    /// Builds a min-heap from the current entry_cache
    fn heapify(&mut self) {
        if self.entry_cache.is_empty() {
            return;
        }

        let len = self.entry_cache.len();
        // Start from the last non-leaf node and sift down
        for i in (0..len / 2).rev() {
            self.sift_down(i);
        }
    }

    /// Sifts element up to maintain heap property
    ///
    /// # Arguments
    /// * `pos` - Position to sift up from
    fn sift_up(&mut self, mut pos: usize) {
        while pos > 0 {
            let parent = (pos - 1) / 2;
            if self.compare_heap_elements(pos, parent) != Ordering::Less {
                break;
            }
            self.entry_cache.swap(pos, parent);
            pos = parent;
        }
    }

    /// Sifts element down to maintain heap property
    ///
    /// # Arguments
    /// * `pos` - Position to sift down from
    fn sift_down(&mut self, mut pos: usize) {
        let len = self.entry_cache.len();
        loop {
            let mut min_pos = pos;
            let left_child = 2 * pos + 1;
            let right_child = 2 * pos + 2;

            // Find the minimum among parent and children
            if left_child < len && self.compare_heap_elements(left_child, min_pos) == Ordering::Less
            {
                min_pos = left_child;
            }

            if right_child < len
                && self.compare_heap_elements(right_child, min_pos) == Ordering::Less
            {
                min_pos = right_child;
            }

            // If no child is smaller, heap property is satisfied
            if min_pos == pos {
                break;
            }

            self.entry_cache.swap(pos, min_pos);
            pos = min_pos;
        }
    }

    /// Compares two heap elements by their current keys
    ///
    /// # Arguments
    /// * `i`, `j` - Indices in the heap (not iterator indices)
    ///
    /// # Returns
    /// * `Ordering` - Comparison result of the keys
    fn compare_heap_elements(&self, i: usize, j: usize) -> Ordering {
        let idx_i = self.entry_cache[i];
        let idx_j = self.entry_cache[j];

        match (&self.current_keys[idx_i], &self.current_keys[idx_j]) {
            (Some(key_i), Some(key_j)) => key_i.cmp(key_j),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }

    /// Adds an iterator index to the heap
    ///
    /// # Arguments
    /// * `iter_idx` - Index of the iterator to add
    fn push_to_heap(&mut self, iter_idx: usize) {
        self.entry_cache.push(iter_idx);
        let pos = self.entry_cache.len() - 1;
        self.sift_up(pos);
    }

    /// Removes and returns the minimum iterator index from the heap
    ///
    /// # Returns
    /// * `Some(index)` - Index of the iterator with minimum key
    /// * `None` - If heap is empty
    fn pop_from_heap(&mut self) -> Option<usize> {
        if self.entry_cache.is_empty() {
            return None;
        }

        let result = self.entry_cache[0];
        let last_idx = self.entry_cache.len() - 1;

        if last_idx > 0 {
            // Move last element to root and restore heap property
            self.entry_cache.swap(0, last_idx);
            self.entry_cache.pop();
            self.sift_down(0);
        } else {
            self.entry_cache.pop();
        }

        Some(result)
    }
}

impl<'a> Iterator for BinaryIterator<'a> {
    /// Returns the next key-value pair in sorted order
    ///
    /// This is our custom iteration method that returns actual key-value pairs.
    /// The standard Iterator trait methods below are implemented to satisfy
    /// the trait requirements but are not the primary interface.
    pub fn next_kv(&mut self) -> Option<EngineResult<(Vec<u8>, Vec<u8>)>> {
        while !self.entry_cache.is_empty() {
            // Get the iterator with the minimum key
            let min_iter_idx = match self.pop_from_heap() {
                Some(idx) => idx,
                None => return None,
            };

            let iter = &mut self.sst_iters[min_iter_idx];

            // Get the current key and value
            let current_key = match &self.current_keys[min_iter_idx] {
                Some(key) => key.clone(),
                None => return None, // This iterator is exhausted
            };

            let current_value = match iter.value() {
                Ok(value) => value.to_vec(),
                Err(e) => return Some(Err(e)),
            };

            // Advance the iterator to the next position
            match iter.next() {
                Ok(has_next) => {
                    if has_next && iter.valid().unwrap_or(false) {
                        // Update cached key and re-add to heap
                        self.current_keys[min_iter_idx] = Some(iter.key().to_vec());
                        self.push_to_heap(min_iter_idx);
                    } else {
                        // Iterator is exhausted
                        self.current_keys[min_iter_idx] = None;
                    }
                }
                Err(e) => return Some(Err(e)),
            }

            // Skip any duplicate keys from other iterators
            while !self.entry_cache.is_empty() {
                let next_min_idx = self.entry_cache[0];
                if let Some(next_key) = &self.current_keys[next_min_idx] {
                    if next_key == &current_key {
                        // Found duplicate key, skip this iterator's current entry
                        let skip_iter_idx = self.pop_from_heap().unwrap();
                        let skip_iter = &mut self.sst_iters[skip_iter_idx];

                        match skip_iter.next() {
                            Ok(has_next) => {
                                if has_next && skip_iter.valid().unwrap_or(false) {
                                    self.current_keys[skip_iter_idx] =
                                        Some(skip_iter.key().to_vec());
                                    self.push_to_heap(skip_iter_idx);
                                } else {
                                    self.current_keys[skip_iter_idx] = None;
                                }
                            }
                            Err(e) => return Some(Err(e)),
                        }
                    } else {
                        break; // No more duplicates
                    }
                } else {
                    break;
                }
            }

            return Some(Ok((current_key, current_value)));
        }

        None // All iterators are exhausted
    }

    // Standard Iterator trait methods - these are required but not the primary
    // interface Use next_kv() for actual key-value iteration

    fn next(&mut self) -> EngineResult<bool> {
        // Move to next position but don't return the actual data
        match self.next_kv() {
            Some(Ok(_)) => Ok(true),
            Some(Err(e)) => Err(e),
            None => Ok(false),
        }
    }

    fn seek(&mut self, _key: &[u8]) -> EngineResult<bool> {
        Err(engine_traits::Error::Other(
            "BinaryIterator does not support seek operation".into(),
        ))
    }

    fn seek_for_prev(&mut self, _key: &[u8]) -> EngineResult<bool> {
        Err(engine_traits::Error::Other(
            "BinaryIterator does not support seek_for_prev operation".into(),
        ))
    }

    fn seek_to_first(&mut self) -> EngineResult<bool> {
        Err(engine_traits::Error::Other(
            "BinaryIterator does not support seek_to_first operation".into(),
        ))
    }

    fn seek_to_last(&mut self) -> EngineResult<bool> {
        Err(engine_traits::Error::Other(
            "BinaryIterator does not support seek_to_last operation".into(),
        ))
    }

    fn prev(&mut self) -> EngineResult<bool> {
        Err(engine_traits::Error::Other(
            "BinaryIterator does not support prev operation".into(),
        ))
    }

    fn key(&self) -> &[u8] {
        panic!("BinaryIterator should use next_kv() to get key-value pairs")
    }

    fn value(&self) -> &[u8] {
        panic!("BinaryIterator should use next_kv() to get key-value pairs")
    }

    fn valid(&self) -> EngineResult<bool> {
        Ok(!self.entry_cache.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_iterator() {
        let empty_iters: Vec<RocksSstIterator> = Vec::new();
        let mut merge_iter = BinaryIterator::new(empty_iters).unwrap();
        assert!(!merge_iter.valid().unwrap());
        assert!(merge_iter.next_kv().is_none());
    }

    // Note: More comprehensive tests would require creating mock SST iterators
    // which is complex and would require substantial test infrastructure.
    // These tests should be added in integration tests where real SST files
    // can be created and used.
}
