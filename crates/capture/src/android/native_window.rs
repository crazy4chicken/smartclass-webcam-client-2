//! The `ANativeWindow` handle the camera writes into and the encoder reads from.
//!
//! Both halves of the Android pipeline name this type — the capture session wraps a window in an
//! output, and the encoder hands out the window its codec was configured with — so the opaque
//! type lives here rather than in either of them. Two separate declarations would be the same C
//! object in two incompatible Rust types, which is exactly the kind of mismatch that only shows
//! up as a confusing build error after both sides are written.
//!
//! The reference count is not a Rust `Clone`. `AMediaCodec_createInputSurface` hands the caller
//! one reference, and its header is explicit that the application releases it; the encoder owns
//! that one for as long as it lives (see [`super::media::Encoder::stop`]). The camera session
//! only borrows the pointer, and must not release it.
//!
//! A window can also come from the application rather than from a codec: [`from_surface`] takes
//! the one behind a Java `Surface`, which is how the interface's own texture becomes a place the
//! camera renders into. Whoever holds a window holds a reference on it, and hands that reference
//! back once; see [`super::preview_surface`] for the slot that keeps the application's.

use std::ffi::c_void;

/// Opaque `ANativeWindow`.
#[repr(C)]
pub(crate) struct ANativeWindow {
    _private: [u8; 0],
}

// `native_window.h` declares these in `libandroid`, which the media stack's `libnativewindow`
// re-exports; one link name is enough. Every function below is available from the platform's
// first API level — none of them carries an availability annotation in the NDK's header — which
// is what makes them safe to name here: the Android toolchain links with `BIND_NOW`, so a symbol
// introduced later than this library's minimum SDK keeps the whole library from loading on an
// older device. [`ANativeWindow_setBuffersTransform`] is one such symbol (API 26) and is
// deliberately not declared.
#[link(name = "android")]
unsafe extern "C" {
    fn ANativeWindow_release(window: *mut ANativeWindow);
    /// `native_window_jni.h`: the window behind a Java `Surface`, with one reference taken for
    /// the caller, or null when the platform has no window for it.
    fn ANativeWindow_fromSurface(env: *mut c_void, surface: *mut c_void) -> *mut ANativeWindow;
    fn ANativeWindow_acquire(window: *mut ANativeWindow);
    /// The pixel size the producer renders into the window; `0` for `width` or `height` means
    /// "leave that side alone". Negative on failure.
    fn ANativeWindow_setBuffersGeometry(
        window: *mut ANativeWindow,
        width: i32,
        height: i32,
        format: i32,
    ) -> i32;
    fn ANativeWindow_getWidth(window: *const ANativeWindow) -> i32;
    fn ANativeWindow_getHeight(window: *const ANativeWindow) -> i32;
}

/// Drops one reference on a window.
///
/// # Safety
///
/// `window` must be a live `ANativeWindow*`, and the caller must own a reference on it — either
/// the one `AMediaCodec_createInputSurface` handed over, the one
/// [`from_surface`](Self::from_surface) hands over, or one taken with
/// [`acquire`](Self::acquire). Releasing a reference twice, or releasing one the caller does not
/// own, frees a surface that is still in use.
pub(crate) unsafe fn release(window: *mut ANativeWindow) {
    // SAFETY: the caller guarantees the pointer is live and that it owns one reference.
    unsafe { ANativeWindow_release(window) };
}

/// The window behind a Java `Surface`, with one reference taken for the caller.
///
/// Null when the platform has no window for the surface, which is the ordinary answer for a
/// `Surface` whose producer is gone.
///
/// # Safety
///
/// `env` must be the `JNIEnv*` of the calling thread and `surface` a live `android.view.Surface`
/// reference; both are only read, for the duration of this call. The caller owns the reference
/// the answer carries and releases it with [`release`](Self::release) exactly once.
pub(crate) unsafe fn from_surface(env: *mut c_void, surface: *mut c_void) -> *mut ANativeWindow {
    // SAFETY: the caller guarantees both arguments describe a live environment and surface for
    // this call.
    unsafe { ANativeWindow_fromSurface(env, surface) }
}

/// Takes one more reference on a window, so it outlives whoever else holds one.
///
/// # Safety
///
/// `window` must be live. The caller owns the reference this adds and releases it with
/// [`release`](Self::release) exactly once.
pub(crate) unsafe fn acquire(window: *mut ANativeWindow) {
    // SAFETY: the caller guarantees the pointer is live.
    unsafe { ANativeWindow_acquire(window) };
}

/// Sets the pixel size of the buffers the producer renders into `window`.
///
/// The camera stretches its crop region to fill the window it is given without keeping the
/// picture's aspect ratio, so the size set here is the one the picture is rendered at — not a
/// hint. `0` on a side leaves that side as it was.
///
/// Negative when the platform refuses the geometry, which is a property of the window rather
/// than of the caller: a caller that is told no falls back to another way of previewing.
///
/// # Safety
///
/// `window` must be live, and **no capture session may be rendering into it**: the geometry of a
/// window a session owns cannot change. Set it before the session is built and not after.
pub(crate) unsafe fn set_buffers_geometry(
    window: *mut ANativeWindow,
    width: u32,
    height: u32,
) -> i32 {
    // `0` for the format means "do not change the format", which is what a caller that only
    // wants the size wants — the format is the producer's business.
    const KEEP_FORMAT: i32 = 0;
    // SAFETY: the caller guarantees the pointer is live and that no session renders into it.
    unsafe { ANativeWindow_setBuffersGeometry(window, width as i32, height as i32, KEEP_FORMAT) }
}

/// The size the window's producer renders at, as the platform reports it back, or `None` when it
/// reports none.
///
/// Read back rather than remembered because the platform rounds what it is given: the size that
/// was asked for is not always the size that is in effect.
///
/// # Safety
///
/// `window` must be live.
pub(crate) unsafe fn size(window: *const ANativeWindow) -> Option<(u32, u32)> {
    // SAFETY: the caller guarantees the pointer is live; the platform only reads from it.
    let width = unsafe { ANativeWindow_getWidth(window) };
    let height = unsafe { ANativeWindow_getHeight(window) };
    (width > 0 && height > 0).then_some((width as u32, height as u32))
}
