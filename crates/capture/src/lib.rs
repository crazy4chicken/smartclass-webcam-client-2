//! Camera capture and encoding for the SmartClass Webcam protocol.
//!
//! This crate is the platform half of the device client: it lists the cameras of the host
//! ([`enumerate_cameras`]), opens one of them ([`open`]) and hands the resulting
//! [`CaptureSource`] to [`scwc_core`], which drives it as a
//! [`FrameSource`](scwc_core::FrameSource) — pulls encoded frames, reconfigures the
//! pipeline on `switch_camera` and asks for stills on `take_photo`.
//!
//! # Backends
//!
//! - **macOS and iOS**: AVFoundation for capture, VideoToolbox for encoding, both driven by
//!   one Objective-C++ shim compiled by this crate's build script (`src/macos/`, shared by
//!   both platforms — the differences are behind `TARGET_OS_IPHONE` in the shim). Frames
//!   leave the encoder as Annex B access units with parameter sets in front of every IDR,
//!   which is what the server's H.264/H.265 consumers expect.
//! - **Linux**: V4L2 (`/dev/video*`) for capture and VAAPI for encoding, both spoken to
//!   directly from Rust with no C shim, so the crate cross-checks without system headers.
//!   MJPEG-only cameras are decoded in software before encoding; see the `linux` module for
//!   the runtime requirements (libva, a render node, the v4l2 device group) and for the
//!   behaviours that hardware testing still has to confirm.
//! - **Windows**: Media Foundation for capture and encoding, WIC for stills, driven from
//!   pure Rust — Media Foundation is COM and the `windows` crate covers it, so no shim is
//!   compiled. Hardware encoders (NVENC, Quick Sync, AMF) are preferred and the inbox
//!   software H.264 encoder is the loud fallback. See the `windows` module for the
//!   behaviours that hardware testing still has to confirm.
//! - **Android**: the Camera2 NDK for capture and `AMediaCodec` for encoding, both reached
//!   through hand-written FFI like the Linux backend, so no shim is compiled either.
//!   Enumeration reports each camera's real capability matrix, and the media path carries
//!   `switch_camera` rebuilds and stills through a JPEG reader. Streaming (up to 4K60, with
//!   the frame cadence verified while media flows), `switch_camera` and `take_photo` are
//!   verified on hardware through the Flutter shell; a sustained wall-clock rate is not — the
//!   run was minutes, not a soak. Thermal step-down is not built: the plan asks for it,
//!   nothing in this backend measures heat, and the configured bitrate stands until an
//!   operator changes it. The `CAMERA` permission is held per process on this platform: an
//!   installed app gets it from the user, while a native binary run from `adb shell` is at the
//!   mercy of the vendor's own policy, which on one vendor's devices means its camera HAL
//!   aborts on such a client's first request (the `android` module records that).
//! - **Everything else**: [`enumerate_cameras`] and [`open`] fail with
//!   [`CaptureError::Os`]; the crate still compiles so that the rest of the workspace can
//!   be built and tested on any host.
//!
//! # iOS
//!
//! The iOS side of the shim is the macOS one with the platform's own device discovery,
//! session preset, interruption and thermal handling:
//!
//! - **Permission**: the host app *must* declare `NSCameraUsageDescription` in its
//!   `Info.plist`. The shim asks for access when the process has not been asked before, and
//!   iOS terminates a process that requests camera access — or starts a capture session —
//!   without that key. A denial or a restriction comes back as
//!   [`CaptureError::PermissionDenied`], the same as on macOS.
//! - **Cameras**: enumeration reports the built-in wide-angle, ultra-wide and telephoto
//!   cameras (the ones the device actually has), and each name carries the position
//!   (front/back), because iOS calls every one of them "Camera".
//! - **Orientation**: frames are handed on as the sensor produces them. Nothing in this
//!   crate rotates them to the interface orientation — that is the app layer's job, since
//!   the protocol carries no rotation field to describe it in.
//! - **Thermal pressure**: state transitions are logged, and that is all — no backend in this
//!   client retargets the encoder, so the configured bitrate stands until an operator changes
//!   it (never a change of resolution or frame rate, which the protocol reserves for
//!   `switch_camera`).
//! - **Interruptions**: a session that a phone call or another client takes away is logged
//!   and restarted once the interruption ends.
//!
//! None of the above has run on real iOS hardware yet: the simulator exposes no camera, so
//! the behaviours are the ones the platform documents, not ones this crate has observed.
//!
//! # Frame pacing
//!
//! Encoded frames travel from the encoder callback into a bounded channel of
//! [`MEDIA_QUEUE_CAPACITY`](scwc_core::MEDIA_QUEUE_CAPACITY) entries. When it is full, delta
//! frames are dropped and keyframes wait for space, mirroring the core's own drop policy:
//! a decoder can always recover at the next IDR, but a dropped IDR costs a whole GOP.
//!
//! # Live preview
//!
//! Every backend also offers the frames it captures to
//! [`scwc_core::preview`](scwc_core::preview), which swaps them to RGBA and hands them to
//! whoever registered a sink there. The path is paced by the consumer and costs the capture
//! nothing it cannot keep up with: each backend asks [`scwc_core::preview::next_frame_size`]
//! what the next frame is worth before it converts anything, and that answers `None` while no
//! preview is configured, while the frame the consumer was handed has not been reported
//! consumed, and for a frame without pixels. When it answers, it names the size to produce the
//! frame at — the interface's own size fitted to the frame's aspect ratio — so a preview is
//! built at the size it is shown at and never at the capture resolution. The Apple backend hands
//! the frame to a queue of its own before rendering it, so its capture callback only asks the
//! gate and returns.
//!
//! # Example
//!
//! ```no_run
//! use scwc_capture::{CaptureConfig, enumerate_cameras, open};
//! use scwc_protocol::Codec;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let cameras = enumerate_cameras()?;
//! let camera = cameras.first().ok_or("no camera")?;
//! let source = open(CaptureConfig {
//!     camera_id: Some(camera.id.clone()),
//!     width: 3840,
//!     height: 2160,
//!     fps: 30,
//!     codec: Codec::H264,
//!     bitrate_kbps: 20_000,
//! })?;
//! # let _ = source;
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod camera_matrix;
mod error;
mod source;

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod macos;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use macos as sys;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as sys;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as sys;

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "android")]
use android as sys;

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "windows",
    target_os = "android"
)))]
mod unsupported;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "windows",
    target_os = "android"
)))]
use unsupported as sys;

pub use crate::error::CaptureError;
pub use crate::source::{CaptureSource, open};
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use crate::source::{release_frame_buffer, retain_frame_buffer};
/// The interface's own preview window, on the one platform that has one.
///
/// On Android the embedding application can hand a `Surface` over for the camera to render into,
/// which is what makes a preview cost nothing per frame; these are the entry points its plugin
/// calls. The window is a native object, so it stays in the Android module rather than in the
/// core: nothing outside this crate names it.
#[cfg(target_os = "android")]
pub use crate::android::preview_surface::{
    active as preview_external_active, attach as preview_attach_surface,
    detach as preview_detach_surface, rotation as preview_rotation_degrees,
    size as preview_size_pixels,
};

/// Lists the cameras of the host.
///
/// Each camera reports the resolutions and frame rates its own format descriptors
/// announce, highest resolution and highest frame rate first. Nothing here starts a
/// capture session: this is the cheap listing an operator picks a camera from and the raw
/// material for a launch-time validation pass, which is a separate step because vendors
/// misreport what their hardware can stream.
///
/// Fails with [`CaptureError::NoCamera`] when the host has no video device and with
/// [`CaptureError::PermissionDenied`] when the process may not use the camera.
pub fn enumerate_cameras() -> Result<Vec<CameraInfo>, CaptureError> {
    sys::enumerate_cameras()
}

/// One camera of the host, as reported by the platform backend.
///
/// `presets` maps a resolution string (`"3840x2160"`) to the frame rates the device
/// announces for it, highest first. The list comes from the device's own format
/// descriptors, so it is a *claim*: the platform can accept a format it cannot actually
/// stream. The planned launch-time validation — creating a session per candidate pair and
/// keeping only what works — attaches its results to the same structure, which is why the
/// presets are a plain matrix rather than pre-flattened registration lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    /// Stable device identifier (AVFoundation `uniqueID` on macOS and iOS, the device
    /// interface path on Windows).
    pub id: String,
    /// Human-readable device name.
    pub name: String,
    /// Announced `(resolution, frame rates)` pairs, largest resolution first.
    pub presets: Vec<(String, Vec<u32>)>,
}

/// What [`open`] should capture and how it should encode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureConfig {
    /// Device identifier from [`CameraInfo::id`]; `None` selects the system default camera.
    pub camera_id: Option<String>,
    /// Requested capture width in pixels.
    pub width: u32,
    /// Requested capture height in pixels.
    pub height: u32,
    /// Requested capture frame rate.
    pub fps: u32,
    /// Codec the hardware encoder produces.
    pub codec: scwc_protocol::Codec,
    /// Target encoder bitrate in kilobits per second.
    pub bitrate_kbps: u32,
}
