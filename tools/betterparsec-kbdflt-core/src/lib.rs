//! Pure, platform-independent installer logic (UpperFilters ordering and
//! REG_MULTI_SZ encoding). Kept in its own crate — with no build script —
//! so its unit tests run without the installer bin's requireAdministrator
//! manifest, which would otherwise force elevation on the test harness.

pub mod multi_sz;
pub mod upperfilters;
