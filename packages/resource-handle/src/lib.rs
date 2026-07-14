// Patched linebender-resource-handle for kei compatibility.
// Replaces Arc<dyn AsRef<[T]>> with Arc<Vec<T>> to avoid vtable NULL.

#![allow(unused)]

mod blob;
mod font;

pub use blob::{Blob, WeakBlob};
pub use font::FontData;
