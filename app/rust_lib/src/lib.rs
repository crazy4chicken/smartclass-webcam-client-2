pub mod api;
mod frb_generated;

/// The native texture path: the app's Apple preview plugin paints the camera's own buffers, so a
/// preview costs no conversion and no copy at all.
///
/// The glue is a C ABI rather than the bridge on purpose. The plugin runs in this process, the
/// buffer never crosses into Dart, and the FRB surface would only add a copy and a Dart object to
/// every frame — the plugin's texture asks [`scwc_preview_latest_frame`] for the newest retained
/// buffer and is told when there is one through the callback it registers.
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod preview_texture {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::Mutex;

    /// The plugin's "a new frame is ready" callback, or `None` while no texture is registered.
    static FRAME_READY: Mutex<Option<extern "C" fn()>> = Mutex::new(None);

    /// Tells the registered plugin that the newest buffer is ready to be painted.
    fn announce() {
        let callback = *FRAME_READY.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(callback) = callback {
            callback();
        }
    }

    /// Registers the callback the capture path tells when a new frame is ready, or clears it.
    ///
    /// # Safety
    ///
    /// `callback` is either null — which stops the path and releases the frame it was holding —
    /// or a function that stays alive while it is registered. It is called from the capture
    /// thread, once per captured frame, so it must return promptly.
    #[no_mangle]
    pub unsafe extern "C" fn scwc_preview_register_frame_callback(
        callback: Option<extern "C" fn()>,
    ) {
        *FRAME_READY.lock().unwrap_or_else(|error| error.into_inner()) = callback;
        if callback.is_none() {
            // Stop listening before the last frame goes: a texture that is registered again
            // starts from the frames captured after it, not from one painted before it.
            scwc_core::preview::set_native_frame_listener(None);
            if let Some(frame) = scwc_core::preview::take_native_frame() {
                // SAFETY: the slot handed over the one reference it owned, and the registered
                // path is the only other holder, which has just been cleared.
                unsafe { scwc_capture::release_frame_buffer(frame) };
            }
        } else {
            scwc_core::preview::set_native_frame_listener(Some(announce));
            // The two paths are alternatives, and this one is the cheaper: a consumer that takes
            // platform buffers wants no frames converted for it, so the stream path is switched
            // off here rather than left to convert every frame nobody reads. A later layout from
            // a client that shows the stream anyway sets a box again and re-enables it.
            scwc_core::preview::set_preview_target_size(0, 0);
        }
    }

    /// The newest camera frame as a retained platform buffer, or null while there is none.
    ///
    /// # Safety
    ///
    /// The answer is either null or a `CVPixelBufferRef` with one reference for the caller to
    /// release; the pointer stays valid until that release, and is already null-safe.
    #[no_mangle]
    pub unsafe extern "C" fn scwc_preview_latest_frame() -> *mut c_void {
        let frame = scwc_core::preview::native_frame();
        if frame.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: the slot holds a live reference to this buffer, so retaining it gives the
        // caller one of its own.
        unsafe { scwc_capture::retain_frame_buffer(frame) }
    }
}

/// The pixel-buffer texture path: the app's Linux and Windows preview plugins paint the frames the
/// capture path already converted, so a preview costs neither a bridge hop nor a Dart decode.
///
/// The same shape as the Apple path above, and the same glue: a C ABI rather than the bridge,
/// because the plugin runs in this process and the frame never crosses into Dart. What differs is
/// the payload — the plugin here paints plain pixels, so it is handed the finished RGBA frame the
/// capture path published for it, scaled to the size the interface asked for, and it owns that frame
/// until the next call replaces it.
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod pixel_buffer_texture {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::{Mutex, MutexGuard};

    use scwc_core::preview::{PreviewFrame, RgbaFrameConsumer};

    /// The plugin's "a new frame is ready" callback, or `None` while no texture is registered.
    static FRAME_READY: Mutex<Option<extern "C" fn()>> = Mutex::new(None);

    /// The frame the plugin is painting, held alive until it releases it.
    static PAINTED: Mutex<Option<Painted>> = Mutex::new(None);

    /// One frame handed to the plugin, with the pointer it was handed as.
    struct Painted {
        /// The pointer the plugin paints from and hands back to release the frame.
        ///
        /// An address rather than a pointer so that the slot itself stays `Send`: it is only ever
        /// compared here, never followed.
        pixels: usize,
        /// The frame, held for as long as the plugin may be painting it.
        frame: Box<PreviewFrame>,
    }

    /// Tells the registered plugin that the newest frame is ready to be painted.
    fn announce() {
        let callback = *FRAME_READY.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(callback) = callback {
            callback();
        }
    }

    /// Frees one frame the capture path published for a texture.
    fn release(frame: *mut c_void) {
        // SAFETY: the slot hands over the frames it published, which are the boxes this module made
        // in `scwc_preview_latest_rgba`, and it hands each of them over once.
        drop(unsafe { Box::from_raw(frame.cast::<PreviewFrame>()) });
    }

    /// Registers the callback the capture path tells when a new frame is ready, or clears it.
    ///
    /// # Safety
    ///
    /// `callback` is either null — which stops the path and frees the frame it was holding — or a
    /// function that stays alive while it is registered. It is called from the capture thread, once
    /// per captured frame, so it must return promptly.
    #[no_mangle]
    pub unsafe extern "C" fn scwc_preview_register_rgba_callback(
        callback: Option<extern "C" fn()>,
    ) {
        *FRAME_READY.lock().unwrap_or_else(|error| error.into_inner()) = callback;
        if callback.is_none() {
            // The frame the plugin was painting goes with the texture: it may not paint it again,
            // and a texture that is registered again starts from the frames captured after it.
            *painted_slot() = None;
        }
        // The texture path is not exclusive of the stream: both are the capture path's own frames,
        // and what the layout sizes is what either of them shows, so the target size the interface
        // set stays as it is.
        scwc_core::preview::set_rgba_frame_listener(callback.map(|_| RgbaFrameConsumer {
            ready: announce,
            release,
        }));
    }

    /// The newest camera frame as packed RGBA, or null while none has been captured.
    ///
    /// # Safety
    ///
    /// The answer is either null or the pixels of a frame this call takes ownership of: `width` and
    /// `height`, when they are not null, are written that frame's dimensions, and the pixels stay
    /// valid until they are handed back to [`scwc_preview_release_rgba`]. The caller releases the
    /// frame it was holding before it asks for another one, so nothing is freed while it paints.
    #[no_mangle]
    pub unsafe extern "C" fn scwc_preview_latest_rgba(width: *mut u32, height: *mut u32) -> *const u8 {
        let Some(frame) = scwc_core::preview::take_rgba_frame() else {
            return ptr::null();
        };
        // SAFETY: the slot handed over the one reference it held, and it is a box this module made.
        let frame = unsafe { Box::from_raw(frame.cast::<PreviewFrame>()) };
        let pixels = frame.rgba.as_ptr();
        if !width.is_null() {
            // SAFETY: the caller passes an out parameter it can write through.
            unsafe { width.write(frame.width) };
        }
        if !height.is_null() {
            // SAFETY: as above.
            unsafe { height.write(frame.height) };
        }
        // A plugin that asks for a newer frame while it still holds one is not painting what the
        // slot holds, and the two have nothing to do with each other: the older frame goes rather
        // than stacking up behind the newer one.
        *painted_slot() = Some(Painted {
            pixels: pixels as usize,
            frame,
        });
        pixels
    }

    /// Releases one frame handed out by [`scwc_preview_latest_rgba`].
    ///
    /// # Safety
    ///
    /// `pixels` is either null or the pointer of the frame the last call to
    /// [`scwc_preview_latest_rgba`] that returned one handed out, and that frame has not been
    /// released yet. A pointer to anything else is ignored, so a caller that lost track of which
    /// frame it holds cannot free a frame out from under the slot.
    #[no_mangle]
    pub unsafe extern "C" fn scwc_preview_release_rgba(pixels: *const u8) {
        let mut painted = painted_slot();
        if painted.as_ref().is_some_and(|held| held.pixels == pixels as usize) {
            *painted = None;
        }
    }

    /// The slot holding the frame the plugin is painting, a poisoned lock treated as an empty one.
    fn painted_slot() -> MutexGuard<'static, Option<Painted>> {
        PAINTED.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// The surface path: the Android preview plugin hands the camera its own texture to render into.
///
/// The same shape as the two paths above and the same glue — a C ABI rather than the bridge,
/// because the plugin runs in this process — with one difference that is the whole point: the
/// only thing that ever crosses is a `Surface`, handed over once when the engine attaches. The
/// camera then renders into the texture with every capture request, so the preview costs no
/// conversion, no copy and no per-frame notice at all: the embedding engine owns the texture and
/// the listener that tells it a newer frame is waiting.
///
/// The names are JNI's, for `com.flutter_rust_bridge.rust_lib.ScwcPreviewPlugin`; every `_` of a
/// package or class name becomes `_1` in the mangled symbol, so a class or package that moves
/// renames every function below.
#[cfg(target_os = "android")]
mod surface_preview {
    use std::ffi::c_void;

    /// Hands the plugin's `Surface` over as the window the camera previews into.
    ///
    /// Called once, when the engine attaches. A window that was handed over before loses its
    /// reference here; a stream that leased it keeps rendering into it until it is torn down.
    ///
    /// # Safety
    ///
    /// `env` is this call's `JNIEnv*` and `surface` a live `android.view.Surface` reference; both
    /// are used for the duration of this call alone.
    #[no_mangle]
    pub unsafe extern "C" fn Java_com_flutter_1rust_1bridge_rust_1lib_ScwcPreviewPlugin_nativeAttachSurface(
        env: *mut c_void,
        _this: *mut c_void,
        surface: *mut c_void,
    ) {
        // SAFETY: the plugin called this from a JNI method that owns both arguments for this call.
        unsafe { scwc_capture::preview_attach_surface(env, surface) };
    }

    /// Gives the window back: nothing is previewed into it, and the next stream that is built
    /// previews from a YUV reader of its own.
    #[no_mangle]
    pub extern "C" fn Java_com_flutter_1rust_1bridge_rust_1lib_ScwcPreviewPlugin_nativeDetachSurface(
        _env: *mut c_void,
        _this: *mut c_void,
    ) {
        scwc_capture::preview_detach_surface();
    }

    /// Whether the camera is rendering into that window right now.
    ///
    /// `false` means the interface's texture is not the picture: a camera that refused the output
    /// combination previews from a reader, and the plugin has to say so rather than hand out an
    /// id for a black box.
    #[no_mangle]
    pub extern "C" fn Java_com_flutter_1rust_1bridge_rust_1lib_ScwcPreviewPlugin_nativePreviewActive(
        _env: *mut c_void,
        _this: *mut c_void,
    ) -> u8 {
        u8::from(scwc_capture::preview_external_active())
    }

    /// The pixel size the camera renders into the window as `width << 32 | height`, or 0 while
    /// none is known yet.
    ///
    /// Packed rather than answered as an array because answering with one would mean calling back
    /// into JNI to build it, for two numbers.
    #[no_mangle]
    pub extern "C" fn Java_com_flutter_1rust_1bridge_rust_1lib_ScwcPreviewPlugin_nativePreviewSize(
        _env: *mut c_void,
        _this: *mut c_void,
    ) -> i64 {
        match scwc_capture::preview_size_pixels() {
            Some((width, height)) => (i64::from(width) << 32) | i64::from(height),
            None => 0,
        }
    }

    /// The clockwise degrees the picture has to be turned by to stand up: 0, 90, 180 or 270.
    ///
    /// A sensor's orientation is a property of the camera and not of its buffers, so the
    /// interface is told it rather than reading it out of a frame.
    #[no_mangle]
    pub extern "C" fn Java_com_flutter_1rust_1bridge_rust_1lib_ScwcPreviewPlugin_nativePreviewRotation(
        _env: *mut c_void,
        _this: *mut c_void,
    ) -> i32 {
        i32::from(scwc_capture::preview_rotation_degrees())
    }
}

/// Installs the process-wide rustls crypto provider the `wss://` attach needs.
///
/// rustls 0.23 builds a TLS configuration only when it can tell which crypto provider to use,
/// and it cannot tell when the crate was compiled with no provider feature at all — which is
/// how `tokio-tungstenite` and `tungstenite` take their rustls dependency, and therefore how
/// the device WebSocket would get it. `scwc-core` selects `ring` for that reason (see its
/// `Cargo.toml`); installing it here makes the choice explicit and independent of every other
/// crate in the graph, so the first `wss://` attach cannot panic on it.
///
/// Called from the bridge's own init, and idempotent: an install that finds a provider already
/// in place — a second init in a test, an application that calls this itself — keeps the first
/// one and is not an error.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
