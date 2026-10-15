//! The Camera2 capture session: the repeating request that keeps the encoder's surface filled.
//!
//! A session is the object that actually makes a camera stream. It is built from two chains
//! that both end at the encoder's input surface — a session output inside an output container,
//! and an output target inside a capture request — and every link has to outlive the one that
//! references it. The session is therefore built front to back, keeping each object as it is
//! created, and freed back to front by [`Session`]'s `Drop`; a failure halfway through leaves a
//! partially built session that the same `Drop` cleans up, rather than leaking the objects
//! created before it.
//!
//! # The still output
//!
//! `take_photo` needs a second chain on the same session: the JPEG reader's window as an
//! output, and a one-shot request built from `TEMPLATE_STILL_CAPTURE` that targets only it
//! ([`Session::capture_still`]). The repeating request is left alone while a still is in
//! flight, so the video stream keeps its cadence. The still chain is optional because the
//! stream combination is a device property: a camera that refuses the pair is rebuilt without
//! it, and the photo path then reports the refusal instead of the stream losing its video.

use std::ffi::c_void;
use std::ptr;

use tracing::{debug, info, warn};

use super::camera2::{
    self, ACameraCaptureSession, ACameraCaptureSession_stateCallbacks, ACameraOutputTarget,
    ACaptureRequest, ACaptureSessionOutput, ACaptureSessionOutputContainer,
};
use super::native_window::ANativeWindow;
use crate::error::CaptureError;

/// One running camera → encoder pipeline.
///
/// Holds the whole object chain so that teardown order is a property of the type rather than of
/// every call site.
pub(crate) struct Session {
    /// The session itself; closed first, because everything else is referenced through it.
    session: *mut ACameraCaptureSession,
    /// The repeating request that fills the surface.
    request: *mut ACaptureRequest,
    /// The request's target, which references the surface.
    target: *mut ACameraOutputTarget,
    /// The session's output, which references the same surface.
    output: *mut ACaptureSessionOutput,
    /// The one-shot request that fills the still writer's window, when one was given.
    still_request: *mut ACaptureRequest,
    /// The still request's target, which references the still window.
    still_target: *mut ACameraOutputTarget,
    /// The session's output for the still window.
    still_output: *mut ACaptureSessionOutput,
    /// The output target for the preview window, added to the repeating request.
    preview_target: *mut ACameraOutputTarget,
    /// The session's output for the preview window.
    preview_output: *mut ACaptureSessionOutput,
    /// The container the session was created from.
    container: *mut ACaptureSessionOutputContainer,
    /// The callbacks the framework keeps a pointer to; they have to outlive the session.
    callbacks: Box<ACameraCaptureSession_stateCallbacks>,
    /// Whether repeating was started, so teardown knows whether to stop it.
    repeating: bool,
}

impl Session {
    /// Builds the session for the encoder surface, plus a still output when one is given, and
    /// starts streaming into the encoder's.
    ///
    /// `window` is the encoder's input surface: the camera writes straight into it, so no frame
    /// is copied on its way to the hardware encoder. `still` is the JPEG reader's window, when
    /// the caller could bring one up; its chain is built here because a session's set of
    /// outputs is fixed at creation, and it is filled only by [`Session::capture_still`]. `fps`
    /// is pinned as the auto-exposure target range, which is what makes the cadence steady
    /// enough for the protocol's segments; whether the camera admits to that rate is the
    /// caller's business, since it is the side that read the capability matrix.
    pub(crate) fn start(
        device: &mut camera2::Device,
        window: *mut ANativeWindow,
        still: Option<*mut ANativeWindow>,
        preview: Option<*mut ANativeWindow>,
        fps: u32,
    ) -> Result<Self, CaptureError> {
        let device_ptr = device
            .as_ptr()
            .ok_or_else(|| CaptureError::Os("the camera is already closed".to_owned()))?;

        // Everything is stored in the session as it is created, so an early return from any
        // step below leaves the rest to `Drop`.
        let mut session = Self {
            session: ptr::null_mut(),
            request: ptr::null_mut(),
            target: ptr::null_mut(),
            output: ptr::null_mut(),
            still_request: ptr::null_mut(),
            still_target: ptr::null_mut(),
            still_output: ptr::null_mut(),
            preview_target: ptr::null_mut(),
            preview_output: ptr::null_mut(),
            container: ptr::null_mut(),
            callbacks: Box::new(ACameraCaptureSession_stateCallbacks {
                // No shared state yet: the pump reads frames from the encoder, and the camera
                // callbacks below only log. Wiring them to the pump is the next step, and the
                // `context` pointer stays null until there is something safe to point at.
                context: ptr::null_mut(),
                on_closed: Some(on_closed),
                on_ready: Some(on_ready),
                on_active: Some(on_active),
            }),
            repeating: false,
        };

        // The output chain: container ← output ← surface, with the still window as a second
        // output when there is one.
        let container = camera2::output_container_create()?;
        session.container = container;
        let output = camera2::output_create(window)?;
        session.output = output;
        camera2::output_container_add(container, output)?;
        if let Some(still_window) = still {
            let still_output = camera2::output_create(still_window)?;
            session.still_output = still_output;
            camera2::output_container_add(container, still_output)?;
        }
        if let Some(preview_window) = preview {
            let preview_output = camera2::output_create(preview_window)?;
            session.preview_output = preview_output;
            camera2::output_container_add(container, preview_output)?;
        }

        // The session, which is created from the container.
        // SAFETY: the device is open, the container is live and the callbacks are owned by
        // `session`, which outlives the session object they are registered with.
        let handle = unsafe {
            camera2::capture_session_create(device_ptr, container, session.callbacks.as_ref())
        }?;
        session.session = handle;

        // The request chain: request ← target ← surface.
        // SAFETY: the device is open.
        let request = unsafe { camera2::capture_request_create(device_ptr, camera2::TEMPLATE_RECORD) }?;
        session.request = request;
        let target = camera2::output_target_create(window)?;
        session.target = target;
        camera2::request_add_target(request, target)?;

        // The preview rides in the repeating request, so every frame the encoder gets is also
        // rendered into the preview reader's window: what the interface shows is what is being
        // recorded, and the capture path never waits on it.
        if let Some(preview_window) = preview {
            let preview_target = camera2::output_target_create(preview_window)?;
            session.preview_target = preview_target;
            camera2::request_add_target(request, preview_target)?;
        }

        // The still request, targeting only the still window: it is submitted by
        // `capture_still` and must not bring the encoder surface into the same request, which
        // would make the one-shot capture stall the video stream behind its own JPEG.
        if let Some(still_window) = still {
            // SAFETY: as for the repeating request above.
            let still_request =
                unsafe { camera2::capture_request_create(device_ptr, camera2::TEMPLATE_STILL_CAPTURE) }?;
            session.still_request = still_request;
            let still_target = camera2::output_target_create(still_window)?;
            session.still_target = still_target;
            camera2::request_add_target(still_request, still_target)?;
        }

        // A constant frame rate: both bounds of the requested range are the configured rate,
        // which is also the one request the metadata header cannot misread in either order.
        camera2::request_set_i32(
            request,
            camera2::TAG_CONTROL_AE_TARGET_FPS_RANGE,
            &[fps as i32, fps as i32],
        )?;

        // SAFETY: the session is live and the request is owned by `session`, which keeps it
        // alive until repeating is stopped.
        unsafe { camera2::set_repeating_request(handle, std::slice::from_mut(&mut session.request)) }?;
        session.repeating = true;

        info!(
            fps,
            still = !session.still_request.is_null(),
            "capture session streaming into the encoder surface"
        );
        Ok(session)
    }

    /// Submits the one-shot still request, if this session has a still output.
    ///
    /// Fails with [`CaptureError::UnsupportedParameters`] when the session was built without
    /// one, which is what the caller reports through `ack {ok: false}`: the camera refused the
    /// JPEG output at this geometry, so no still can be produced while the video runs.
    pub(crate) fn capture_still(&mut self) -> Result<(), CaptureError> {
        if self.still_request.is_null() {
            return Err(CaptureError::UnsupportedParameters);
        }
        let mut requests = [self.still_request];
        // SAFETY: the session is live and the still request is owned by `session`, which keeps
        // it alive until repeating is stopped; its target is one of the session's own outputs.
        unsafe { camera2::capture_once(self.session, &mut requests) }?;
        debug!("still capture submitted");
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Teardown is the reverse of construction. The NDK does not spell out how long a
        // request has to outlive a repeating call, so the session is stopped and closed first,
        // and only then are the objects it referenced freed.
        if self.repeating && !self.session.is_null() {
            // SAFETY: the session is live and repeating was started on it.
            unsafe { camera2::stop_repeating(self.session) };
            self.repeating = false;
        }
        if !self.session.is_null() {
            // SAFETY: the pointer came from `capture_session_create` and is not used again; all
            // access to a closed session is documented as a crash, which is why it comes first.
            unsafe { camera2::capture_session_close(self.session) };
            self.session = ptr::null_mut();
        }
        if !self.request.is_null() {
            // SAFETY: the pointer came from `capture_request_create` and is not used again.
            unsafe { camera2::capture_request_free(self.request) };
            self.request = ptr::null_mut();
        }
        if !self.target.is_null() {
            // SAFETY: the pointer came from `output_target_create` and is not used again. The
            // surface it was built from is the encoder's, not this session's.
            unsafe { camera2::output_target_free(self.target) };
            self.target = ptr::null_mut();
        }
        if !self.output.is_null() {
            // SAFETY: as above, for the session output.
            unsafe { camera2::output_free(self.output) };
            self.output = ptr::null_mut();
        }
        if !self.still_request.is_null() {
            // SAFETY: the pointer came from `capture_request_create` and is not used again; the
            // session that could submit it is already closed.
            unsafe { camera2::capture_request_free(self.still_request) };
            self.still_request = ptr::null_mut();
        }
        if !self.still_target.is_null() {
            // SAFETY: as in `target`, for the still window's target.
            unsafe { camera2::output_target_free(self.still_target) };
            self.still_target = ptr::null_mut();
        }
        if !self.still_output.is_null() {
            // SAFETY: as in `output`, for the still window's session output. The window itself
            // belongs to the still reader, which outlives this session.
            unsafe { camera2::output_free(self.still_output) };
            self.still_output = ptr::null_mut();
        }
        if !self.preview_target.is_null() {
            // SAFETY: as in `target`, for the preview window's target.
            unsafe { camera2::output_target_free(self.preview_target) };
            self.preview_target = ptr::null_mut();
        }
        if !self.preview_output.is_null() {
            // SAFETY: as in `output`, for the preview window's session output. The window
            // belongs to the preview reader, which outlives this session.
            unsafe { camera2::output_free(self.preview_output) };
            self.preview_output = ptr::null_mut();
        }
        if !self.container.is_null() {
            // SAFETY: as above, for the container.
            unsafe { camera2::output_container_free(self.container) };
            self.container = ptr::null_mut();
        }
    }
}

/// The session was closed and deleted from memory.
///
/// Touching the session after this crashes, so the callback only logs: teardown in this crate
/// happens on the owning thread, and the framework calls this one on its own.
unsafe extern "C" fn on_closed(_context: *mut c_void, _session: *mut ACameraCaptureSession) {
    debug!("the capture session closed");
}

/// The session ran out of requests to process.
///
/// With a repeating request in place this should not happen; it means the stream stopped, which
/// the client surfaces rather than swallowing.
unsafe extern "C" fn on_ready(_context: *mut c_void, _session: *mut ACameraCaptureSession) {
    warn!("the capture session went idle: the repeating request is no longer running");
}

/// The session started processing requests.
unsafe extern "C" fn on_active(_context: *mut c_void, _session: *mut ACameraCaptureSession) {
    debug!("the capture session became active");
}
