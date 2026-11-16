//! API level of the runtime.
//!
//! Items whose signature changed between API levels live in a versioned module; the current
//! level's module is re-exported so callers do not need to name it.

/// The API level this crate implements.
pub const ENGINE_API_LEVEL: u32 = 7;

/// Items of API level 7 (the current level, re-exported at this module's root).
pub mod api_v7 {
    /// Items introduced at level 7 and kept by later levels (currently none).
    pub mod and_newer {}
    #[allow(unused_imports)]
    pub use and_newer::*;
}
pub use api_v7::*;
