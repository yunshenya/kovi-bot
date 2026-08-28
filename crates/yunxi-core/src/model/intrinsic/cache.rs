//! Deliberately non-persistent, bounded runtime cache primitives.

use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct BoundedInferenceCache<T> {
    capacity: usize,
    entries: VecDeque<T>,
}

impl<T> BoundedInferenceCache<T> {
    pub fn new(capacity: usize) -> Option<Self> {
        (capacity > 0).then(|| Self {
            capacity,
            entries: VecDeque::with_capacity(capacity),
        })
    }

    pub fn push(&mut self, value: T) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(value);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}
