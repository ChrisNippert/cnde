//! Interactive pointer grabs: moving and resizing windows.

pub mod move_grab;
pub use move_grab::MoveSurfaceGrab;

pub mod resize_grab;
pub use resize_grab::{ResizeEdge, ResizeSurfaceGrab};
