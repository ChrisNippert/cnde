//! Backends (ARCHITECTURE.md §4.1). Milestone 1 only has winit (nested);
//! udev/DRM arrives in Milestone 2, at which point `State::backend` becomes an enum.

pub mod winit;
