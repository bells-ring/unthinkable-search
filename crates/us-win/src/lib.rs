//! us-win: Windows I/O for unthinkable-search (volumes, MFT, USN, directory walk).
#![cfg(windows)]

pub mod mft;
pub mod usn;
pub mod util;
pub mod volumes;
pub mod walk;
