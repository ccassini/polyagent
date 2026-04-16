use std::sync::Arc;

use crossbeam_queue::ArrayQueue;

/// Lock-free bounded ring queue for low-latency event passing.
#[derive(Debug)]
pub struct Ring<T> {
    inner: Arc<ArrayQueue<T>>,
}

impl<T> Clone for Ring<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> Ring<T> {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(ArrayQueue::new(capacity)),
        }
    }

    pub fn try_push(&self, value: T) -> Result<(), T> {
        self.inner.push(value)
    }

    pub fn try_pop(&self) -> Option<T> {
        self.inner.pop()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }
}
