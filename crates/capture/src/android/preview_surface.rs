//! The application's own preview window: a `Surface` the interface hands over for the camera to
//! render into.
//!
//! A capture session's outputs are fixed when it is built, so the window has to be known before
//! a stream is brought up — but the interface's texture is created by the embedding engine on its
//! own schedule, which is why the window is kept here, in a slot, rather than passed down as an
//! argument. A stream that is built takes a lease on whatever the slot holds at that moment, and
//! builds its session with that window as the preview output; a stream built while the slot is
//! empty previews from a YUV reader of its own, exactly as this backend always has.
//!
//! # Why this is worth a slot
//!
//! Rendering into the application's window costs nothing per frame: the camera writes into a
//! buffer the platform already owns, the engine takes it as an external texture, and neither a
//! conversion nor a copy nor a notice crosses into Rust. The frame-available listener that makes
//! the engine pick a buffer up is installed by the engine's own texture entry, so attaching the
//! window is the whole of the work — nothing in this module runs per frame.
//!
//! # References
//!
//! The slot owns exactly one reference on the window it names, taken by
//! [`native_window::from_surface`]. A lease takes a second reference, under the slot's lock, so
//! [`detach`] can never release a window a running session is still rendering into: the window is
//! freed when the last reference goes, which is after both the slot and the stream are done with
//! it.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use tracing::{debug, info, warn};

use super::native_window::{self, ANativeWindow};
use crate::error::CaptureError;

/// The window the application handed over, and the size the camera renders into it.
struct External {
    /// The window, with the one reference this slot owns.
    window: *mut ANativeWindow,
}

/// The application's window, or `None` while it handed none over.
static EXTERNAL: Mutex<Option<External>> = Mutex::new(None);

// SAFETY: a window is a reference-counted handle the platform owns: this module only names it,
// takes and gives back references on it, and never dereferences it. Every one of those happens
// under the slot's lock or on the capture thread alone, which is what makes a `static` of one
// sound — the same reasoning [`super::preview::PreviewCapture`] and `Lease` below both rest on.
unsafe impl Send for External {}
unsafe impl Sync for External {}

/// Whether the running stream renders into the application's window.
///
/// The interface asks this to know whether the texture it registered is the picture or a black
/// box: a camera that refused the output combination leaves this `false` while the preview comes
/// from a YUV reader.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Clockwise degrees the picture has to be turned by to stand up: 0, 90, 180 or 270.
///
/// A sensor's orientation is a static property of the camera, not something its buffers carry, so
/// the interface is told it rather than reading it out of a frame.
static ROTATION: AtomicU16 = AtomicU16::new(0);

/// The width the camera renders into the window, mirrored out of the slot for the interface.
static WIDTH: AtomicU32 = AtomicU32::new(0);

/// The height the camera renders into the window, mirrored out of the slot for the interface.
static HEIGHT: AtomicU32 = AtomicU32::new(0);

/// Hands the application's `Surface` over as the window the camera previews into.
///
/// A window that was already handed over loses its reference here: the slot names one window at a
/// time. A stream that leased the previous one owns a reference of its own, so nothing rendering
/// into it is left with a freed window.
///
/// # Safety
///
/// `env` is the `JNIEnv*` of the calling thread and `surface` a live `android.view.Surface`
/// reference, both valid for this call alone.
pub unsafe fn attach(env: *mut c_void, surface: *mut c_void) {
    // SAFETY: the caller guarantees both arguments describe a live environment and surface for
    // this call, and owns the reference the answer carries.
    let window = unsafe { native_window::from_surface(env, surface) };
    if window.is_null() {
        warn!("the interface handed over no window to preview into");
        return;
    }
    let previous = slot().replace(External { window });
    if let Some(previous) = previous {
        // SAFETY: the slot owned exactly one reference on the window it named, and it has just
        // stopped naming it; a stream that leased it owns a reference of its own.
        unsafe { native_window::release(previous.window) };
    }
    // A new window is not yet part of any session, so nothing renders into it for now.
    ACTIVE.store(false, Ordering::Release);
    WIDTH.store(0, Ordering::Release);
    HEIGHT.store(0, Ordering::Release);
    info!("the interface's preview window is attached");
}

/// Gives the window back: nothing is previewed into it, and the next stream that is built
/// previews from a YUV reader of its own.
///
/// Safe at any time, including while a stream renders into the window — that stream holds a
/// reference of its own and keeps rendering until it is torn down.
pub fn detach() {
    if let Some(previous) = slot().take() {
        // SAFETY: the slot owned exactly one reference on the window it named, and it has just
        // stopped naming it; a running stream's lease is its own reference.
        unsafe { native_window::release(previous.window) };
    }
    ACTIVE.store(false, Ordering::Release);
    WIDTH.store(0, Ordering::Release);
    HEIGHT.store(0, Ordering::Release);
    debug!("the interface's preview window is detached");
}

/// The application's window, with a reference of this stream's own, or `None` while none was
/// handed over.
///
/// The reference is taken under the slot's lock, which is what makes [`detach`] safe while a
/// stream is running: the slot cannot release the window it names between this look and the
/// acquire.
pub(crate) fn lease() -> Option<Lease> {
    let slot = slot();
    let external = slot.as_ref()?;
    // SAFETY: the slot holds one reference for as long as it names the window, and the lock is
    // held across the acquire, so the window cannot go away underneath this call.
    unsafe { native_window::acquire(external.window) };
    Some(Lease {
        window: external.window,
    })
}

/// Whether the camera is rendering into the application's window right now.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Records whether the running stream renders into the application's window.
pub(crate) fn set_active(active: bool) {
    ACTIVE.store(active, Ordering::Release);
}

/// Records the clockwise turn the picture needs, for the interface to ask about.
pub(crate) fn set_rotation(degrees: u16) {
    ROTATION.store(degrees, Ordering::Release);
}

/// The clockwise turn the picture has to be turned by to stand up: 0, 90, 180 or 270.
pub fn rotation() -> u16 {
    ROTATION.load(Ordering::Acquire)
}

/// The size of the picture the interface shows, or `None` while none has been rendered yet.
///
/// The size the platform reports back rather than the one that was asked for: windows round. It
/// is the *shown* size — the buffer's sides exchanged when the sensor needs a quarter turn —
/// because the picture the engine samples out of the texture is already upright: the camera
/// writes its sensor's own orientation into the window, the buffer's transform matrix turns it
/// back, and what the interface sees is the turned picture. Reporting the buffer's own sides
/// here would have the interface letterbox a portrait picture in a landscape box.
pub fn size() -> Option<(u32, u32)> {
    let width = WIDTH.load(Ordering::Acquire);
    let height = HEIGHT.load(Ordering::Acquire);
    if width == 0 || height == 0 {
        return None;
    }
    Some(match rotation() {
        90 | 270 => (height, width),
        _ => (width, height),
    })
}

/// One stream's use of the application's window.
///
/// Holds the reference the stream acquired, so the window outlives the slot that first named it
/// and is released exactly once, when the stream is done with it.
pub(crate) struct Lease {
    /// The window; the lease owns one reference on it.
    window: *mut ANativeWindow,
}

impl Lease {
    /// Sets the pixel size the camera renders into the window.
    ///
    /// Fails with [`CaptureError::UnsupportedParameters`] when the platform refuses the
    /// geometry — a property of the window rather than a caller's mistake, which is why the
    /// answer is an error and not a panic: the caller drops the lease and previews another way.
    ///
    /// Must be called before any session is built with this window; the geometry of a window a
    /// session owns cannot change.
    pub(crate) fn set_size(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        // SAFETY: this lease owns a reference on the window, so it is live, and no session
        // renders into it yet — the only state in which its geometry may change.
        let status = unsafe { native_window::set_buffers_geometry(self.window, width, height) };
        if status < 0 {
            return Err(CaptureError::Os(format!(
                "the preview window refused {width}x{height} (status {status})"
            )));
        }
        // SAFETY: as above.
        let (width, height) = unsafe { native_window::size(self.window) }.ok_or_else(|| {
            CaptureError::Os("the preview window reports no size to render at".to_owned())
        })?;
        WIDTH.store(width, Ordering::Release);
        HEIGHT.store(height, Ordering::Release);
        Ok(())
    }

    /// The window, for the capture session to add as an output.
    pub(crate) fn window(&self) -> *mut ANativeWindow {
        self.window
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // SAFETY: this lease acquired exactly one reference in `lease` and gives it back once.
        unsafe { native_window::release(self.window) };
    }
}

// SAFETY: a lease is created, used and dropped on the capture thread alone, which is the only
// thread that ever renders into the window through a session; the pointer is never dereferenced
// in Rust.
unsafe impl Send for Lease {}

/// The slot, with a poisoned lock treated as an empty one.
fn slot() -> MutexGuard<'static, Option<External>> {
    EXTERNAL.lock().unwrap_or_else(|error| error.into_inner())
}
