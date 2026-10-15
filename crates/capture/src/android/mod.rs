//! Android backend: Camera2 NDK for capture, `AMediaCodec` for encoding.
//!
//! Both APIs are reached through hand-written FFI (see [`camera2`]) so that the crate
//! cross-checks on any host, the same way the Linux backend declares the kernel ABI rather
//! than including `videodev2.h`.
//!
//! # What is different about Android
//!
//! - **Permission is per process.** The platform gates `ACameraManager_openCamera` on the
//!   `CAMERA` permission, which Android grants per application: an installed app gets it from
//!   the user, while on the phone this backend was first exercised against, a native binary
//!   started from `adb shell` was allowed to open the camera too. A run that is refused comes
//!   back as [`CaptureError::PermissionDenied`], not as a camera that never delivers.
//! - **Cameras have no names.** The NDK reports an id and a facing; the friendly name a
//!   user sees in the camera app is a vendor extra. Enumeration therefore builds a name out
//!   of the facing (`"Camera 0 (back)"`), which is what distinguishes the cameras in the
//!   registration the server stores.
//!
//! # State of this backend
//!
//! Everything the core can ask of a camera is wired: enumeration reads the camera's own
//! capability matrix, opening a camera brings up a hardware encoder with the camera writing
//! straight into its input surface, a capture thread moves encoded access units to the core,
//! `switch_camera` tears the stream down and builds it again on the same open device, and
//! `take_photo` submits a one-shot still request to a JPEG reader riding in the same session.
//!
//! The plan's thermal policy — throttling the encoder under thermal pressure — is deliberately
//! not implemented here: the server reports no thermal state of its own, nothing in this
//! backend measures it, and heat leaves the configured bitrate alone until an operator changes
//! it.
//!
//! Most of that wiring has since run on hardware — a vivo V2405A (MediaTek, Android 16)
//! through the Flutter shell, which is the context a device client actually runs in:
//!
//! - **Streaming**: registered and attached, then sent 1280x720@60 H.264 from the hardware
//!   encoder with `dropped 0` in the core's counters, and later 3840x2160@60 at that same
//!   cadence: every steady-state segment row advances 150 frames per ~2500 ms flush, and 2220
//!   frames land in ~37.6 s of segment time (≈59 fps). Those rows carry ~62 MB in total, ≈13 Mbps
//!   — the 11.3 MB opening row is the first IDR, not the rate. The stream's wall-clock window was
//!   ~78 s, so the dev server persisted about half of it; closing that gap is what a soak run is
//!   for.
//! - **`switch_camera`**: the server moved the camera to 640x480@30 and then to 3840x2160@60;
//!   the session and encoder were rebuilt on the same open device each time and frames kept
//!   flowing afterwards.
//! - **`take_photo`**: a still came back as a 210 KB `image/jpeg` carrying the `request_id` of
//!   the command that asked for it.
//!
//! # Why a run from `adb shell` says nothing about this backend
//!
//! The same code, run as a native binary from `adb shell`, never delivered a frame on that
//! phone: its vendor HAL aborts (`SIGABRT` inside `processCaptureRequest`, a mutex used after
//! destruction in `libvcf_session.so`) on the first request from *any* such client — every
//! geometry, template and output tried, including the most vanilla `TEMPLATE_PREVIEW` request
//! there is — while the vendor's own camera app streams from the same device without trouble.
//! In the app context the identical request path works, so that abort is a property of the
//! caller the vendor framework does not recognize (a `shell`-uid process with no package),
//! not of what this backend asks for. Reach for a `adb shell` probe only when a device is
//! suspected of being wedged, and expect the HAL, not the client, to be the story.

mod camera2;
mod image_reader;
mod media;
mod native_window;
mod preview;
// `pub(crate)` because the crate root re-exports this module's five entry points for the app's
// plugin: a path through two private modules is not one `pub use` can name.
pub(crate) mod preview_surface;
mod session;
mod still;

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use scwc_protocol::Codec;
use tracing::{debug, error, info, warn};

use self::camera2::{ACameraDevice_StateCallbacks, LensFacing, Manager, Metadata};
use self::native_window::ANativeWindow;
use self::preview::PreviewCapture;
use self::still::StillCapture;
use crate::camera_matrix::{fps_by_resolution, fps_ranges};
use crate::error::CaptureError;
use crate::source::{FrameCallback, JpegCallback};
use crate::{CameraInfo, CaptureConfig};

/// The `AIMAGE_FORMAT_*` values an encoder can be fed from.
///
/// `PRIVATE` is the zero-copy path: the camera writes straight into the encoder's input
/// surface. `YUV_420_888` is the fallback a software encoder needs, and is what the
/// capability matrix falls back to when a resolution has no `PRIVATE` entry (some cameras
/// only publish theirs for the sizes their own camera app records).
const VIDEO_FORMATS: [i32; 2] = [camera2::IMAGE_FORMAT_PRIVATE, camera2::IMAGE_FORMAT_YUV_420_888];

/// The clockwise turn the preview needs so the picture stands up in the interface.
///
/// The sensor reports how far its scanlines are rotated from the device's natural orientation —
/// 0, 90, 180 or 270. The interface is a portrait window in the common case, so a sensor mounted
/// at 90° (this backend's development device) delivers a landscape buffer that has to come back
/// a quarter turn. The still and video paths are deliberately left alone: the protocol has no
/// orientation field, and turning them would misreport what the camera produced.
fn rotation_degrees(characteristics: &Metadata) -> u16 {
    match characteristics.sensor_orientation() {
        Some(degrees) if degrees > 0 => (degrees as u16) % 360,
        _ => 0,
    }
}

/// What one camera says it can stream, read from its static characteristics.
///
/// The list is the camera's *claim*: vendors misreport what their hardware sustains, which
/// is why the plan validates candidate pairs by creating a session before trusting them.
struct CapabilityMatrix {
    /// `(resolution, frame rates)` per resolution, largest resolution first.
    presets: Vec<(String, Vec<u32>)>,
    /// The frame-rate ranges auto-exposure advertises, ascending.
    ae_ranges: Vec<(u32, u32)>,
}

impl CapabilityMatrix {
    /// Reads the matrix out of a camera's characteristics.
    fn read(characteristics: &Metadata) -> Self {
        let ae_ranges = match characteristics.entry(camera2::TAG_CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES)
        {
            Some(entry) => entry.as_i32().map(fps_ranges).unwrap_or_default(),
            None => {
                debug!("the camera advertises no auto-exposure frame-rate ranges");
                Vec::new()
            }
        };

        let presets = match characteristics.entry(camera2::TAG_SCALER_AVAILABLE_MIN_FRAME_DURATIONS) {
            Some(entry) => match entry.as_i64() {
                Some(durations) => fps_by_resolution(durations, &VIDEO_FORMATS),
                None => {
                    warn!("the camera reports frame durations in an unexpected type");
                    Vec::new()
                }
            },
            None => {
                debug!("the camera reports no minimum frame durations");
                Vec::new()
            }
        };

        Self { presets, ae_ranges }
    }

    /// Rejects a request the camera will not accept, and flags one it merely doubts.
    ///
    /// A size the camera does not publish at all cannot start a session, so the request
    /// fails here with the reason. A published size at an unpublished frame rate is only a
    /// warning: durations are a claim about the sensor, and the session created later is
    /// what actually decides.
    fn check(&self, width: u32, height: u32, fps: u32) -> Result<(), CaptureError> {
        if self.presets.is_empty() {
            // The camera published no durations at all; nothing to check against.
            return Ok(());
        }

        let requested = format!("{width}x{height}");
        match self.presets.iter().find(|(preset, _)| preset == &requested) {
            None => {
                let sizes: Vec<&str> = self
                    .presets
                    .iter()
                    .map(|(preset, _)| preset.as_str())
                    .collect();
                warn!(resolution = %requested, available = ?sizes, "the camera does not publish this size");
                Err(CaptureError::UnsupportedParameters)
            }
            Some((_, rates)) if !rates.contains(&fps) => {
                warn!(
                    resolution = %requested,
                    requested = fps,
                    announced = ?rates,
                    "the camera does not publish this frame rate; trying it anyway"
                );
                Ok(())
            }
            Some(_) => Ok(()),
        }
    }
}

/// Lists the cameras of the device.
///
/// A camera whose characteristics cannot be read is still listed, without presets, for the
/// same reason the Windows backend does it: an operator picking a camera is better served
/// by seeing the device than by an enumeration that hides it.
pub(crate) fn enumerate_cameras() -> Result<Vec<CameraInfo>, CaptureError> {
    let manager = Manager::create()?;
    let ids = manager.camera_ids()?;
    if ids.is_empty() {
        return Err(CaptureError::NoCamera);
    }

    let mut cameras = Vec::with_capacity(ids.len());
    let mut denied = false;

    for id in &ids {
        let (name, presets, level) = match manager.characteristics(id) {
            Ok(characteristics) => {
                let matrix = CapabilityMatrix::read(&characteristics);
                (
                    camera_name(id, characteristics.lens_facing()),
                    matrix.presets,
                    characteristics.hardware_level(),
                )
            }
            Err(CaptureError::PermissionDenied) => {
                denied = true;
                (camera_name(id, None), Vec::new(), None)
            }
            Err(error) => {
                debug!(camera = %id, %error, "cannot read this camera's characteristics");
                (camera_name(id, None), Vec::new(), None)
            }
        };

        debug!(
            camera = %id,
            %name,
            ?level,
            resolutions = presets.len(),
            "found a camera"
        );
        cameras.push(CameraInfo {
            id: id.clone(),
            name,
            presets,
        });
    }

    if cameras.is_empty() {
        return Err(if denied {
            CaptureError::PermissionDenied
        } else {
            CaptureError::NoCamera
        });
    }
    Ok(cameras)
}

/// The name shown for a camera the NDK only identifies by number.
fn camera_name(id: &str, facing: Option<LensFacing>) -> String {
    match facing {
        Some(LensFacing::Front) => format!("Camera {id} (front)"),
        Some(LensFacing::Back) => format!("Camera {id} (back)"),
        Some(LensFacing::External) => format!("Camera {id} (external)"),
        None => format!("Camera {id}"),
    }
}

/// The camera a run should open.
///
/// `--camera-id` selects one of the ids [`enumerate_cameras`] reports; without it the back
/// camera wins, because that is the one pointed at a classroom, falling back to whatever
/// the device lists first. A phone exposes several cameras and only the back one is
/// normally wired to the encoder at 4K.
fn resolve_camera_id(manager: &Manager, requested: Option<&str>) -> Result<String, CaptureError> {
    let ids = manager.camera_ids()?;
    if ids.is_empty() {
        return Err(CaptureError::NoCamera);
    }

    if let Some(requested) = requested {
        return ids
            .into_iter()
            .find(|id| id == requested)
            .ok_or_else(|| CaptureError::Os(format!("no camera of this device has id {requested:?}")));
    }

    for id in &ids {
        let is_back = manager
            .characteristics(id)
            .ok()
            .and_then(|characteristics| characteristics.lens_facing())
            == Some(LensFacing::Back);
        if is_back {
            return Ok(id.clone());
        }
    }

    Ok(ids.into_iter().next().expect("checked non-empty above"))
}

/// One camera's capture pipeline, as the core sees it.
///
/// The platform objects live on a thread of their own (see [`pump`]); this handle is the
/// channel to it. That split is what the other backends do too, and here it also keeps every
/// NDK call on one thread, which is the contract the framework is happiest with.
pub(crate) struct Handle {
    /// Requests to the capture thread.
    commands: Sender<Command>,
    /// The capture thread, joined by [`Handle::close`].
    pump: Option<JoinHandle<()>>,
}

// SAFETY: the handle is a channel sender and a join handle, both of which are `Send`; every
// platform object belongs to the capture thread, and the callbacks it registers never touch
// shared state.
unsafe impl Send for Handle {}

/// One request from the owning thread to the capture thread.
enum Command {
    /// Tells the capture thread whether a consumer is taking frames.
    ///
    /// Android's capture session writes into the encoder's surface whether or not anyone reads
    /// the result, so this cannot stop encoding the way macOS stops its own encoder; what it does
    /// is make a stream that starts now begin on a keyframe, so the core does not have to wait
    /// out an I-frame interval before the first decodable frame.
    SetEncoding {
        /// Whether a consumer is taking frames.
        enabled: bool,
    },
    /// Rebuild the pipeline with these capture and encoder parameters.
    SetParameters {
        /// Requested picture width in pixels.
        width: u32,
        /// Requested picture height in pixels.
        height: u32,
        /// Requested frame rate.
        fps: u32,
        /// Codec to encode with.
        codec: Codec,
        /// Target bitrate in kilobits per second.
        bitrate_kbps: u32,
        /// Where the outcome is reported.
        reply: SyncSender<Result<(), CaptureError>>,
    },
    /// Capture one still and deliver it through this callback.
    TakePhoto {
        /// The backend entry point to deliver the still through.
        cb: JpegCallback,
        /// The opaque pointer the callback is invoked with.
        user: SendPtr,
        /// Where the outcome is reported.
        reply: SyncSender<Result<(), CaptureError>>,
    },
    /// Stop capturing and leave the capture thread.
    Stop,
}

/// A `user` pointer on its way to the capture thread.
struct SendPtr(*mut c_void);

// SAFETY: the pointer is the `CallbackState` box `source.rs` keeps alive for the whole life of
// the `CaptureSource`, which outlives this backend; the capture thread only hands it back to
// the callbacks that were registered with it.
unsafe impl Send for SendPtr {}

/// How long the capture thread blocks for an encoded frame before looking at its commands.
///
/// It bounds how long teardown waits for the thread: a shorter timeout costs a wakeup every
/// few milliseconds, a longer one delays a shutdown.
const DEQUEUE_TIMEOUT_US: i64 = 50_000;

/// How long [`Handle::open`] waits for the capture thread to bring the pipeline up.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a still capture may take before the photo path gives up.
///
/// The one-shot request has to work its way through the camera's own settling (auto-exposure
/// and focus for the still) before the JPEG is written; five seconds is far past what a phone
/// needs and still bounds how long the capture thread — which stops draining the encoder while
/// it waits — can be held away from the video stream.
const STILL_TIMEOUT: Duration = Duration::from_secs(5);

/// What the capture thread needs to build the pipeline.
struct Setup {
    /// Capture and encoder parameters.
    cfg: CaptureConfig,
    /// Where encoded frames are delivered.
    frame_cb: FrameCallback,
    /// The opaque pointer the callbacks are invoked with.
    user: SendPtr,
}

impl Handle {
    /// Opens a camera and starts capturing.
    ///
    /// The work happens on the capture thread, so a failure there — no permission, an
    /// unsupported geometry, no encoder for the codec — is reported here as the error it was.
    /// Fails with [`CaptureError::PermissionDenied`] when this process does not hold the
    /// `CAMERA` permission, with [`CaptureError::UnsupportedParameters`] when the camera or the
    /// encoder cannot do what was asked, and with [`CaptureError::NoCamera`] when the device has
    /// no camera to open.
    pub(crate) fn open(
        cfg: &CaptureConfig,
        frame_cb: FrameCallback,
        _jpeg_cb: JpegCallback,
        user: *mut c_void,
    ) -> Result<Self, CaptureError> {
        let (commands, requests) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let setup = Setup {
            cfg: cfg.clone(),
            frame_cb,
            user: SendPtr(user),
        };

        let pump = thread::Builder::new()
            .name("scwc-capture-android".to_owned())
            .spawn(move || pump(setup, requests, ready_tx))
            .map_err(|error| {
                CaptureError::Os(format!("cannot start the capture thread: {error}"))
            })?;

        match ready_rx.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                commands,
                pump: Some(pump),
            }),
            Ok(Err(error)) => {
                let _ = pump.join();
                Err(error)
            }
            Err(error) => {
                let _ = pump.join();
                Err(CaptureError::Os(format!(
                    "the capture thread did not report ready: {error}"
                )))
            }
        }
    }

    /// Reconfigures the pipeline for a `switch_camera` command.
    ///
    /// The session and the encoder cannot change geometry while they run, so the capture thread
    /// tears the stream down and builds a new one on the same open device. Blocks until that
    /// has happened — at most one frame — and reports the camera's answer, which is what the
    /// core turns into `ack {ok: true/false}`.
    pub(crate) fn set_parameters(
        &self,
        width: u32,
        height: u32,
        fps: u32,
        codec: Codec,
        bitrate_kbps: u32,
    ) -> Result<(), CaptureError> {
        let (reply, outcome) = mpsc::sync_channel(1);
        self.send(Command::SetParameters {
            width,
            height,
            fps,
            codec,
            bitrate_kbps,
            reply,
        });
        outcome.recv().unwrap_or(Err(CaptureError::ChannelClosed))
    }

    /// Captures one still, delivered through `jpeg_cb` before this function returns.
    ///
    /// Blocks for the camera's still sequence, which the core allows: it reconfigures and takes
    /// photos at frame boundaries anyway. A photo the camera cannot produce — no JPEG output in
    /// this session — comes back as [`CaptureError::UnsupportedParameters`].
    pub(crate) fn take_photo(
        &self,
        jpeg_cb: JpegCallback,
        user: *mut c_void,
    ) -> Result<(), CaptureError> {
        let (reply, outcome) = mpsc::sync_channel(1);
        self.send(Command::TakePhoto {
            cb: jpeg_cb,
            user: SendPtr(user),
            reply,
        });
        outcome.recv().unwrap_or(Err(CaptureError::ChannelClosed))
    }

    /// Hands a command to the capture thread.
    ///
    /// Nothing wakes the thread: it looks at its channel every
    /// [`DEQUEUE_TIMEOUT_US`](DEQUEUE_TIMEOUT_US) microseconds while waiting for the encoder,
    /// so a command is picked up within that, and the platform keeps every call on the one
    /// thread that owns the objects.
    fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            debug!("the capture thread is gone; the command is dropped with it");
        }
    }

    /// Tells the capture thread whether a consumer is taking frames.
    ///
    /// Returns without waiting: the capture thread picks the request up at its next loop
    /// boundary, and nothing in the caller depends on the keyframe having been produced yet.
    pub(crate) fn set_encoding(&self, enabled: bool) {
        self.send(Command::SetEncoding { enabled });
    }

    /// Stops capturing and releases the camera.
    pub(crate) fn close(&mut self) {
        // A send failure means the thread already left, which is not an error here.
        self.send(Command::Stop);
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.close();
    }
}

/// The capture thread: builds the pipeline, then moves frames from the encoder to the core.
///
/// Everything the framework holds a pointer to lives in `pipeline` for the whole loop, and is
/// dropped — in declaration order — when the loop ends, after the session has stopped feeding
/// the encoder.
fn pump(setup: Setup, requests: Receiver<Command>, ready: SyncSender<Result<(), CaptureError>>) {
    let mut pipeline = match Pipeline::open(&setup) {
        Ok(pipeline) => pipeline,
        Err(error) => {
            // The opening thread is waiting for exactly this.
            let _ = ready.send(Err(error));
            return;
        }
    };

    if ready.send(Ok(())).is_err() {
        // The opener gave up; the pipeline still has to be torn down, which the drop does.
        return;
    }

    loop {
        match requests.try_recv() {
            Ok(Command::Stop) => break,
            Ok(Command::SetParameters {
                width,
                height,
                fps,
                codec,
                bitrate_kbps,
                reply,
            }) => {
                let cfg = CaptureConfig {
                    camera_id: pipeline.cfg.camera_id.clone(),
                    width,
                    height,
                    fps,
                    codec,
                    bitrate_kbps,
                };
                let outcome = pipeline.rebuild(&cfg);
                let _ = reply.send(outcome);
            }
            Ok(Command::SetEncoding { enabled }) => {
                if let Some(stream) = pipeline.stream.as_mut() {
                    // The codec stops producing output while no consumer reads it; the camera
                    // keeps rendering into the encoder's surface either way, because the
                    // session's outputs cannot change under a running request.
                    stream.encoder.set_suspended(!enabled);
                    if enabled {
                        // A stream that starts now cannot decode until the next IDR, and the
                        // resume itself may not have produced one.
                        stream.encoder.request_keyframe();
                    }
                }
            }
            Ok(Command::TakePhoto { cb, user, reply }) => {
                let outcome = pipeline.take_photo(cb, user.0);
                let _ = reply.send(outcome);
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => break,
        }

        // A pipeline whose last rebuild failed has nothing left to pull frames from; leaving
        // the loop closes the command channel, which is how the source learns capture stopped.
        let Some(stream) = pipeline.stream.as_mut() else {
            warn!("the Android capture stream is down; the client will report no more frames");
            break;
        };

        // The preview reads whatever the camera last wrote into its window. It takes at most one
        // frame per loop and converts nothing while the UI shows no preview, so it neither waits
        // for the encoder nor costs a pass nobody asked for.
        if let Some(preview) = stream.preview.as_mut() {
            preview.publish();
        }

        match stream.encoder.next_access_unit(DEQUEUE_TIMEOUT_US) {
            Ok(Some(unit)) => {
                // SAFETY: `user` is the `CallbackState` box `source.rs` keeps alive for as long
                // as this backend, and the bytes live until `unit` is dropped.
                unsafe {
                    (setup.frame_cb)(
                        setup.user.0,
                        unit.data.as_ptr(),
                        unit.data.len(),
                        u8::from(unit.is_keyframe),
                    )
                };
            }
            // Nothing encoded within the timeout: look at the commands again.
            Ok(None) => {}
            Err(error) => {
                warn!(%error, "the Android encoder stopped producing frames");
                break;
            }
        }
    }

    debug!("leaving the Android capture thread");
}

/// One capture stream: the session, the encoder it feeds, and the still reader alongside them.
///
/// Field order is the drop order: the session has to stop feeding both surfaces before either
/// goes away, the encoder gives its input surface back when it stops, and the still reader owns
/// the window the session's still output pointed at, so it outlives the session as well.
struct Stream {
    /// The running capture session.
    #[allow(dead_code)]
    session: session::Session,
    /// The interface's own preview window, while this session renders into it.
    ///
    /// Placed after `session` on purpose: the session's outputs are what reference the window,
    /// and they are freed by `Session`'s `Drop` before the reference this holds goes.
    ///
    /// Never read: the lease is kept for what it owns — one reference on the window, given back
    /// when this stream is dropped and not before.
    #[allow(dead_code)]
    external: Option<preview_surface::Lease>,
    /// The hardware encoder the session fills.
    encoder: media::Encoder,
    /// The JPEG reader `take_photo` reads from, when the camera accepted one.
    still: Option<StillCapture>,
    /// The YUV reader the preview frames come from, when the camera accepted one.
    preview: Option<PreviewCapture>,
}

impl Stream {
    /// Brings up the chain: encoder, still reader, preview, session.
    ///
    /// The preview is the interface's own window when it handed one over, and a YUV reader of
    /// this backend's otherwise — see [`Self::start_session`] for the order the outputs are
    /// surrendered in.
    fn start(
        device: &mut camera2::Device,
        cfg: &CaptureConfig,
        characteristics: &Metadata,
    ) -> Result<Self, CaptureError> {
        // The encoder first: the session needs the surface its codec was configured with, and
        // starting the codec before the camera writes into it is what keeps the first frames
        // from arriving at a codec that is not running yet.
        let mut encoder = media::Encoder::create(
            cfg.codec,
            cfg.width,
            cfg.height,
            cfg.fps,
            cfg.bitrate_kbps,
        )?;
        encoder.start()?;
        // A fresh stream has no consumer: the core turns encoding on when a stream becomes
        // active, and until then the codec only consumes what the camera renders into it. The
        // camera has to keep rendering — the session's outputs are fixed — so this is the only
        // way an idle pipeline does not pay for an encoder.
        encoder.set_suspended(true);

        // The still output is best-effort: a device that cannot bring up a JPEG reader loses
        // `take_photo`, not the stream, and so does a camera that refuses to carry a still
        // output beside the video one (the stream combination is a device property).
        let mut still = match StillCapture::new(cfg.width, cfg.height) {
            Ok(still) => Some(still),
            Err(error) => {
                warn!(%error, "no still output for this Android stream; take_photo will refuse");
                None
            }
        };
        // The preview output is best-effort for the same reason: a camera that will not carry a
        // third output loses the interface's picture, not the stream.
        //
        // The interface's own window, when it handed one over, comes first: the camera renders
        // into it with every capture request, so the picture costs no conversion, no copy and no
        // per-frame work at all — the engine owns the texture and the notice that a newer frame
        // is waiting. The reader below is the fallback, and stays the only path while the
        // interface handed over no window.
        let rotation = rotation_degrees(&characteristics);
        let mut external = match preview_surface::lease() {
            Some(mut lease) => {
                // The size the camera renders at is the reader's cap and shape: the window is a
                // preview like any other, and a larger one would only cost camera bandwidth.
                let (width, height) = preview::preview_size(cfg.width, cfg.height);
                match lease.set_size(width, height) {
                    Ok(()) => Some(lease),
                    Err(error) => {
                        warn!(%error, width, height, "the interface's preview window will not take this size; the preview will come from a YUV reader");
                        None
                    }
                }
            }
            None => None,
        };
        let mut preview = if external.is_some() {
            // The session carries one preview output; a camera that is already rendering into
            // the interface's window has no need of a reader, and asking for both is a
            // combination far more cameras refuse.
            None
        } else {
            match PreviewCapture::new(cfg.width, cfg.height, rotation) {
                Ok(preview) => Some(preview),
                Err(error) => {
                    warn!(%error, "no preview output for this Android stream; the preview will stay empty");
                    None
                }
            }
        };

        let session = Self::start_session(
            device,
            encoder.input_surface(),
            &mut external,
            &mut preview,
            &mut still,
            cfg.fps,
        )?;

        // Which of the two the picture comes from is what a run's log has to answer: the two
        // paths cost nothing alike, and only one of them is the interface's own texture.
        preview_surface::set_rotation(rotation);
        preview_surface::set_active(external.is_some());
        if external.is_some() {
            info!("the Android preview is rendering into the interface's own texture");
        } else if preview.is_some() {
            info!("the Android preview is converting YUV frames");
        } else {
            warn!("the Android stream carries no preview output at all");
        }

        Ok(Self {
            session,
            external,
            encoder,
            still,
            preview,
        })
    }

    /// One attempt at the session, with whichever of the three optional outputs the caller still
    /// holds.
    ///
    /// The preview window is the interface's own while it holds one, and the reader's otherwise:
    /// a session carries one preview output, and it is named by whichever the stream kept.
    fn try_session(
        device: &mut camera2::Device,
        surface: *mut ANativeWindow,
        external: &Option<preview_surface::Lease>,
        preview: &Option<PreviewCapture>,
        still: &Option<StillCapture>,
        fps: u32,
    ) -> Result<session::Session, CaptureError> {
        session::Session::start(
            device,
            surface,
            still.as_ref().map(StillCapture::window),
            external
                .as_ref()
                .map(preview_surface::Lease::window)
                .or_else(|| preview.as_ref().map(PreviewCapture::window)),
            fps,
        )
    }

    /// Brings the session up, surrendering the outputs the camera refuses one at a time.
    ///
    /// A camera may refuse the combination of outputs while accepting a subset of them, and the
    /// stream is the thing that must survive. The order is fixed: the preview first — the
    /// interface's window before a reader of our own — then the still, and the encoder alone is
    /// the combination every camera carries. `external`, `preview` and `still` are left holding
    /// whatever the camera accepted, so the stream's fields describe the session it runs on.
    fn start_session(
        device: &mut camera2::Device,
        surface: *mut ANativeWindow,
        external: &mut Option<preview_surface::Lease>,
        preview: &mut Option<PreviewCapture>,
        still: &mut Option<StillCapture>,
        fps: u32,
    ) -> Result<session::Session, CaptureError> {
        let mut attempt = Self::try_session(device, surface, external, preview, still, fps);
        if let Err(error) = attempt {
            if external.take().is_some() {
                warn!(%error, "the camera refused the interface's preview window; trying a YUV reader");
                attempt = Self::try_session(device, surface, external, preview, still, fps);
            } else {
                attempt = Err(error);
            }
        }
        if let Err(error) = attempt {
            if preview.take().is_some() {
                warn!(%error, "the camera refused the preview output; rebuilding the session without it");
                attempt = Self::try_session(device, surface, external, preview, still, fps);
            } else {
                attempt = Err(error);
            }
        }
        if let Err(error) = attempt {
            if still.take().is_some() {
                warn!(%error, "the camera refused the still output too; rebuilding the session with the encoder alone");
                attempt = Self::try_session(device, surface, external, preview, still, fps);
            } else {
                attempt = Err(error);
            }
        }
        attempt
    }
}

/// The camera-side objects the capture thread owns, and the stream running on them.
///
/// Field order is the drop order, and it is not arbitrary: the stream has to stop before the
/// device that drives it is closed, and the manager and the callbacks outlive everything,
/// because the framework keeps pointers to both for as long as the device is open.
struct Pipeline {
    /// The running stream, or `None` while a rebuild is in flight or after one failed.
    stream: Option<Stream>,
    /// The parameters the running stream was configured with.
    cfg: CaptureConfig,
    /// The opened camera.
    #[allow(dead_code)]
    device: camera2::Device,
    /// The callbacks the framework holds for the lifetime of the device.
    #[allow(dead_code)]
    callbacks: Box<ACameraDevice_StateCallbacks>,
    /// The manager the device was opened through.
    #[allow(dead_code)]
    manager: Manager,
    /// The camera's characteristics, kept for the capability check on reconfiguration.
    characteristics: Metadata,
}

impl Pipeline {
    /// Brings up the whole chain: camera, encoder, session.
    fn open(setup: &Setup) -> Result<Self, CaptureError> {
        let cfg = &setup.cfg;
        let manager = Manager::create()?;
        let camera_id = resolve_camera_id(&manager, cfg.camera_id.as_deref())?;
        let characteristics = manager.characteristics(&camera_id)?;
        let matrix = CapabilityMatrix::read(&characteristics);

        info!(
            camera = %camera_id,
            resolutions = matrix.presets.len(),
            ae_ranges = ?matrix.ae_ranges,
            orientation = characteristics.sensor_orientation(),
            level = ?characteristics.hardware_level(),
            "camera capability matrix"
        );
        for (preset, rates) in &matrix.presets {
            debug!(resolution = %preset, framerates = ?rates, "announced stream configuration");
        }
        matrix.check(cfg.width, cfg.height, cfg.fps)?;
        if !matrix.ae_ranges.is_empty() && !matrix.ae_ranges.contains(&(cfg.fps, cfg.fps)) {
            warn!(
                fps = cfg.fps,
                announced = ?matrix.ae_ranges,
                "the camera does not advertise this rate as a constant frame rate"
            );
        }

        // The call that proves the process may use a camera at all.
        let mut callbacks = Box::new(ACameraDevice_StateCallbacks {
            context: std::ptr::null_mut(),
            on_disconnected: Some(on_disconnected),
            on_error: Some(on_error),
        });
        let mut device = manager.open(&camera_id, &mut callbacks)?;
        let stream = Stream::start(&mut device, cfg, &characteristics)?;

        let pipeline = Self {
            stream: Some(stream),
            cfg: cfg.clone(),
            device,
            callbacks,
            manager,
            characteristics,
        };

        info!(
            camera = %camera_id,
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            codec = ?cfg.codec,
            "android capture pipeline running"
        );

        Ok(pipeline)
    }

    /// Rebuilds the running stream for `switch_camera`, restoring it if the new parameters fail.
    ///
    /// The camera carries one session at a time, so the running stream goes before the new one
    /// is built. A rebuild that cannot come back up — neither with the new parameters nor with
    /// the previous ones — leaves [`Pipeline::stream`] empty and the capture thread ends, so the
    /// source reports that capture stopped instead of pretending to stream.
    fn rebuild(&mut self, cfg: &CaptureConfig) -> Result<(), CaptureError> {
        // Check the request against what the camera announced before touching the stream: a
        // geometry it never published cannot start a session, and refusing it here keeps the
        // running stream exactly as the server believes it is.
        let matrix = CapabilityMatrix::read(&self.characteristics);
        matrix.check(cfg.width, cfg.height, cfg.fps)?;

        let previous = self.cfg.clone();
        self.stream = None;
        match Stream::start(&mut self.device, cfg, &self.characteristics) {
            Ok(stream) => {
                self.stream = Some(stream);
                self.cfg = cfg.clone();
                info!(
                    width = cfg.width,
                    height = cfg.height,
                    fps = cfg.fps,
                    codec = ?cfg.codec,
                    bitrate_kbps = cfg.bitrate_kbps,
                    "android capture reconfigured"
                );
                Ok(())
            }
            Err(error) => {
                warn!(%error, "the requested Android capture parameters failed; restoring the previous stream");
                match Stream::start(&mut self.device, &previous, &self.characteristics) {
                    Ok(stream) => {
                        self.stream = Some(stream);
                    }
                    Err(restore) => {
                        error!(%restore, "the previous Android capture parameters no longer work either");
                    }
                }
                Err(error)
            }
        }
    }

    /// Captures one still and delivers the JPEG through `cb`.
    fn take_photo(&mut self, cb: JpegCallback, user: *mut c_void) -> Result<(), CaptureError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(CaptureError::Os(
                "the Android capture stream is down".to_owned(),
            ));
        };
        let Stream {
            session,
            still: reader,
            ..
        } = stream;
        let Some(reader) = reader.as_mut() else {
            return Err(CaptureError::UnsupportedParameters);
        };

        // Anything the camera wrote for an earlier photo but nobody collected is dropped first,
        // so the JPEG handed to the callback below is the one this call asked for.
        reader.drain();
        session.capture_still()?;
        let jpeg = reader.take_jpeg(STILL_TIMEOUT)?;
        // SAFETY: `user` is the `CallbackState` box `source.rs` keeps alive for as long as this
        // backend, and the callback copies the bytes before returning.
        unsafe { cb(user, jpeg.as_ptr(), jpeg.len()) };
        debug!(bytes = jpeg.len(), "still delivered");
        Ok(())
    }
}

/// The framework took the camera away: another client outranks this one, or it was
/// unplugged.
///
/// The device pointer is deliberately unused: this increment registers no shared state, and
/// the callback can still fire while the handle is being dropped, so touching anything
/// through `context` would risk a use-after-free. The pump thread in the next increment
/// owns the real notification.
unsafe extern "C" fn on_disconnected(_context: *mut c_void, _device: *mut camera2::ACameraDevice) {
    warn!("the camera was disconnected; no further frames will arrive");
}

/// The device failed beyond recovery and has to be reopened.
unsafe extern "C" fn on_error(
    _context: *mut c_void,
    _device: *mut camera2::ACameraDevice,
    error: i32,
) {
    warn!(error, "the camera reported a fatal error");
}
