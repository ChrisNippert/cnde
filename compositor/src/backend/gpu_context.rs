//! A GL context that survives a GPU reset, and the means to notice one.
//!
//! When the kernel resets the GPU (a game faulted it, say), every GL context
//! is told it was lost. Mesa's driver then calls abort() for any context that
//! was not created asking to be told about resets, which killed the whole
//! session. A context created with a "lose context on reset" strategy gets
//! errors instead, and `glGetGraphicsResetStatus` says what happened, so the
//! compositor can build a new renderer and carry on.
//!
//! Smithay's `EGLContext` constructors have no way to ask for this, so the
//! context is created here with `eglCreateContext` and handed to smithay with
//! `EGLContext::from_raw`.

use std::ffi::c_void;

use smithay::backend::{
    egl::{
        EGLContext, EGLDisplay,
        context::{GlAttributes, PixelFormatRequirements},
        ffi,
    },
    renderer::gles::GlesRenderer,
};
use tracing::{info, warn};

// EGL_EXT_create_context_robustness (the same values the GLES/KHR flavours
// use for the strategy value). The generated bindings do not export them.
const CONTEXT_OPENGL_ROBUST_ACCESS_EXT: i32 = 0x30BF;
const CONTEXT_OPENGL_RESET_NOTIFICATION_STRATEGY_EXT: i32 = 0x3138;
const CONTEXT_OPENGL_RESET_NOTIFICATION_STRATEGY_KHR: i32 = 0x31BD;
const LOSE_CONTEXT_ON_RESET: i32 = 0x31BF;

/// What `glGetGraphicsResetStatus` reports (GL_KHR_robustness).
const GUILTY_CONTEXT_RESET: u32 = 0x8253;
const INNOCENT_CONTEXT_RESET: u32 = 0x8254;
const UNKNOWN_CONTEXT_RESET: u32 = 0x8255;

/// How the context was created, for the log and for deciding whether a reset
/// can be seen at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Robustness {
    /// Created asking to lose the context on reset: a reset is reported to us.
    LoseOnReset,
    /// Smithay's plain context. A reset aborts the process inside Mesa.
    None,
}

/// Make a context on `display`, asking to lose it on a GPU reset, falling
/// back to smithay's own (non-robust) context when the extension is missing
/// or the driver refuses.
pub fn create_context(
    display: &EGLDisplay,
) -> Result<(EGLContext, Robustness), smithay::backend::egl::Error> {
    match robust_context(display) {
        Some(context) => Ok((context, Robustness::LoseOnReset)),
        None => {
            warn!("no reset-safe GL context available; a GPU reset will end the session");
            let context = EGLContext::new_with_priority(
                display,
                smithay::backend::egl::context::ContextPriority::High,
            )?;
            Ok((context, Robustness::None))
        }
    }
}

fn robust_context(display: &EGLDisplay) -> Option<EGLContext> {
    let has = |name: &str| display.extensions().iter().any(|e| e == name);
    let strategy = if has("EGL_EXT_create_context_robustness") {
        CONTEXT_OPENGL_RESET_NOTIFICATION_STRATEGY_EXT
    } else if display.get_egl_version() >= (1, 5) || has("EGL_KHR_create_context") {
        CONTEXT_OPENGL_RESET_NOTIFICATION_STRATEGY_KHR
    } else {
        return None;
    };

    // The context needs a real config (smithay's `from_raw` insists on one),
    // so pick the usual 8-bit one; smithay's own context is configless.
    let attributes = GlAttributes {
        version: (2, 0),
        profile: None,
        debug: false,
        vsync: false,
    };
    let (_, config) = display
        .choose_config(attributes, PixelFormatRequirements::_8_bit())
        .ok()?;

    // Same as the plain context: GLES 2 with high priority when the driver
    // has it. Robust buffer access first, then without, in case the driver
    // only allows the reset notification.
    let priority = has("EGL_IMG_context_priority");
    for robust_access in [true, false] {
        let mut attribs: Vec<i32> = vec![ffi::egl::CONTEXT_CLIENT_VERSION as i32, 2];
        if priority {
            attribs.push(ffi::egl::CONTEXT_PRIORITY_LEVEL_IMG as i32);
            attribs.push(ffi::egl::CONTEXT_PRIORITY_HIGH_IMG as i32);
        }
        attribs.push(strategy);
        attribs.push(LOSE_CONTEXT_ON_RESET);
        if robust_access {
            attribs.push(CONTEXT_OPENGL_ROBUST_ACCESS_EXT);
            attribs.push(ffi::egl::TRUE as i32);
        }
        attribs.push(ffi::egl::NONE as i32);

        // Safety: the display handle is live for as long as `display` is, and
        // the attribute list ends with EGL_NONE.
        let raw = unsafe {
            ffi::egl::CreateContext(
                **display.get_display_handle(),
                config,
                ffi::egl::NO_CONTEXT,
                attribs.as_ptr(),
            )
        };
        if raw.is_null() {
            continue;
        }
        // Safety: the context, config and display are valid, and the caller
        // keeps `display` alive (it terminates the EGL display when dropped,
        // the one `from_raw` makes does not).
        match unsafe { EGLContext::from_raw(**display.get_display_handle(), config, raw) } {
            Ok(context) => {
                info!(robust_access, "created a reset-safe GL context");
                return Some(context);
            }
            Err(err) => {
                warn!(?err, "could not wrap the reset-safe GL context");
                // Safety: we just created `raw` and nothing else owns it.
                unsafe { ffi::egl::DestroyContext(**display.get_display_handle(), raw) };
                return None;
            }
        }
    }
    None
}

/// Free a context made by [`create_context`]. Smithay does not destroy
/// contexts it was handed with `from_raw`, so this must run after the
/// renderer holding it is dropped, with the handle read before that.
pub fn destroy_context(display: &EGLDisplay, handle: *const c_void) {
    // Safety: `handle` came from `get_context_handle` of a context that is no
    // longer in use; EGL defers the destruction if it is still current.
    unsafe { ffi::egl::DestroyContext(**display.get_display_handle(), handle) };
}

/// Whether the GPU was reset under this renderer: `None` if not, or if the
/// driver can't say.
pub fn reset_status(renderer: &mut GlesRenderer) -> Option<ResetStatus> {
    renderer.with_context(|_| status_now()).ok().flatten()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetStatus {
    /// This context's work caused the reset.
    Guilty,
    /// Another context caused it, ours was lost anyway.
    Innocent,
    Unknown,
}

/// Call `glGetGraphicsResetStatus` on the current context. It is not in
/// smithay's bindings, so it is looked up by name.
fn status_now() -> Option<ResetStatus> {
    type Fn = unsafe extern "system" fn() -> u32;
    let names: [&[u8]; 3] = [
        b"glGetGraphicsResetStatusKHR\0",
        b"glGetGraphicsResetStatus\0",
        b"glGetGraphicsResetStatusEXT\0",
    ];
    for name in names {
        // Safety: the name is NUL terminated.
        let ptr = unsafe { ffi::egl::GetProcAddress(name.as_ptr().cast()) };
        if ptr.is_null() {
            continue;
        }
        // Safety: all three entry points have this signature.
        let func: Fn = unsafe { std::mem::transmute(ptr) };
        return match unsafe { func() } {
            GUILTY_CONTEXT_RESET => Some(ResetStatus::Guilty),
            INNOCENT_CONTEXT_RESET => Some(ResetStatus::Innocent),
            UNKNOWN_CONTEXT_RESET => Some(ResetStatus::Unknown),
            _ => None,
        };
    }
    None
}
