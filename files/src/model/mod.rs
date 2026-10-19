//! The file manager's data layer: listing, drives, recents, trash, and the
//! small text helpers the UI needs. No UI code lives here.

// Parts of the model the window doesn't use yet (Overview, search, trash).
#![allow(dead_code, unused_imports)]

mod drives;
mod format;
mod fuzzy;
mod listing;
mod recent;
mod trash;

pub use drives::{Drive, drives, eject};
pub use format::{human_size, human_time};
pub use fuzzy::fuzzy_score;
pub use listing::{Entry, SortKey, dir_size, kind_of, list_dir, sort};
pub use recent::{Recent, recent};
pub use trash::{trash, trash_dir, trash_size};
