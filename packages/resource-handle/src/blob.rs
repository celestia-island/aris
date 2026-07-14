// Patched Blob: uses Arc<Vec<T>> internally instead of Arc<dyn AsRef<[T]>>
// to avoid trait object vtable dispatch (which produces NULL on kei VM).

use core::sync::atomic::{AtomicU64, Ordering};

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct Blob<T> {
    data: std::sync::Arc<std::vec::Vec<T>>,
    id: u64,
}

impl<T> Clone for Blob<T> {
    fn clone(&self) -> Self {
        Self { data: self.data.clone(), id: self.id }
    }
}

impl<T> std::fmt::Debug for Blob<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Blob").field("len", &self.data.len()).field("id", &self.id).finish()
    }
}

impl<T> PartialEq for Blob<T> {
    fn eq(&self, other: &Self) -> bool { self.id == other.id }
}

impl<T: Clone> Blob<T> {
    pub fn new(data: std::sync::Arc<dyn AsRef<[T]> + Send + Sync>) -> Self {
        let bytes: &[T] = data.as_ref().as_ref();
        Self {
            data: std::sync::Arc::new(bytes.to_vec()),
            id: ID_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn from_raw_parts(data: std::sync::Arc<dyn AsRef<[T]> + Send + Sync>, id: u64) -> Self {
        let bytes: &[T] = data.as_ref().as_ref();
        Self {
            data: std::sync::Arc::new(bytes.to_vec()),
            id,
        }
    }
}

impl<T: Clone + Send + Sync + 'static> Blob<T> {
    pub fn into_raw_parts(self) -> (std::sync::Arc<dyn AsRef<[T]> + Send + Sync>, u64) {
        (self.data, self.id)
    }
}

impl<T> Blob<T> {
    pub fn from_vec(data: std::vec::Vec<T>) -> Self {
        Self {
            data: std::sync::Arc::new(data),
            id: ID_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn len(&self) -> usize { self.data.len() }
    pub fn is_empty(&self) -> bool { self.data.is_empty() }
    pub fn data(&self) -> &[T] { &self.data }
    pub fn id(&self) -> u64 { self.id }
    pub fn strong_count(&self) -> usize { std::sync::Arc::strong_count(&self.data) }

    /// Downgrade to a WeakBlob that doesn't keep the data alive.
    pub fn downgrade(&self) -> WeakBlob<T> {
        WeakBlob { weak: std::sync::Arc::downgrade(&self.data), id: self.id }
    }
}

impl<T> AsRef<[T]> for Blob<T> {
    fn as_ref(&self) -> &[T] { &self.data }
}

impl<T> core::ops::Deref for Blob<T> {
    type Target = [T];
    fn deref(&self) -> &[T] { &self.data }
}

/// Weak reference to a Blob's data. Doesn't prevent deallocation.
pub struct WeakBlob<T> {
    weak: std::sync::Weak<std::vec::Vec<T>>,
    id: u64,
}

impl<T> Clone for WeakBlob<T> {
    fn clone(&self) -> Self {
        Self { weak: self.weak.clone(), id: self.id }
    }
}

impl<T> WeakBlob<T> {
    pub fn new() -> Self {
        Self { weak: std::sync::Weak::new(), id: 0 }
    }

    /// Try to upgrade back to a strong Blob. Returns None if the data was freed.
    pub fn upgrade(&self) -> Option<Blob<T>> {
        self.weak.upgrade().map(|data| Blob { data, id: self.id })
    }

    pub fn strong_count(&self) -> usize { self.weak.strong_count() }
}

impl<T> Default for WeakBlob<T> { fn default() -> Self { Self::new() } }
