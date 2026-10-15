//! Still capture: the JPEG the camera writes on `take_photo`.
//!
//! Android has no still-image endpoint in the video pipeline, so a photo is a second session
//! output: an `AImageReader` in `AIMAGE_FORMAT_JPEG` whose window joins the capture session
//! next to the encoder's surface, plus a one-shot capture request that targets only it. The
//! reader hands the still over as a single plane, and those bytes are the JPEG the protocol's
//! photo channel carries — nothing here re-encodes them, and nothing here changes the video
//! stream's cadence: the repeating request keeps filling the encoder while the still runs.
//!
//! # Where the lifetimes sit
//!
//! The session references the reader's window for as long as the session lives, so a
//! [`StillCapture`] has to outlive the session it was handed to — the pipeline's field order
//! is what guarantees that. The window is *borrowed*: `AImageReader_delete` releases it, so
//! unlike the encoder's input surface there is no separate reference of our own to give back.
//!
//! # What is not known before hardware runs this
//!
//! Whether a camera accepts a JPEG output next to its `PRIVATE` video output at the
//! requested geometry is a device property, so the caller treats the still output as
//! optional: a session the framework refuses is rebuilt without it, and `take_photo` then
//! reports that this camera cannot produce stills instead of failing the stream.

use std::ffi::c_int;
use std::ptr;
use std::slice;
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, error, info};

use super::image_reader::{
    AImage, AImageReader, AImageReader_acquireNextImage, AImageReader_delete,
    AImageReader_getWindow, AImageReader_new, AImage_delete, AImage_getPlaneData,
};
use super::native_window::ANativeWindow;
use crate::error::CaptureError;

/// `AIMAGE_FORMAT_JPEG`: the compressed still the camera's JPEG output carries.
pub(crate) const IMAGE_FORMAT_JPEG: i32 = 0x100;

/// How many images the reader may hand out at once.
///
/// The still path acquires one image per `take_photo` and deletes it before returning, so two
/// is already one more than is ever held: a second buffer lets the camera write the next
/// capture while the previous one is being copied out, without the producer ever blocking.
pub(crate) const MAX_IMAGES: i32 = 2;

/// How long to wait between polls while the camera has not written a buffer yet.
///
/// The reader offers no blocking acquire (`AImageReader_acquireNextImage` returns
/// `AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE` immediately), so the wait is a poll loop; five
/// milliseconds keeps the latency of a still that arrives late well under one video frame.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// `AMEDIA_OK`: the call succeeded.
pub(crate) const AMEDIA_OK: c_int = 0;
/// `AMEDIA_ERROR_BASE`: the first of the negative media codes.
const AMEDIA_ERROR_BASE: c_int = -10_000;
/// `AMEDIA_ERROR_UNKNOWN`.
const AMEDIA_ERROR_UNKNOWN: c_int = AMEDIA_ERROR_BASE;
/// `AMEDIA_ERROR_MALFORMED`.
const AMEDIA_ERROR_MALFORMED: c_int = AMEDIA_ERROR_BASE - 1;
/// `AMEDIA_ERROR_UNSUPPORTED`.
const AMEDIA_ERROR_UNSUPPORTED: c_int = AMEDIA_ERROR_BASE - 2;
/// `AMEDIA_ERROR_INVALID_OBJECT`: the reader behind the image is gone.
const AMEDIA_ERROR_INVALID_OBJECT: c_int = AMEDIA_ERROR_BASE - 3;
/// `AMEDIA_ERROR_INVALID_PARAMETER`: a geometry the platform will not accept.
pub(crate) const AMEDIA_ERROR_INVALID_PARAMETER: c_int = AMEDIA_ERROR_BASE - 4;
/// `AMEDIA_ERROR_INVALID_OPERATION`.
const AMEDIA_ERROR_INVALID_OPERATION: c_int = AMEDIA_ERROR_BASE - 5;
/// `AMEDIA_ERROR_IO`.
const AMEDIA_ERROR_IO: c_int = AMEDIA_ERROR_BASE - 7;
/// `AMEDIA_ERROR_WOULD_BLOCK`.
const AMEDIA_ERROR_WOULD_BLOCK: c_int = AMEDIA_ERROR_BASE - 8;
/// `AMEDIA_IMGREADER_ERROR_BASE`: the first of the image reader's codes.
const AMEDIA_IMGREADER_ERROR_BASE: c_int = -30_000;
/// `AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE`: no buffer has been written yet.
pub(crate) const AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE: c_int = AMEDIA_IMGREADER_ERROR_BASE - 1;
/// `AMEDIA_IMGREADER_MAX_IMAGES_ACQUIRED`: all `maxImages` images are still held.
const AMEDIA_IMGREADER_MAX_IMAGES_ACQUIRED: c_int = AMEDIA_IMGREADER_ERROR_BASE - 2;
/// `AMEDIA_IMGREADER_CANNOT_LOCK_IMAGE`: the image could not be mapped for CPU access.
const AMEDIA_IMGREADER_CANNOT_LOCK_IMAGE: c_int = AMEDIA_IMGREADER_ERROR_BASE - 3;

/// The JPEG start-of-image marker (`FF D8`) every still begins with.
const JPEG_SOI: [u8; 2] = [0xFF, 0xD8];
/// The JPEG end-of-image marker (`FF D9`) every still ends with.
const JPEG_EOI: [u8; 2] = [0xFF, 0xD9];

/// The JPEG output of one camera.
pub(crate) struct StillCapture {
    /// The reader the camera writes the still into.
    reader: *mut AImageReader,
    /// The reader's window, borrowed until the reader is deleted.
    window: *mut ANativeWindow,
}

impl StillCapture {
    /// Creates a JPEG image reader for `width`x`height`.
    ///
    /// Fails with [`CaptureError::UnsupportedParameters`] when the geometry cannot be given
    /// to the platform, and with [`CaptureError::Os`] when no reader can be created at all.
    pub(crate) fn new(width: u32, height: u32) -> Result<Self, CaptureError> {
        // The header takes `int32_t`, and a size that does not fit is one the platform could
        // not accept either, so both conversions are refusals of the same kind.
        let width = i32::try_from(width).map_err(|_| CaptureError::UnsupportedParameters)?;
        let height = i32::try_from(height).map_err(|_| CaptureError::UnsupportedParameters)?;

        let mut reader: *mut AImageReader = ptr::null_mut();
        // SAFETY: the out pointer is a live local, the format and image count are constants
        // the header lists as accepted, and the reader (if any) is owned by this value from
        // here on — the call either fills the pointer with a reader the caller must delete or
        // leaves it untouched with a non-OK status.
        let status =
            unsafe { AImageReader_new(width, height, IMAGE_FORMAT_JPEG, MAX_IMAGES, &mut reader) };
        if status != AMEDIA_OK {
            // Width and height are the only caller-controlled inputs the header can reject,
            // so a parameter refusal here is a geometry this camera refuses.
            if status == AMEDIA_ERROR_INVALID_PARAMETER {
                return Err(CaptureError::UnsupportedParameters);
            }
            return Err(status_error("AImageReader_new", status));
        }
        if reader.is_null() {
            return Err(CaptureError::Os(
                "AImageReader_new reported success but returned no reader".to_owned(),
            ));
        }

        let mut window: *mut ANativeWindow = ptr::null_mut();
        // SAFETY: the reader was created just above and is still live, and the out pointer is
        // a local the platform fills with the window it owns.
        let status = unsafe { AImageReader_getWindow(reader, &mut window) };
        if status != AMEDIA_OK || window.is_null() {
            // SAFETY: the reader is live and was never handed out, so this is its only owner
            // and the delete cannot double-free.
            unsafe { AImageReader_delete(reader) };
            if status != AMEDIA_OK {
                return Err(status_error("AImageReader_getWindow", status));
            }
            return Err(CaptureError::Os(
                "AImageReader_getWindow reported success but returned no window".to_owned(),
            ));
        }

        debug!(
            width,
            height, "created the JPEG image reader for the still output"
        );
        Ok(Self { reader, window })
    }

    /// The window the capture session has to include as an output.
    ///
    /// Valid until this value is dropped; the session must be gone by then.
    pub(crate) fn window(&self) -> *mut ANativeWindow {
        self.window
    }

    /// Waits up to `timeout` for the still the camera wrote and returns its bytes.
    ///
    /// The bytes are copied out of the image before it is deleted, so the returned vector
    /// outlives the platform object. Fails with [`CaptureError::Os`] when no still arrives
    /// within `timeout`.
    pub(crate) fn take_jpeg(&mut self, timeout: Duration) -> Result<Vec<u8>, CaptureError> {
        let start = Instant::now();
        loop {
            let mut image: *mut AImage = ptr::null_mut();
            // SAFETY: the reader is live and owned by this value, and the out pointer is a
            // local. On `AMEDIA_OK` the platform has handed over one image that this call is
            // then responsible for deleting.
            let status = unsafe { AImageReader_acquireNextImage(self.reader, &mut image) };
            match status {
                AMEDIA_OK => return read_plane(image),
                AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE => {
                    if start.elapsed() >= timeout {
                        return Err(CaptureError::Os(format!(
                            "no still arrived within {timeout:?}"
                        )));
                    }
                    thread::sleep(POLL_INTERVAL);
                }
                // Any other status is terminal — all images still held, an image that cannot
                // be locked, a reader the platform has invalidated — so waiting again cannot
                // turn it into a still.
                _ => return Err(status_error("AImageReader_acquireNextImage", status)),
            }
        }
    }

    /// Discards every still the reader is holding.
    ///
    /// Called before a new capture is submitted. The camera writes a JPEG whether or not the
    /// caller is still waiting for it, so a photo whose `take_jpeg` already gave up — or one
    /// the framework delivered after the caller moved on — would otherwise be handed over as
    /// the answer to the *next* request, which is a still from before the operator asked.
    pub(crate) fn drain(&mut self) {
        loop {
            let mut image: *mut AImage = ptr::null_mut();
            // SAFETY: the reader is live and owned by this value, and the out pointer is a
            // local. An image handed over here is deleted below before the next acquire, so at
            // most one is held at a time and the reader never runs out of its `MAX_IMAGES`.
            let status = unsafe { AImageReader_acquireNextImage(self.reader, &mut image) };
            if status != AMEDIA_OK || image.is_null() {
                // `NO_BUFFER_AVAILABLE` — the ordinary case, nothing left to drop — or a
                // status there is nothing useful to do about: this runs on a path where the
                // caller is about to ask the camera for a new still anyway.
                if status != AMEDIA_OK && status != AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE {
                    debug!(status, "could not drain the still reader");
                }
                return;
            }
            // SAFETY: the platform handed this image over above and it is not used again.
            unsafe { AImage_delete(image) };
        }
    }
}

impl Drop for StillCapture {
    fn drop(&mut self) {
        // The reader owns its window; deleting it releases both.
        if !self.reader.is_null() {
            // SAFETY: this value owns the reader for its whole life and no other copy of the
            // pointer exists, so the delete happens once. The reader must not be used after
            // this, which is why both fields are cleared below.
            unsafe { AImageReader_delete(self.reader) };
            self.reader = ptr::null_mut();
        }
        self.window = ptr::null_mut();
    }
}

// SAFETY: the reader is created and deleted on the capture thread alone, and the window it
// hands out is only passed back to that same thread's session. Moving the value into the
// capture thread at construction is the one transfer that happens.
unsafe impl Send for StillCapture {}

/// Copies plane 0 of an acquired image out, deletes the image, and returns the bytes.
///
/// The function owns `image` on every path after a successful acquire: a JPEG image has
/// exactly one plane, and the bytes `AImage_getPlaneData` points at are only valid until the
/// image is deleted, so the copy happens before the delete rather than being handed out as a
/// borrow.
fn read_plane(image: *mut AImage) -> Result<Vec<u8>, CaptureError> {
    let mut data: *mut u8 = ptr::null_mut();
    let mut length: c_int = 0;
    // SAFETY: `image` was just acquired from a live reader and is not deleted until the end
    // of this function; both out pointers are live locals. Plane 0 exists for a JPEG image,
    // whose plane count the header fixes at one.
    let status = unsafe { AImage_getPlaneData(image, 0, &mut data, &mut length) };

    let result = if status != AMEDIA_OK {
        Err(status_error("AImage_getPlaneData", status))
    } else if data.is_null() || length <= 0 {
        Err(CaptureError::Os(
            "the still image carried no readable plane".to_owned(),
        ))
    } else {
        // SAFETY: the platform filled `data` with a pointer into the image's plane and
        // `length` with its valid length in bytes; the image is still alive, so the range is
        // readable for the whole call, and the slice is copied out before the image is
        // deleted below.
        let bytes = unsafe { slice::from_raw_parts(data, length as usize) };
        verify_jpeg(bytes).map(|()| bytes.to_vec())
    };

    // SAFETY: the image was acquired above and is deleted exactly once, here, whichever branch
    // produced `result`. Nothing reads `data` after it.
    unsafe { AImage_delete(image) };

    if let Ok(jpeg) = &result {
        info!(
            bytes = jpeg.len(),
            "delivered a still from the JPEG image reader"
        );
    }
    result
}

/// The header's symbolic name for a `media_status_t`, for logs.
pub(crate) fn status_name(status: c_int) -> &'static str {
    match status {
        AMEDIA_OK => "AMEDIA_OK",
        AMEDIA_ERROR_UNKNOWN => "AMEDIA_ERROR_UNKNOWN",
        AMEDIA_ERROR_MALFORMED => "AMEDIA_ERROR_MALFORMED",
        AMEDIA_ERROR_UNSUPPORTED => "AMEDIA_ERROR_UNSUPPORTED",
        AMEDIA_ERROR_INVALID_OBJECT => "AMEDIA_ERROR_INVALID_OBJECT",
        AMEDIA_ERROR_INVALID_PARAMETER => "AMEDIA_ERROR_INVALID_PARAMETER",
        AMEDIA_ERROR_INVALID_OPERATION => "AMEDIA_ERROR_INVALID_OPERATION",
        AMEDIA_ERROR_IO => "AMEDIA_ERROR_IO",
        AMEDIA_ERROR_WOULD_BLOCK => "AMEDIA_ERROR_WOULD_BLOCK",
        AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE => "AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE",
        AMEDIA_IMGREADER_MAX_IMAGES_ACQUIRED => "AMEDIA_IMGREADER_MAX_IMAGES_ACQUIRED",
        AMEDIA_IMGREADER_CANNOT_LOCK_IMAGE => "AMEDIA_IMGREADER_CANNOT_LOCK_IMAGE",
        _ => "unrecognized media status",
    }
}

/// Turns a `media_status_t` into a [`CaptureError`], naming the operation and the code.
///
/// Mirrors the media and camera2 helpers: the message carries both the call and the numeric
/// status, so a failure on a device is diagnosable from the client's log alone.
pub(crate) fn status_error(operation: &str, status: c_int) -> CaptureError {
    let name = status_name(status);
    error!(code = status, status = name, "{operation}");
    CaptureError::Os(format!("{operation}: {name} ({status})"))
}

/// Checks that `bytes` is a whole JPEG: the `FF D8` start marker in front, `FF D9` behind.
///
/// The far end drops a still that does not parse — the protocol's photo channel and its
/// `MEDIA_QUEUE` checklist discard a frame on any mismatch rather than reporting it — so a
/// truncated capture would otherwise look like success here and a missing photo there. A
/// capture that does not end in EOI is one the camera never finished writing, which is what
/// this rejects before it reaches the wire.
fn verify_jpeg(bytes: &[u8]) -> Result<(), CaptureError> {
    if bytes.starts_with(&JPEG_SOI) && bytes.ends_with(&JPEG_EOI) {
        return Ok(());
    }
    Err(CaptureError::Os(format!(
        "the still is not a complete JPEG: {} bytes without both SOI and EOI markers",
        bytes.len()
    )))
}
