#[derive(Debug)]
pub struct MemoryPool<V> {
    slots: Vec<Option<V>>,
    free_indices: Vec<usize>,
    capacity: usize,
}

impl<V> MemoryPool<V> {
    pub fn new(capacity: usize) -> Result<Self, crate::error::CacheError> {
        // Rule 7: validate parameters on entry. A zero-capacity pool would make
        // every allocation fail; reject the misconfiguration explicitly.
        if capacity == 0 {
            return Err(crate::error::CacheError::ConfigurationError);
        }
        let mut slots = Vec::with_capacity(capacity);
        let mut free_indices = Vec::with_capacity(capacity);
        for i in 0..capacity {
            slots.push(None);
            free_indices.push(i);
        }
        Ok(MemoryPool {
            slots,
            free_indices,
            capacity,
        })
    }

    pub fn allocate(&mut self, value: V) -> Result<usize, crate::error::CacheError> {
        match self.free_indices.pop() {
            Some(idx) => {
                self.slots[idx] = Some(value);
                Ok(idx)
            }
            None => Err(crate::error::CacheError::ConfigurationError),
        }
    }

    pub fn deallocate(&mut self, idx: usize) -> Result<(), crate::error::CacheError> {
        if idx >= self.capacity {
            return Err(crate::error::CacheError::ConfigurationError);
        }
        self.slots[idx] = None;
        self.free_indices.push(idx);
        Ok(())
    }

    pub fn get(&self, idx: usize) -> Option<&V> {
        if idx >= self.capacity {
            return None;
        }
        self.slots[idx].as_ref()
    }

    pub fn get_mut(&mut self, idx: usize) -> Option<&mut V> {
        if idx >= self.capacity {
            return None;
        }
        self.slots[idx].as_mut()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}
