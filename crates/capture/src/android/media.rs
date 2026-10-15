//! Hand-declared bindings for the Android media NDK (`libmediandk.so`) and the hardware
//! encoder built on them.
//!
//! The crate talks to the platform through hand-written FFI rather than a binding
//! generator, for the same reason [`camera2`](super::camera2) and the Linux backend do:
//! `cargo check` has to work on a host without the NDK, its headers or a C toolchain, and the
//! workspace's own `cargo check` runs on Windows, macOS and Linux alike.
//!
//! Every declaration below is transcribed from the NDK r28 headers
//! (`media/NdkMediaCodec.h`, `media/NdkMediaFormat.h`, `media/NdkMediaError.h`,
//! `android/native_window.h`), and the link names are the ones the NDK writes down:
//!
//! - `libmediandk.so` holds the codec and the format, which is why `AMediaFormat_*` is
//!   declared here beside the codec entry points rather than in a module of its own. The
//!   `AMEDIAFORMAT_KEY_*` keys are *exported data*, not macros — the header declares them
//!   `extern const char*` — so they are read through those pointers instead of being spelled
//!   out as literals (which would silently drift from the platform if a key ever changed).
//! - `android/native_window.h` contributes one thing here: the opaque type name
//!   `ANativeWindow`, which is what [`Encoder::input_surface`] hands to the camera session.
//!   The header's two entry points are deliberately *not* declared in this file — see
//!   [`Encoder::stop`] for why nothing here manages that surface's lifetime — and the
//!   session's own [`native_window`](super::native_window) module is where the reference
//!   counting calls belong.
//!
//! # The shape of the encoder
//!
//! The camera writes frames straight into a surface the codec owns
//! ([`Encoder::input_surface`]), the codec encodes them on its own threads, and the encoder
//! here only *drains* output: one access unit per [`Encoder::next_access_unit`] call. Nothing
//! is queued by hand, which is why there is no `dequeueInputBuffer` in this file.
//!
//! What comes back out is turned into the wire format the server consumes: Annex B, with the
//! parameter sets in front of every IDR. The codec does not deliver it that way — see
//! [`to_annex_b`] — so the buffer is rewritten here, and the sequence header the codec
//! delivers out of band (`csd-0`/`csd-1`) is remembered and replayed.
//!
//! # What is not known before hardware runs this
//!
//! Whether `AMediaCodec` reports the sequence header as `csd-0`/`csd-1` or as one `csd-0`
//! blob differs per device, and so does whether the output is AVCC or already Annex B; both
//! shapes are handled below, and neither has been observed on a device from this host.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;
use std::slice;

use bytes::BufMut;
use tracing::{debug, error, info, warn};

use scwc_protocol::Codec;

use super::native_window::{self, ANativeWindow};
use crate::error::CaptureError;

// ---------------------------------------------------------------------------------------
// Opaque handle types.
//
// None of these is ever dereferenced by Rust: the platform owns the storage and hands out
// pointers, so the types exist only to make those pointers distinct from each other.
// ---------------------------------------------------------------------------------------

/// Opaque `AMediaCodec`: the platform's encoder (or decoder) instance.
#[repr(C)]
pub(crate) struct AMediaCodec {
    _private: [u8; 0],
}

/// Opaque `AMediaFormat`: a set of typed key/value pairs describing a stream.
#[repr(C)]
pub(crate) struct AMediaFormat {
    _private: [u8; 0],
}

/// `AMediaCrypto`: the DRM handle passed to `AMediaCodec_configure`.
///
/// Declared only so the null pointer this backend passes there has the right type. Cleartext
/// capture needs no crypto at all, which is what the header's `AMediaCrypto*` argument
/// accepts.
#[repr(C)]
pub(crate) struct AMediaCrypto {
    _private: [u8; 0],
}

/// `AMediaCodecBufferInfo`: where one dequeued output buffer starts and what it holds.
///
/// The field order and types are the header's, and the platform fills this struct in place
/// through the pointer handed to `AMediaCodec_dequeueOutputBuffer`, so a reordering here
/// would be a silent misread rather than a compile error.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct AMediaCodecBufferInfo {
    /// Byte offset of the payload within the codec's output buffer.
    pub(crate) offset: i32,
    /// Payload length in bytes.
    pub(crate) size: i32,
    /// Presentation timestamp of the access unit, in microseconds.
    pub(crate) presentation_time_us: i64,
    /// `AMEDIACODEC_BUFFER_FLAG_*` bits.
    pub(crate) flags: u32,
}

impl Default for AMediaCodecBufferInfo {
    /// The all-zero value the platform overwrites when a buffer is dequeued.
    ///
    /// Written out by hand rather than derived: the struct carries a `flags` field whose zero
    /// value means "no flags set", and an explicit initializer is the one place that reads as
    /// what it is.
    fn default() -> Self {
        Self {
            offset: 0,
            size: 0,
            presentation_time_us: 0,
            flags: 0,
        }
    }
}

// ---------------------------------------------------------------------------------------
// `media_status_t` (`NdkMediaError.h`).
// ---------------------------------------------------------------------------------------

/// `AMEDIA_OK`: the call succeeded.
const AMEDIA_OK: c_int = 0;
/// `AMEDIACODEC_ERROR_INSUFFICIENT_RESOURCE`: the platform could not allocate the codec.
const AMEDIACODEC_ERROR_INSUFFICIENT_RESOURCE: c_int = 1100;
/// `AMEDIACODEC_ERROR_RECLAIMED`: the resource manager took the codec away.
///
/// The codec is in a terminal state afterwards and must be released rather than reused.
const AMEDIACODEC_ERROR_RECLAIMED: c_int = 1101;
/// `AMEDIA_ERROR_BASE`: the first of the negative codes.
const AMEDIA_ERROR_BASE: c_int = -10_000;
/// `AMEDIA_ERROR_UNKNOWN`.
const AMEDIA_ERROR_UNKNOWN: c_int = AMEDIA_ERROR_BASE;
/// `AMEDIA_ERROR_MALFORMED`.
const AMEDIA_ERROR_MALFORMED: c_int = AMEDIA_ERROR_BASE - 1;
/// `AMEDIA_ERROR_UNSUPPORTED`.
const AMEDIA_ERROR_UNSUPPORTED: c_int = AMEDIA_ERROR_BASE - 2;
/// `AMEDIA_ERROR_INVALID_OBJECT`.
const AMEDIA_ERROR_INVALID_OBJECT: c_int = AMEDIA_ERROR_BASE - 3;
/// `AMEDIA_ERROR_INVALID_PARAMETER`.
const AMEDIA_ERROR_INVALID_PARAMETER: c_int = AMEDIA_ERROR_BASE - 4;
/// `AMEDIA_ERROR_INVALID_OPERATION`.
const AMEDIA_ERROR_INVALID_OPERATION: c_int = AMEDIA_ERROR_BASE - 5;
/// `AMEDIA_ERROR_END_OF_STREAM`.
const AMEDIA_ERROR_END_OF_STREAM: c_int = AMEDIA_ERROR_BASE - 6;
/// `AMEDIA_ERROR_IO`.
const AMEDIA_ERROR_IO: c_int = AMEDIA_ERROR_BASE - 7;
/// `AMEDIA_ERROR_WOULD_BLOCK`.
const AMEDIA_ERROR_WOULD_BLOCK: c_int = AMEDIA_ERROR_BASE - 8;

// ---------------------------------------------------------------------------------------
// Codec flags and dequeue info codes (`NdkMediaCodec.h`).
// ---------------------------------------------------------------------------------------

/// `AMEDIACODEC_BUFFER_FLAG_KEY_FRAME`: the unit can be decoded without any earlier one.
const AMEDIACODEC_BUFFER_FLAG_KEY_FRAME: u32 = 1;
/// `AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG`: the unit is a codec configuration, not a picture.
///
/// Some devices put the sequence header in the output stream this way instead of only in
/// `csd-0`; the unit is passed through as an access unit, because a decoder that joins
/// mid-stream can use it and dropping it would lose the only copy of the parameter sets on
/// those devices.
const AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG: u32 = 2;

/// `AMEDIACODEC_CONFIGURE_FLAG_ENCODE`: this codec encodes rather than decodes.
const AMEDIACODEC_CONFIGURE_FLAG_ENCODE: u32 = 1;
/// `AMEDIACODEC_INFO_TRY_AGAIN_LATER`: nothing was produced within the timeout.
const AMEDIACODEC_INFO_TRY_AGAIN_LATER: isize = -1;
/// `AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED`: the output format is available for the first
/// time, or has changed.
const AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED: isize = -2;
/// `AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED`: the output buffer set was replaced.
///
/// This is the one code that needs no handling at all: it is a leftover from the Java API's
/// `getOutputBuffers()`, whose stable array this NDK never exposes — `AMediaCodec_getOutputBuffer`
/// takes an index and looks the buffer up itself — so there is nothing for this backend to
/// re-fetch.
const AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED: isize = -3;

/// `COLOR_FormatSurface`, the `android.graphics.ImageFormat` code for an opaque buffer a
/// hardware encoder consumes directly.
///
/// This is the media NDK's name for the same format [`camera2::IMAGE_FORMAT_PRIVATE`] calls
/// `PRIVATE`: the camera writes into the codec's surface through it, with no CPU copy.
///
/// [`camera2::IMAGE_FORMAT_PRIVATE`]: super::camera2::IMAGE_FORMAT_PRIVATE
const COLOR_FORMAT_SURFACE: i32 = 0x7F00_0789;

/// How many seconds of video one GOP covers.
///
/// Deliberately the same two seconds as the VAAPI and VideoToolbox backends: a client that
/// loses frames recovers at the next IDR, so the interval trades bitrate against recovery
/// time, and the server's own consumers assume no longer a wait than this.
const I_FRAME_INTERVAL_SECONDS: u32 = 2;

/// `MediaFormat.KEY_CSD_0`: the first codec-specific-data blob of the output format.
///
/// Spelled as a literal rather than read through the NDK symbol, which only exists from API 28,
/// while the devices this client targets include Android 8. The loader resolves every symbol
/// this library references *before* `main` runs — `BIND_NOW`, as the Android toolchain links it
/// — so a symbol from a newer API than the oldest supported device would keep the whole library
/// from loading there. The value is the Java API's documented key string.
const KEY_CSD_0: &CStr = c"csd-0";

/// `MediaFormat.KEY_CSD_1`: the second codec-specific-data blob of the output format.
///
/// The sibling of [`KEY_CSD_0`], spelled out for the same reason.
const KEY_CSD_1: &CStr = c"csd-1";

/// The `media_status_t` a call has to return to count as successful.
const STATUS_OK: c_int = AMEDIA_OK;

// ---------------------------------------------------------------------------------------
// libmediandk entry points.
//
// The C names are kept verbatim: this file is meant to be read against `NdkMediaCodec.h` and
// `NdkMediaFormat.h` line by line, so nothing is renamed on the way in.
// ---------------------------------------------------------------------------------------

#[link(name = "mediandk")]
unsafe extern "C" {
    fn AMediaCodec_createEncoderByType(mime_type: *const c_char) -> *mut AMediaCodec;
    fn AMediaCodec_configure(
        codec: *mut AMediaCodec,
        format: *const AMediaFormat,
        surface: *mut ANativeWindow,
        crypto: *mut AMediaCrypto,
        flags: u32,
    ) -> c_int;
    fn AMediaCodec_createInputSurface(
        codec: *mut AMediaCodec,
        surface: *mut *mut ANativeWindow,
    ) -> c_int;
    fn AMediaCodec_start(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_stop(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_delete(codec: *mut AMediaCodec) -> c_int;
    fn AMediaCodec_dequeueOutputBuffer(
        codec: *mut AMediaCodec,
        info: *mut AMediaCodecBufferInfo,
        timeout_us: i64,
    ) -> isize;
    fn AMediaCodec_getOutputBuffer(
        codec: *mut AMediaCodec,
        index: usize,
        out_size: *mut usize,
    ) -> *mut u8;
    fn AMediaCodec_releaseOutputBuffer(codec: *mut AMediaCodec, index: usize, render: bool)
    -> c_int;
    fn AMediaCodec_getOutputFormat(codec: *mut AMediaCodec) -> *mut AMediaFormat;

    fn AMediaFormat_new() -> *mut AMediaFormat;
    fn AMediaCodec_setParameters(
        codec: *mut AMediaCodec,
        params: *const AMediaFormat,
    ) -> c_int;
    fn AMediaFormat_delete(format: *mut AMediaFormat) -> c_int;
    fn AMediaFormat_setInt32(format: *mut AMediaFormat, name: *const c_char, value: i32);
    fn AMediaFormat_setString(
        format: *mut AMediaFormat,
        name: *const c_char,
        value: *const c_char,
    );
    fn AMediaFormat_getInt32(
        format: *mut AMediaFormat,
        name: *const c_char,
        out: *mut i32,
    ) -> bool;
    fn AMediaFormat_getBuffer(
        format: *mut AMediaFormat,
        name: *const c_char,
        data: *mut *mut c_void,
        size: *mut usize,
    ) -> bool;

    /// `AMEDIAFORMAT_KEY_MIME`.
    static AMEDIAFORMAT_KEY_MIME: *mut c_char;
    /// `AMEDIAFORMAT_KEY_WIDTH`.
    static AMEDIAFORMAT_KEY_WIDTH: *mut c_char;
    /// `AMEDIAFORMAT_KEY_HEIGHT`.
    static AMEDIAFORMAT_KEY_HEIGHT: *mut c_char;
    /// `AMEDIAFORMAT_KEY_BIT_RATE`.
    static AMEDIAFORMAT_KEY_BIT_RATE: *mut c_char;
    /// `AMEDIAFORMAT_KEY_FRAME_RATE`.
    static AMEDIAFORMAT_KEY_FRAME_RATE: *mut c_char;
    /// `AMEDIAFORMAT_KEY_I_FRAME_INTERVAL`.
    static AMEDIAFORMAT_KEY_I_FRAME_INTERVAL: *mut c_char;
    /// `AMEDIAFORMAT_KEY_COLOR_FORMAT`.
    static AMEDIAFORMAT_KEY_COLOR_FORMAT: *mut c_char;
}

// ---------------------------------------------------------------------------------------
// `android/native_window.h`.
//
// The surface type is shared with the camera session, which wraps the same pointer in an
// output, so it lives in [`super::native_window`] and is imported above rather than declared
// again here: two declarations would be one C object in two incompatible Rust types.
// ---------------------------------------------------------------------------------------

/// The codec's own name for the `AMEDIAFORMAT_KEY_*` entries, as a C string.
///
/// The key is a variable, not a macro, so it is read once per use instead of being spelled out
/// as a literal in this file. Reading the static is itself an `unsafe` operation, which is why
/// every caller below spells the read *inside* the `unsafe` block that makes the FFI call: the
/// read and the call are one step there, and the `// SAFETY:` comment beside them covers both.
fn key(name: *mut c_char) -> *const c_char {
    name.cast_const()
}

/// The header's name for a `media_status_t`, for logs.
fn status_name(status: c_int) -> &'static str {
    match status {
        AMEDIA_OK => "AMEDIA_OK",
        AMEDIACODEC_ERROR_INSUFFICIENT_RESOURCE => "AMEDIACODEC_ERROR_INSUFFICIENT_RESOURCE",
        AMEDIACODEC_ERROR_RECLAIMED => "AMEDIACODEC_ERROR_RECLAIMED",
        AMEDIA_ERROR_UNKNOWN => "AMEDIA_ERROR_UNKNOWN",
        AMEDIA_ERROR_MALFORMED => "AMEDIA_ERROR_MALFORMED",
        AMEDIA_ERROR_UNSUPPORTED => "AMEDIA_ERROR_UNSUPPORTED",
        AMEDIA_ERROR_INVALID_OBJECT => "AMEDIA_ERROR_INVALID_OBJECT",
        AMEDIA_ERROR_INVALID_PARAMETER => "AMEDIA_ERROR_INVALID_PARAMETER",
        AMEDIA_ERROR_INVALID_OPERATION => "AMEDIA_ERROR_INVALID_OPERATION",
        AMEDIA_ERROR_END_OF_STREAM => "AMEDIA_ERROR_END_OF_STREAM",
        AMEDIA_ERROR_IO => "AMEDIA_ERROR_IO",
        AMEDIA_ERROR_WOULD_BLOCK => "AMEDIA_ERROR_WOULD_BLOCK",
        _ => "unrecognized media status",
    }
}

/// Turns a `media_status_t` into a [`CaptureError`], logging the platform's own code.
///
/// Mirrors the camera2 helper: the message names both the operation and the numeric status,
/// so a failure on a device is diagnosable from the client's log alone.
fn status_error(operation: &str, status: c_int) -> CaptureError {
    let name = status_name(status);
    error!(code = status, status = name, "{operation}");
    CaptureError::Os(format!("{operation}: {name} ({status})"))
}

/// The MIME type of a codec, or `None` for one this platform cannot encode.
///
/// The media NDK names codecs the way the `MediaFormat` API does: `video/avc` for H.264 and
/// `video/hevc` for H.265. The other protocol codecs (MJPEG, MPEG-4, VP8, VP9, AV1) are
/// deliberately not mapped: `MediaCodec` has encoders for several of them, but the server's
/// H.264/H.265 consumers are the only ones this backend feeds, and claiming support for a
/// codec nothing downstream can decode would move the failure to the far end of the wire.
fn mime_type(codec: Codec) -> Option<&'static str> {
    match codec {
        Codec::H264 => Some("video/avc"),
        Codec::H265 => Some("video/hevc"),
        Codec::Mjpeg | Codec::Mpeg4 | Codec::Vp8 | Codec::Vp9 | Codec::Av1 => None,
    }
}

/// One encoded access unit, Annex B, ready for the wire.
///
/// The codec also reports a presentation timestamp per unit, which the core does not take: the
/// `FrameSource` contract hands over bytes and a keyframe flag, and the core stamps each frame
/// with its own capture clock so that a `ts` gap matches a dropped frame. Carrying the codec's
/// timestamp here would be carrying a number nobody reads.
pub(crate) struct AccessUnit {
    /// The access unit's bytes, each NAL unit introduced by a four-byte start code.
    pub(crate) data: Vec<u8>,
    /// Whether the unit is an IDR, which is what a client can join the stream at.
    pub(crate) is_keyframe: bool,
}

/// One live hardware encoder.
///
/// The codec runs on the platform's own threads: the camera writes into the input surface,
/// and this handle only drains access units. Every method must run on the thread that owns
/// the handle — the capture thread — because [`Encoder::next_access_unit`] borrows `&mut self`
/// and the codec's output queue is what the whole pipeline is paced by.
pub(crate) struct Encoder {
    /// The codec, owned here from `create` until `stop`.
    codec: *mut AMediaCodec,
    /// The surface the camera writes into, and the reference [`Encoder::input_surface`] hands
    /// out.
    ///
    /// The codec holds its own reference to it, and the extra one taken here is what keeps the
    /// pointer valid even if the codec stops before the camera session does.
    input_surface: *mut ANativeWindow,
    /// The codec this encoder was built for; decides how the bitstream is rewritten.
    codec_kind: Codec,
    /// `csd-0` of the last output format: SPS || PPS for H.264, VPS || SPS || PPS for H.265.
    csd_0: Vec<u8>,
    /// `csd-1` of the last output format, usually the PPS of H.264 or the SPS of H.265.
    csd_1: Vec<u8>,
    /// Whether `AMediaCodec_start` has succeeded, i.e. whether output can be dequeued.
    started: bool,
}

impl Encoder {
    /// Creates and configures a hardware encoder for `codec` at this geometry.
    ///
    /// Fails with [`CaptureError::UnsupportedParameters`] when the platform has no encoder
    /// for the codec, and with [`CaptureError::Os`] otherwise. That includes a device whose
    /// codec cannot take the requested geometry or bitrate: `MediaCodec` picks the component
    /// itself here, and a configuration it refuses comes back as a status code rather than as
    /// a list this backend could have filtered in advance.
    pub(crate) fn create(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self, CaptureError> {
        // A zero in any of these is a caller mistake rather than a device failure, and the
        // error the core reacts to for "these parameters cannot be honoured" is this one.
        if width == 0 || height == 0 || fps == 0 || bitrate_kbps == 0 {
            return Err(CaptureError::UnsupportedParameters);
        }
        let Some(mime) = mime_type(codec) else {
            return Err(CaptureError::UnsupportedParameters);
        };

        info!(
            ?codec,
            mime, width, height, fps, bitrate_kbps, "creating the Android hardware encoder"
        );
        let codec_pointer = create_codec_by_type(mime)?;
        let mut encoder = Self {
            codec: codec_pointer,
            input_surface: ptr::null_mut(),
            codec_kind: codec,
            csd_0: Vec::new(),
            csd_1: Vec::new(),
            started: false,
        };
        match encoder.configure(mime, width, height, fps, bitrate_kbps) {
            Ok(()) => Ok(encoder),
            Err(error) => {
                // The codec is deleted rather than stopped: it was never started, so
                // `AMediaCodec_stop` would only report an invalid operation. The input
                // surface is released if `configure` got as far as creating it.
                encoder.release_surface();
                delete_codec(encoder.codec);
                encoder.codec = ptr::null_mut();
                Err(error)
            }
        }
    }

    /// Configures the codec and creates the surface the camera will write into.
    fn configure(
        &mut self,
        mime: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<(), CaptureError> {
        let Some(format) = MediaFormat::new() else {
            return Err(CaptureError::Os(
                "AMediaFormat_new returned null".to_owned(),
            ));
        };

        let mime = CString::new(mime).map_err(|error| {
            CaptureError::Os(format!("the codec's MIME type is not a C string: {error}"))
        })?;
        let Ok(width) = i32::try_from(width) else {
            return Err(CaptureError::UnsupportedParameters);
        };
        let Ok(height) = i32::try_from(height) else {
            return Err(CaptureError::UnsupportedParameters);
        };
        let Ok(fps) = i32::try_from(fps) else {
            return Err(CaptureError::UnsupportedParameters);
        };
        // The format takes the bitrate in bits per second, and it is an `int32`: a request
        // large enough to overflow it is clamped rather than wrapped into a negative number,
        // which the codec would reject.
        let bitrate = bitrate_kbps.saturating_mul(1000).min(i32::MAX as u32) as i32;
        let i_frame_interval = i32::try_from(I_FRAME_INTERVAL_SECONDS).unwrap_or(i32::MAX);

        // SAFETY: each key is an `AMEDIAFORMAT_KEY_*` extern static read for its value only.
        // The loader fills them from `libmediandk.so`'s data section before `main` runs and the
        // platform never reassigns them, so each read yields a valid NUL-terminated key at any
        // point in the program's life.
        unsafe {
            format.set_string(AMEDIAFORMAT_KEY_MIME, &mime);
            format.set_int32(AMEDIAFORMAT_KEY_WIDTH, width);
            format.set_int32(AMEDIAFORMAT_KEY_HEIGHT, height);
            format.set_int32(AMEDIAFORMAT_KEY_BIT_RATE, bitrate);
            format.set_int32(AMEDIAFORMAT_KEY_FRAME_RATE, fps);
            format.set_int32(AMEDIAFORMAT_KEY_I_FRAME_INTERVAL, i_frame_interval);
            format.set_int32(AMEDIAFORMAT_KEY_COLOR_FORMAT, COLOR_FORMAT_SURFACE);
        }

        // No window and no crypto: the input surface this encoder will use does not exist yet,
        // so the header is given a null window, and the stream is cleartext, so the DRM handle
        // is null too. Both are documented as "none" rather than as errors.
        // SAFETY: the codec is live and in the Configured state (created above, configured
        // once, never started), the format outlives the call, and the runtime accepts the null
        // window and crypto for an encoder that is handed its input through a surface.
        let status = unsafe {
            AMediaCodec_configure(
                self.codec,
                format.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                AMEDIACODEC_CONFIGURE_FLAG_ENCODE,
            )
        };
        // The format is dropped at the end of this function, after the codec has copied
        // everything it wants out of it; it is freed on the failure paths too.
        if status != STATUS_OK {
            return Err(status_error("configuring the media codec", status));
        }

        debug!(
            width,
            height,
            fps,
            bitrate,
            i_frame_interval,
            "the codec accepted the format; creating its input surface"
        );

        let mut surface: *mut ANativeWindow = ptr::null_mut();
        // SAFETY: the codec was configured just above and has not been started, which is the
        // window the header allows this call in. The out pointer is a local.
        let status = unsafe { AMediaCodec_createInputSurface(self.codec, &mut surface) };
        if status != STATUS_OK {
            return Err(status_error("creating the codec's input surface", status));
        }
        if surface.is_null() {
            return Err(CaptureError::Os(
                "AMediaCodec_createInputSurface returned a null surface".to_owned(),
            ));
        }

        // The creation reference is this struct's own, and `release_surface` gives it back
        // exactly once. It is what keeps the pointer good for the camera session for as long as
        // the encoder lives, without depending on the codec's internal count.
        self.input_surface = surface;

        debug!("the codec's input surface is ready for the camera session");
        Ok(())
    }

    /// The surface the camera writes frames into (`AMediaCodec_createInputSurface`).
    ///
    /// Valid until [`Encoder::stop`]: the encoder holds a reference of its own to it, so the
    /// pointer stays good even if the codec is torn down first.
    pub(crate) fn input_surface(&self) -> *mut ANativeWindow {
        self.input_surface
    }

    /// Asks the running encoder for a keyframe.
    ///
    /// `AMediaCodec` starts a stream in the middle of a GOP, so a consumer that attaches to an
    /// already-running encoder cannot decode anything until the next IDR arrives — up to the
    /// configured interval away. Requesting a sync frame at that moment is what
    /// [`FrameSource::set_encoding`](scwc_core::FrameSource::set_encoding) asks for when it
    /// re-enables encoding: the next unit the codec produces is then an IDR.
    ///
    /// The request is best-effort in the same way a bitrate change is: a codec may ignore a
    /// parameter it cannot honour, and that is not a failure of the capture path.
    pub(crate) fn request_keyframe(&mut self) {
        if self.codec.is_null() || !self.started {
            return;
        }
        let Some(params) = MediaFormat::new() else {
            return;
        };
        // The platform's own key for `AMediaCodec`'s parameter-set: `PARAMETER_KEY_REQUEST_SYNC_FRAME`,
        // whose value is documented as "request a sync frame"; it is read as a value, not a flag.
        const KEY_REQUEST_SYNC_FRAME: &CStr = c"request-sync-frame";
        params.set_int32_str(KEY_REQUEST_SYNC_FRAME, 0);
        // SAFETY: the codec is live and started, and `params` outlives the call.
        let status = unsafe { AMediaCodec_setParameters(self.codec, params.as_ptr()) };
        if status != STATUS_OK {
            debug!(status, "the encoder did not take the keyframe request");
        }
    }

    /// Tells the running codec whether to produce output at all.
    ///
    /// `AMediaCodec`'s `PARAMETER_KEY_SUSPEND` is the one non-destructive gate this platform
    /// offers: the camera keeps rendering into the encoder's input surface — it has to, the
    /// session's outputs are fixed — but a suspended codec consumes those frames without
    /// encoding them, so a pipeline with no consumer stops paying for an encoder and stops
    /// feeding the core's queue with units nobody takes. Resuming produces a keyframe first,
    /// which is what a stream starting now needs.
    ///
    /// Best-effort like the keyframe request: a codec that ignores the parameter keeps
    /// encoding, which costs power and nothing else.
    pub(crate) fn set_suspended(&mut self, suspended: bool) {
        if self.codec.is_null() || !self.started {
            return;
        }
        let Some(params) = MediaFormat::new() else {
            return;
        };
        // The platform's own key for `AMediaCodec`'s parameter-set:
        // `PARAMETER_KEY_SUSPEND`, non-zero to suspend and zero to resume.
        const KEY_SUSPEND: &CStr = c"suspend";
        params.set_int32_str(KEY_SUSPEND, i32::from(suspended));
        // SAFETY: the codec is live and started, and `params` outlives the call.
        let status = unsafe { AMediaCodec_setParameters(self.codec, params.as_ptr()) };
        if status != STATUS_OK {
            debug!(status, suspended, "the encoder did not take the suspend change");
        } else {
            debug!(suspended, "the encoder's output gate moved");
        }
    }

    /// Moves the codec to Executing (`AMediaCodec_start`).
    ///
    /// Must be called after the camera session has been given the input surface: an encoder
    /// configured with a surface produces no output until something renders into it, and the
    /// session is what starts rendering.
    pub(crate) fn start(&mut self) -> Result<(), CaptureError> {
        if self.started {
            // Already running: the header would answer this with `AMEDIA_ERROR_INVALID_OPERATION`,
            // which says nothing useful about the pipeline.
            return Ok(());
        }
        if self.codec.is_null() {
            return Err(CaptureError::Os(
                "the media codec was already released".to_owned(),
            ));
        }
        // SAFETY: the codec is live and configured; `start` is the transition to Executing.
        let status = unsafe { AMediaCodec_start(self.codec) };
        if status != STATUS_OK {
            return Err(status_error("starting the media codec", status));
        }
        self.started = true;
        info!("the Android hardware encoder is running");
        Ok(())
    }

    /// Blocks up to `timeout_us` for one encoded access unit.
    ///
    /// Returns `Ok(None)` when the timeout expired with nothing to report, which is the
    /// ordinary case between frames. A negative timeout waits indefinitely, as the platform
    /// documents.
    pub(crate) fn next_access_unit(
        &mut self,
        timeout_us: i64,
    ) -> Result<Option<AccessUnit>, CaptureError> {
        if self.codec.is_null() {
            return Err(CaptureError::Os(
                "the media codec was already released".to_owned(),
            ));
        }

        let mut info = AMediaCodecBufferInfo::default();
        // SAFETY: the codec is live and started, and `info` is a local the platform fills in
        // when the returned index is not one of the info codes.
        let index = unsafe { AMediaCodec_dequeueOutputBuffer(self.codec, &mut info, timeout_us) };

        if index == AMEDIACODEC_INFO_TRY_AGAIN_LATER {
            return Ok(None);
        }
        if index == AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED {
            // The codec says what it actually produces, which is where the parameter sets
            // come from. The format is re-read rather than only read once, because a codec may
            // change its mind mid-stream (a profile switch after a resolution change, say).
            self.read_output_format()?;
            return Ok(None);
        }
        if index == AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED {
            // Nothing to do: the buffer set behind the indices is looked up by the platform on
            // every `AMediaCodec_getOutputBuffer`, so there is no cached array to refresh.
            debug!("the codec replaced its output buffer set; ignoring it");
            return Ok(None);
        }
        if index < 0 {
            // Any other negative value is a status code, not an index.
            let status = index as c_int;
            return Err(status_error("dequeuing an encoded buffer", status));
        }

        let index = index as usize;
        let unit = self.copy_output_unit(index, &info);
        // The buffer's bytes live only until this call, which is why they were copied out
        // first. `render = false` because there is no output surface to display them on.
        // SAFETY: `index` came from `dequeueOutputBuffer` and has not been released yet.
        let status = unsafe { AMediaCodec_releaseOutputBuffer(self.codec, index, false) };
        if status != STATUS_OK {
            // The unit itself is intact, so it is still returned; a codec that cannot give a
            // buffer back is worth knowing about, and it will report the same failure on the
            // next call.
            warn!(
                code = status,
                status = status_name(status),
                "releasing an output buffer failed"
            );
        }
        // `copy_output_unit` already returns the `Option` this function promises, and the
        // release above happens whether the buffer held a unit or not.
        unit
    }

    /// Copies one dequeued output buffer out and turns it into an access unit.
    ///
    /// Returns `Ok(None)` for a buffer that carries nothing — the codec reports a zero-length
    /// unit for some configuration changes, and there is no access unit in it to send.
    fn copy_output_unit(
        &self,
        index: usize,
        info: &AMediaCodecBufferInfo,
    ) -> Result<Option<AccessUnit>, CaptureError> {
        let Some(offset) = usize::try_from(info.offset).ok() else {
            return Err(CaptureError::Os(format!(
                "the codec reported a negative output offset ({})",
                info.offset
            )));
        };
        let Some(size) = usize::try_from(info.size).ok() else {
            return Err(CaptureError::Os(format!(
                "the codec reported a negative output size ({})",
                info.size
            )));
        };
        if size == 0 {
            debug!(flags = info.flags, "the codec reported an empty output buffer");
            return Ok(None);
        }

        let mut capacity = 0usize;
        // SAFETY: `index` came from `dequeueOutputBuffer` and has not been released, and the
        // out pointer is a local.
        let buffer = unsafe { AMediaCodec_getOutputBuffer(self.codec, index, &mut capacity) };
        if buffer.is_null() {
            return Err(CaptureError::Os(
                "AMediaCodec_getOutputBuffer returned a null buffer".to_owned(),
            ));
        }
        let Some(end) = offset.checked_add(size) else {
            return Err(CaptureError::Os(format!(
                "the codec reported {size} bytes at offset {offset}, which overflows"
            )));
        };
        if end > capacity {
            return Err(CaptureError::Os(format!(
                "the codec reported {size} bytes at offset {offset} in a {capacity}-byte buffer"
            )));
        }

        // SAFETY: `buffer` points at `capacity` bytes owned by the codec for as long as the
        // buffer is not released, and the range checked above is inside it. The bytes are
        // copied here because the buffer dies at `AMediaCodec_releaseOutputBuffer`.
        let encoded = unsafe { slice::from_raw_parts(buffer.add(offset), size) };
        let is_keyframe = info.flags & AMEDIACODEC_BUFFER_FLAG_KEY_FRAME != 0;
        if info.flags & AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG != 0 {
            debug!(
                bytes = size,
                "the codec emitted its configuration in the stream itself"
            );
        }

        let data = to_annex_b(self.codec_kind, encoded, is_keyframe, &self.csd_0, &self.csd_1);
        Ok(Some(AccessUnit { data, is_keyframe }))
    }

    /// Reads the codec's current output format and remembers its parameter sets.
    fn read_output_format(&mut self) -> Result<(), CaptureError> {
        // SAFETY: the codec is live and started, which is when the output format exists. The
        // caller owns the returned format and frees it below.
        let format = unsafe { AMediaCodec_getOutputFormat(self.codec) };
        let Some(format) = MediaFormat::from_owned(format) else {
            return Err(CaptureError::Os(
                "AMediaCodec_getOutputFormat returned null".to_owned(),
            ));
        };

        // The parameter sets are copied out of the format before it is deleted, because the
        // `AMediaFormat_getBuffer` documentation ties the returned bytes to the format's
        // lifetime. The keys are the crate's own literals — see [`KEY_CSD_0`] — so the read
        // needs no `unsafe` beyond what `buffer` itself carries.
        self.csd_0 = format.buffer(KEY_CSD_0);
        self.csd_1 = format.buffer(KEY_CSD_1);
        debug!(
            csd_0 = self.csd_0.len(),
            csd_1 = self.csd_1.len(),
            "the codec reported its output format"
        );
        if self.csd_0.is_empty() && self.csd_1.is_empty() {
            // A codec that hands out no sequence header has to put it in the stream instead,
            // which `to_annex_b` handles by passing such a unit through.
            debug!(
                "the codec reports no csd-0/csd-1, so its parameter sets have to arrive in the \
                 stream itself"
            );
        }
        Ok(())
    }

    /// Stops the codec and releases it. Idempotent.
    ///
    /// Also gives back the reference this module took on the input surface, so the pointer
    /// [`Encoder::input_surface`] handed to the camera session — and the pointer that session
    /// hands to the framework, which is what drains the surface — is what `stop` invalidates:
    /// the caller has to stop the session with it before calling this.
    pub(crate) fn stop(&mut self) {
        if !self.codec.is_null() {
            if self.started {
                // SAFETY: the codec is live and started, which is the state `stop` expects.
                let status = unsafe { AMediaCodec_stop(self.codec) };
                if status != STATUS_OK {
                    // Nothing the caller can do about it, and the codec is deleted next either
                    // way; the status is logged so a device that fails here is visible.
                    warn!(
                        code = status,
                        status = status_name(status),
                        "stopping the media codec failed"
                    );
                }
                self.started = false;
            }
            delete_codec(self.codec);
            self.codec = ptr::null_mut();
        }
        // Released after the codec is deleted: the codec stops using the surface first, and
        // what is dropped here is only the reference the codec handed over at creation.
        self.release_surface();
        self.csd_0 = Vec::new();
        self.csd_1 = Vec::new();
    }

    /// Drops the reference to the input surface, if the encoder still holds it.
    fn release_surface(&mut self) {
        if self.input_surface.is_null() {
            return;
        }
        // SAFETY: the pointer came from `AMediaCodec_createInputSurface`, whose header makes the
        // caller responsible for releasing it, and that one reference is released exactly once
        // because the field is cleared here — `stop` and `create`'s failure path both go through
        // this method, and whichever runs first takes the pointer away from the other.
        unsafe { native_window::release(self.input_surface) };
        self.input_surface = ptr::null_mut();
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.stop();
    }
}

// SAFETY: the encoder is used from the capture thread alone; moving it into that thread at
// construction is the one transfer that happens. What it carries is two platform pointers and
// two byte vectors. The platform's own `MediaCodec` may be called from any thread but not from
// two at once, and every call on either pointer happens through `&mut self` — one at a time,
// on whichever thread owns the handle. The camera session that receives
// `Encoder::input_surface` runs on that same thread.
unsafe impl Send for Encoder {}

/// Creates the platform's encoder for one MIME type.
fn create_codec_by_type(mime: &str) -> Result<*mut AMediaCodec, CaptureError> {
    let mime = CString::new(mime).map_err(|error| {
        CaptureError::Os(format!("the codec's MIME type is not a C string: {error}"))
    })?;
    // SAFETY: `mime` is a NUL-terminated string that outlives the call. The returned codec is
    // owned by the caller, and null means the platform has no encoder for this type.
    let codec = unsafe { AMediaCodec_createEncoderByType(mime.as_ptr()) };
    if codec.is_null() {
        // The component name is not known before this call, so the MIME type is all there is to
        // name in the message.
        debug!(mime = %mime.to_string_lossy(), "the platform has no encoder for this codec");
        return Err(CaptureError::UnsupportedParameters);
    }
    Ok(codec)
}

/// Deletes a codec, reporting instead of propagating failure.
///
/// Takes the pointer by value rather than by reference: the caller clears its own copy in the
/// same step, which is what keeps the delete from happening twice.
fn delete_codec(codec: *mut AMediaCodec) {
    if codec.is_null() {
        return;
    }
    // SAFETY: the pointer came from `AMediaCodec_createEncoderByType` and is deleted once,
    // because every caller drops its copy.
    let status = unsafe { AMediaCodec_delete(codec) };
    if status != STATUS_OK {
        debug!(
            code = status,
            status = status_name(status),
            "deleting the media codec reported a failure"
        );
    }
}

/// An `AMediaFormat` owned by this crate, deleted when it goes out of scope.
///
/// Both the format built for `AMediaCodec_configure` and the one
/// `AMediaCodec_getOutputFormat` hands out are owned by the caller, so neither would be freed
/// by an early return in the code between creating and dropping it.
struct MediaFormat {
    /// The owned format; never null, because the only ways to build this are checked.
    ptr: *mut AMediaFormat,
}

impl MediaFormat {
    /// A new, empty format, or `None` when the platform cannot allocate one.
    fn new() -> Option<Self> {
        // SAFETY: takes no arguments and returns an owned format, or null.
        let ptr = unsafe { AMediaFormat_new() };
        Self::from_owned(ptr)
    }

    /// Takes ownership of a format the platform handed out, unless it is null.
    fn from_owned(ptr: *mut AMediaFormat) -> Option<Self> {
        (!ptr.is_null()).then_some(Self { ptr })
    }

    /// The raw pointer, for the calls that read the format.
    fn as_ptr(&self) -> *const AMediaFormat {
        self.ptr
    }

    /// Stores a string-valued entry. The value is copied into the format.
    fn set_string(&self, name: *mut c_char, value: &CStr) {
        // SAFETY: the format is live; `name` is read from an `AMEDIAFORMAT_KEY_*` extern static,
        // which the loader fills from `libmediandk.so`'s data section before `main` and the
        // platform never reassigns, so it is a valid key for the whole program's life; and the
        // value is a NUL-terminated string that outlives the call.
        unsafe { AMediaFormat_setString(self.ptr, key(name), value.as_ptr()) };
    }

    /// Stores an integer entry.
    fn set_int32(&self, name: *mut c_char, value: i32) {
        // SAFETY: the format is live, and `name` is an `AMEDIAFORMAT_KEY_*` extern static read
        // for the value the loader put there before `main`, which the platform never changes.
        unsafe { AMediaFormat_setInt32(self.ptr, key(name), value) };
    }

    /// Stores an integer entry under a key spelled as a C string.
    ///
    /// For the keys this crate states itself rather than reads from the platform (see
    /// [`Encoder::request_keyframe`]): the NDK exports a symbol for each `AMEDIAFORMAT_KEY_*`,
    /// but nothing for `AMediaCodec`'s parameter keys, whose values are the documented strings.
    fn set_int32_str(&self, name: &CStr, value: i32) {
        // SAFETY: the format is live, and `name` is a NUL-terminated key that outlives the call.
        unsafe { AMediaFormat_setInt32(self.ptr, name.as_ptr(), value) };
    }

    /// Reads a byte-array entry, or an empty vector when it is absent or of another type.
    ///
    /// Returns an owned copy: the bytes belong to the format and are only valid for as long as
    /// it lives.
    fn buffer(&self, name: &CStr) -> Vec<u8> {
        let mut data: *mut c_void = ptr::null_mut();
        let mut size = 0usize;
        // SAFETY: the format is live, `name` is a NUL-terminated key that outlives the call, and
        // both out parameters are locals. The call returns false and writes nothing when the
        // entry is absent.
        let found = unsafe { AMediaFormat_getBuffer(self.ptr, name.as_ptr(), &mut data, &mut size) };
        if !found || data.is_null() || size == 0 {
            return Vec::new();
        }
        // SAFETY: `found` means the format holds `size` bytes at `data`, valid until the
        // format is deleted — which is why the bytes are copied out here.
        unsafe { slice::from_raw_parts(data.cast::<u8>(), size) }.to_vec()
    }

    /// Reads an integer entry, or `None` when it is absent or of another type.
    ///
    /// Unused while nothing asks the codec what geometry it settled on, but declared beside
    /// its siblings so that the next increment reads the output format rather than re-deriving
    /// the FFI declaration for it.
    #[allow(dead_code)]
    fn int32(&self, name: *mut c_char) -> Option<i32> {
        let mut value = 0i32;
        // SAFETY: as in `buffer`, for an `int32` entry.
        let found = unsafe { AMediaFormat_getInt32(self.ptr, key(name), &mut value) };
        found.then_some(value)
    }
}

impl Drop for MediaFormat {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `AMediaFormat_new` or `AMediaCodec_getOutputFormat`
        // and is deleted once, because `MediaFormat` is the only owner and is not `Clone`.
        unsafe { AMediaFormat_delete(self.ptr) };
    }
}

// ---------------------------------------------------------------------------------------
// Bitstream: from what `MediaCodec` emits to what the server consumes.
//
// The wire format is Annex B, with the parameter sets in front of every IDR, so that each
// access unit a client can join at is decodable on its own. What comes out of the codec is
// one of two shapes, and which one is a device property:
//
// - **AVCC**, the common case: each NAL unit is introduced by its length in four big-endian
//   bytes, and the sequence header is *not* in the stream at all — the codec reports it once,
//   out of band, in `csd-0`/`csd-1` (SPS || PPS for H.264; VPS, SPS and PPS for H.265, split
//   across the two keys differently per device). Those access units are rewritten below:
//   every length prefix becomes a start code, and the remembered parameter sets are prepended
//   in front of each IDR.
// - **Annex B**, on some devices: the units already carry start codes, and the parameter sets
//   are usually already in the stream ahead of the IDR. Such a unit is passed through
//   unchanged, and the parameter sets are only prepended when the unit does not already
//   contain them — prepending them twice would put two SPS/PPS pairs in one access unit,
//   which decoders tolerate unevenly.
//
// The two are told apart by inspecting the unit, since `MediaCodec` reports no flag for it:
// a start code where a length prefix would be, or a length prefix larger than the buffer it is
// supposed to describe, can only be Annex B.
// ---------------------------------------------------------------------------------------

/// The four-byte start code that introduces a NAL unit in Annex B.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Whether a buffer at `data` begins with a start code.
///
/// A three-byte start code is deliberately *not* accepted: a three-byte prefix is also a valid
/// AVCC length of 1, and `AMediaCodec` writes four-byte prefixes, so accepting three would
/// misfile a legitimate AVCC unit as Annex B.
fn starts_with_start_code(data: &[u8]) -> bool {
    data.starts_with(&START_CODE)
}

/// Whether the AVCC length prefix at the start of `encoded` is unusable, which means the
/// buffer is not AVCC at all.
///
/// Two cases: a prefix of zero, which no encoder writes because it would describe an empty NAL
/// unit, and a prefix larger than the buffer it is supposed to describe. Both are what an
/// Annex B unit looks like when it is read as a length — `00 00 00 01` is a length of one byte,
/// and any other start code is a length of hundreds of megabytes.
fn avcc_length_is_impossible(encoded: &[u8]) -> bool {
    let Some(prefix) = encoded.get(..4) else {
        // Shorter than one prefix: nothing to convert, and `to_annex_b` passes it through.
        return true;
    };
    let declared = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);
    declared == 0 || declared as usize > encoded.len()
}

/// Converts one output buffer into an Annex B access unit, prepending the parameter sets of
/// `csd_0`/`csd_1` in front of a keyframe that does not already carry them.
fn to_annex_b(
    codec: Codec,
    encoded: &[u8],
    is_keyframe: bool,
    csd_0: &[u8],
    csd_1: &[u8],
) -> Vec<u8> {
    if encoded.is_empty() {
        return Vec::new();
    }

    let annex_b = if starts_with_start_code(encoded) || avcc_length_is_impossible(encoded) {
        // Already Annex B: the bytes go out as they came in, start codes and all.
        debug!(
            bytes = encoded.len(),
            "the codec emitted Annex B; passing the unit through"
        );
        encoded.to_vec()
    } else {
        avcc_to_annex_b(encoded)
    };

    if !is_keyframe {
        return annex_b;
    }
    if has_parameter_sets(codec, &annex_b) {
        // The device put them in the stream itself. Prepending the copies from `csd-0`/`csd-1`
        // would repeat them inside one access unit.
        return annex_b;
    }

    let mut unit = Vec::with_capacity(annex_b.len() + csd_0.len() + csd_1.len() + 12);
    append_parameter_sets(&mut unit, csd_0, csd_1);
    unit.extend_from_slice(&annex_b);
    unit
}

/// Rewrites length-prefixed NAL units (AVCC) as start-code-prefixed ones (Annex B).
///
/// The first prefix decides the prefix width, which `MediaCodec` always writes as four bytes;
/// a unit whose prefixes do not add up is copied out as far as it was converted and then left
/// alone, because dropping bitstream on the floor would turn a decoder error into a silent
/// loss of a picture.
fn avcc_to_annex_b(encoded: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(encoded.len() + 16);
    let mut cursor = 0usize;

    while cursor + 4 <= encoded.len() {
        let prefix = &encoded[cursor..cursor + 4];
        let declared = if prefix[0] == 0 && prefix[1] == 0 && prefix[2] == 0 {
            4
        } else if prefix[0] == 0 && prefix[1] == 0 {
            2
        } else if prefix[0] == 0 {
            1
        } else {
            // Not a length prefix: either the codec used a wider one than the header
            // documents, or the unit already carries a start code. Either way the rest is
            // copied verbatim rather than misread.
            warn!(
                bytes = encoded.len(),
                cursor,
                "the codec's output has no readable NAL unit length prefix; copying the rest \
                 through unchanged"
            );
            out.put_slice(&encoded[cursor..]);
            return out;
        };
        let length = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
        let start = cursor + declared;
        let Some(end) = start.checked_add(length) else {
            warn!("a NAL unit length in the codec's output overflows; copying the rest through");
            out.put_slice(&encoded[cursor..]);
            return out;
        };
        if length == 0 || end > encoded.len() {
            warn!(
                bytes = encoded.len(),
                cursor,
                length,
                "a NAL unit extends past the codec's output buffer; copying the rest through"
            );
            out.put_slice(&encoded[cursor..]);
            return out;
        }
        out.put_slice(&START_CODE);
        out.put_slice(&encoded[start..end]);
        cursor = end;
    }

    if cursor < encoded.len() {
        // A trailing fragment shorter than a length prefix, which no encoder produces but
        // which has to go somewhere other than nowhere.
        warn!(
            bytes = encoded.len() - cursor,
            "the codec's output ends in a partial NAL unit length prefix"
        );
        out.put_slice(&encoded[cursor..]);
    }
    out
}

/// Appends the parameter sets, each as its own Annex B NAL unit.
///
/// The two blobs are appended in the codec's own order — `csd-0` first — which is the order
/// the decoder needs them in: SPS before PPS for H.264, and for H.265 whichever way the device
/// chose to split VPS, SPS and PPS across the two keys.
fn append_parameter_sets(out: &mut Vec<u8>, csd_0: &[u8], csd_1: &[u8]) {
    for (index, csd) in [csd_0, csd_1].into_iter().enumerate() {
        if csd.is_empty() {
            continue;
        }
        out.put_slice(&START_CODE);
        // A `csd-0` that is itself Annex B keeps its own start codes through this copy, which
        // is why it is split into NAL units and re-emitted rather than pasted in whole.
        for nal in split_annex_b(csd) {
            out.put_slice(nal);
        }
        debug!(
            csd = index,
            bytes = csd.len(),
            "prefixed the IDR with a parameter set"
        );
    }
}

/// Whether the unit already contains the parameter sets this codec's decoder needs.
fn has_parameter_sets(codec: Codec, unit: &[u8]) -> bool {
    match codec {
        // H.264: SPS is 7, PPS is 8.
        Codec::H264 => split_annex_b(unit)
            .iter()
            .any(|nal| matches!(nal.first().map(|byte| byte & 0x1F), Some(7 | 8))),
        // H.265: VPS is 32, SPS is 33, PPS is 34 — all inside the 32..=63 range, which uses the
        // whole first byte as the NAL unit type.
        Codec::H265 => split_annex_b(unit)
            .iter()
            .any(|nal| matches!(nal.first().map(|byte| byte >> 1), Some(32..=34))),
        // The other codecs never reach here: `Encoder::create` rejects them.
        Codec::Mjpeg | Codec::Mpeg4 | Codec::Vp8 | Codec::Vp9 | Codec::Av1 => false,
    }
}

/// Splits an Annex B blob into its NAL units, without their start codes.
///
/// Both the three-byte and the four-byte start code are recognised here, because this runs on
/// bytes the *device* produced and a parameter-set blob may use either. A blob that begins
/// with no start code at all is one NAL unit.
fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut cursor = 0usize;
    let mut start = None;

    while cursor < data.len() {
        let code_length = if data[cursor..].starts_with(&[0, 0, 1]) {
            3
        } else if data[cursor..].starts_with(&START_CODE) {
            4
        } else {
            cursor += 1;
            continue;
        };
        if let Some(previous) = start {
            units.push(&data[previous..cursor]);
        }
        cursor += code_length;
        start = Some(cursor);
    }

    if let Some(previous) = start {
        units.push(&data[previous..]);
    }
    if units.is_empty() {
        units.push(data);
    }
    units
}
