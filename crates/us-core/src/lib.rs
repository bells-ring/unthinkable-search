//! us-core: platform-independent heart of unthinkable-search.
//! Index layout, search, sorting, NTFS parsing and the on-disk format.

pub mod alloc;
pub mod fold;
pub mod index;
pub mod live;
pub mod ntfs;
pub mod query;
pub mod scan;
pub mod search;
pub mod sort;
pub mod store;

pub use index::{build, Index};
pub use query::Query;
pub use scan::{VolumeMeta, VolumeScan};
