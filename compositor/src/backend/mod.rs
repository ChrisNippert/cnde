//! Backends (ARCHITECTURE.md §4.1): winit runs nested inside another
//! desktop for development; udev drives real hardware from a TTY.

pub mod udev;
pub mod winit;

use smithay::{
    backend::{allocator::dmabuf::Dmabuf, renderer::gles::GlesRenderer},
    wayland::dmabuf::DmabufState,
};

pub enum Backend {
    Winit(winit::WinitBackend),
    Udev(udev::UdevBackend),
}

impl Backend {
    /// The renderer used for offscreen work (snapshots). `None` while the
    /// udev backend has no GPU (e.g. before the first device is opened).
    pub fn renderer(&mut self) -> Option<&mut GlesRenderer> {
        match self {
            Backend::Winit(winit) => Some(winit.renderer()),
            Backend::Udev(udev) => udev.renderer(),
        }
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        match self {
            Backend::Winit(winit) => winit.import_dmabuf(dmabuf),
            Backend::Udev(udev) => udev.import_dmabuf(dmabuf),
        }
    }

    pub fn dmabuf_state(&mut self) -> &mut DmabufState {
        match self {
            Backend::Winit(winit) => &mut winit.dmabuf_state,
            Backend::Udev(udev) => &mut udev.dmabuf_state,
        }
    }

    /// Whether we draw the pointer ourselves (nested, the host does).
    pub fn draws_cursor(&self) -> bool {
        matches!(self, Backend::Udev(_))
    }
}
