use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct Generation(pub u64);

impl Generation {
    pub fn new(value: u64) -> Self {
        Generation(value)
    }

    pub fn is_stale(&self, current: Generation) -> bool {
        self.0 < current.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    Absent,
    Ready,
    Stale,
    InFlight,
    Failed,
}

#[derive(Debug)]
pub struct CacheEntry<V> {
    pub value: V,
    pub generation: Generation,
    pub expiration: Option<Duration>,
}

impl<V> CacheEntry<V> {
    pub fn new(value: V, generation: Generation, expiration: Option<Duration>) -> Self {
        CacheEntry {
            value,
            generation,
            expiration,
        }
    }
}
