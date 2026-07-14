//! aris-parley — minimal FontContext facade for the aris in-tree Linebender fork.
//!
//! This crate wraps `aris-fontique` (the aris-internal fork of Linebender's
//! fontique) behind a `FontContext` struct that `blitz-dom` and `aris-render`
//! expect from the upstream `parley` crate. Only the subset needed by the
//! aris renderer is implemented; shaping and layout are handled by
//! `blitz-dom`'s own layout pipeline.

pub mod fontique {
    //! Re-exports of the key fontique API used by aris-render.

    pub use fontique::{
        Blob, Collection, CollectionOptions, SourceCache, SourceCacheOptions, Query, QueryFamily,
        QueryFont, QueryStatus, Attributes, GenericFamily,
    };
}

use fontique::{Collection, CollectionOptions, SourceCache};

/// Minimal font context used by `blitz-dom` / `aris-render` to provide font data.
///
/// This is a lightweight facade that holds a fontique `SourceCache`
/// (shared across documents) and a `Collection` (font families + attributes
/// per-document). It mirrors the field-access pattern of the upstream
/// `parley::FontContext` struct that `aris-render` relies on.
pub struct FontContext {
    pub source_cache: SourceCache,
    pub collection: Collection,
}

impl FontContext {
    /// Create a new, empty font context with no system fonts.
    pub fn new() -> Self {
        Self::with_options(CollectionOptions {
            shared: false,
            system_fonts: false,
        })
    }

    /// Create a font context with the given collection options.
    pub fn with_options(options: CollectionOptions) -> Self {
        Self {
            source_cache: SourceCache::new_shared(),
            collection: Collection::new(options),
        }
    }
}

impl Default for FontContext {
    fn default() -> Self {
        Self::new()
    }
}
