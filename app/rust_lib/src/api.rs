//! Flutter-facing API of the device client.
//!
//! Every entry point here is a thin bridge over [`scwc_core`] and [`scwc_capture`]: the core
//! owns registration, the session, control and the media path, so this module only translates
//! Dart values into core values, owns the runtime the agent runs on, and streams progress back.
//!
//! # Events
//!
//! [`AgentEvent`] has two sources:
//!
//! - the core's event hook, which reports its register → attach → session cycle and the failures
//!   it survives:
//!   - [`AgentEvent::Reconnecting`] — the core is backing off before another registration
//!     attempt; the attempt counts up since the last attach that worked.
//!   - [`AgentEvent::Warn`] — a failure the core reported, either through its hook or as a
//!     `tracing` record the subscriber below forwards.
//!   - [`AgentEvent::ParametersApplied`] — the parameter set the capture pipeline is actually
//!     running, which is also how the set a session opens with reaches the UI.
//!   - [`AgentEvent::Notice`] — a request the core had to postpone, a parameter switch or a
//!     reconnect that waits for the stream that was live when it arrived.
//! - what this module observes itself:
//!   - [`AgentEvent::Connecting`] — emitted when a run starts, and on every registration the core
//!     begins.
//!   - [`AgentEvent::Online`] — emitted when the first encoded frame is pulled through the
//!     counting source. The core pulls frames only for a stream the server announced, so that
//!     first frame is the earliest point at which the registration was accepted, the attach
//!     succeeded and a stream is live.
//!   - [`AgentEvent::Stats`] — once a second while the agent runs.
//!   - [`AgentEvent::Error`] — [`run`] returning an error, which means the client cannot go on
//!     (a configuration that can never work); transport failures are retried inside the core.
//!     The agent task ending without such an error — a panic inside the runtime — arrives here
//!     too, so it cannot go unnoticed.
//!
//! `Recording` and `RecordingStopped` are part of the API but are not emitted yet: stream ids are
//! assigned inside the core's control state and never leave it.
//!
//! # Configuration
//!
//! The server identity is not part of the running agent alone: [`save_server_config`] writes it to
//! `<config dir>/sc-webcam/server.json` — next to the capture parameters the core persists — and
//! [`load_server_config`] reads it back, which is what lets a fresh launch connect on its own.
//!
//! # Driving a running agent
//!
//! [`apply_capture_params`] and [`save_server_config`] send the running agent the two requests the
//! core knows how to route: a parameter switch travels the `switch_camera` path (applied at once
//! while no stream is live, at the end of the active one otherwise), and a reconnect registers
//! again with the saved credentials — immediately, unless a stream is live.
//!
//! # Logging
//!
//! Every crate logs through `tracing`, which is a no-op while no subscriber is installed. The
//! first [`start_agent`] installs a process-wide one that appends to a file in the system
//! temporary directory and forwards warnings and errors of the client crates into the event
//! stream.
//!
//! # Preview
//!
//! [`preview_stream`] carries the camera picture while an agent runs: the capture path converts
//! every frame to RGBA and scales it aspect-locked to the size the UI last set through
//! [`set_preview_target_size`], and this module hands those frames to every live Dart stream. The
//! picture is consumer-paced: only the newest frame is kept, and it is handed over once the UI
//! reports through [`preview_frame_consumed`] that it is done with the previous one, so a UI that
//! cannot keep up with the camera drops frames instead of accumulating delay. The sink is
//! registered with the run and dropped again when it ends, so a stopped agent does no conversion
//! work at all.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Once};
use std::time::Duration;

use flutter_rust_bridge::frb;
use scwc_capture::{CaptureConfig, CaptureSource, enumerate_cameras, open};
use scwc_core::preview::PreviewFrame as CorePreviewFrame;
use scwc_core::{
    ClientConfig, CoreEvent, EncodedFrame, FrameSource, ServerConfig as CoreServerConfig,
    SwitchParams, UiCommand, default_bitrate_kbps, run,
};
use scwc_protocol::{CameraRegistration, Codec};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, layer::Context};

use crate::frb_generated::StreamSink;

/// One camera of the host, as the operator picks it in the UI.
#[derive(Debug)]
pub struct CameraInfoDto {
    /// Stable device identifier, handed back to [`AgentConfig::camera_id`].
    pub id: String,
    /// Human-readable device name.
    pub name: String,
    /// Announced `(resolution, frame rates)` pairs, largest resolution first.
    pub presets: Vec<(String, Vec<u32>)>,
}

/// Everything the UI has to know to start the agent.
#[derive(Debug)]
pub struct AgentConfig {
    /// Server base URL, for example `https://smartclass.example.edu`.
    pub server: String,
    /// Device id the token was issued for.
    pub device_id: String,
    /// Device bearer token (`wdt_...`).
    pub token: String,
    /// Camera to capture from, from [`CameraInfoDto::id`].
    pub camera_id: String,
    /// Capture width in pixels.
    pub width: u32,
    /// Capture height in pixels.
    pub height: u32,
    /// Capture frame rate.
    pub fps: u32,
    /// `h264` or `h265`, exactly as the protocol spells them.
    pub codec: String,
    /// Encoder bitrate in kilobits per second; `None` derives one from the codec, the resolution
    /// and the frame rate, which is what an empty bitrate field in the UI means.
    pub bitrate_kbps: Option<u32>,
}

/// One live preview frame of the camera, ready to be turned into a picture.
///
/// The capture path converts and scales on the Rust side, so the UI only has to wrap the pixels
/// in an image and centre it inside its preview area.
#[derive(Debug, Clone)]
pub struct PreviewFrame {
    /// Row-major pixels without padding, four bytes each: red, green, blue, alpha.
    pub rgba: Vec<u8>,
    /// Frame width in pixels, after the scaling the UI asked for.
    pub width: u32,
    /// Frame height in pixels, after the scaling the UI asked for.
    pub height: u32,
}

/// The server identity the client registers with, as the UI shows and stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Server base URL, for example `https://smartclass.example.edu`.
    pub server: String,
    /// Device id the token was issued for.
    pub device_id: String,
    /// Device bearer token (`wdt_...`).
    pub token: String,
}

impl ServerConfig {
    /// The same identity in the vocabulary the core speaks; the field name differs only because
    /// the UI calls the credential what the operator calls it.
    fn into_core(self) -> CoreServerConfig {
        CoreServerConfig {
            server: self.server,
            device_id: self.device_id,
            device_token: self.token,
        }
    }
}

/// Progress of the background agent.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A run started and is registering with the server.
    Connecting,
    /// The first frame of a live stream reached the core.
    Online,
    /// The server started a stream over this connection.
    Recording {
        /// Stream the server announced.
        stream_id: String,
    },
    /// The server's stream ended.
    RecordingStopped,
    /// The core lost its connection and is registering again.
    Reconnecting {
        /// Reconnect attempts since the last attach that worked, counting from one.
        attempt: u32,
    },
    /// A failure the client survived; it keeps capturing.
    Warn {
        /// Human-readable message.
        message: String,
    },
    /// The capture pipeline is now running this parameter set.
    ///
    /// Emitted for every switch and for the set a session opens with, so the UI can show what the
    /// camera actually runs instead of what it asked for. The agent drives one camera, so the
    /// set is not attributed.
    ParametersApplied {
        /// Protocol resolution, for example `1920x1080`.
        resolution: String,
        /// Frame rate in frames per second.
        fps: u32,
        /// Codec the frames are encoded with, spelled as the protocol spells it (`h264`).
        codec: String,
        /// Encoder bitrate in kilobits per second.
        bitrate_kbps: u32,
    },
    /// Something the client postponed or wants the operator to know that is not a failure.
    Notice {
        /// Human-readable message.
        message: String,
    },
    /// Frame counters, reported once a second while the agent runs.
    Stats {
        /// Encoded frames handed to the core since the agent started.
        sent_frames: u64,
        /// Frames dropped, which happens inside the core's own media queue.
        dropped_frames: u64,
    },
    /// The agent stopped because the client cannot go on.
    Error {
        /// Human-readable reason.
        message: String,
    },
}

/// Lists the cameras of the host.
///
/// Synchronous and cheap: nothing here opens a capture session. Fails with the platform's own
/// message when the host has no camera or the process may not use one.
#[frb(sync)]
pub fn list_cameras() -> Result<Vec<CameraInfoDto>, String> {
    enumerate_cameras()
        .map(|cameras| {
            cameras
                .into_iter()
                .map(|camera| CameraInfoDto {
                    id: camera.id,
                    name: camera.name,
                    presets: camera.presets,
                })
                .collect()
        })
        .map_err(|error| error.to_string())
}

/// Starts the device client in the background.
///
/// Returns as soon as the agent is scheduled. Fails when an agent already runs, or when the
/// configuration can never work: an empty device id or token, an unparsable server URL, an
/// unsupported codec name, or no camera at all. Everything that can fail later — the camera not
/// opening, the server rejecting the token — arrives as an [`AgentEvent`] the agent retries
/// through, and the client only stops when [`stop_agent`] asks it to.
pub async fn start_agent(config: AgentConfig) -> Result<(), String> {
    init_logging();

    let codec = parse_codec(&config.codec)?;
    let resolution = format!("{}x{}", config.width, config.height);
    let (supported_resolutions, supported_framerates) =
        capabilities(&config.camera_id, &resolution, config.fps);
    // VideoToolbox, Media Foundation and VAAPI encode both; the configured codec stays first as
    // the preferred one.
    let supported_codec = match codec {
        Codec::H265 => vec![Codec::H265, Codec::H264],
        _ => vec![Codec::H264, Codec::H265],
    };
    // An empty bitrate field means "whatever the stream needs": the protocol has no bitrate, so
    // the client derives the one the core would apply from the same three parameters.
    let bitrate_kbps = config
        .bitrate_kbps
        .unwrap_or_else(|| default_bitrate_kbps(codec, &resolution, config.fps));

    let (commands_tx, commands_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut client_config = ClientConfig::new(
        config.server,
        config.device_id,
        config.token,
        vec![CameraRegistration {
            camera_enum: 0,
            resolution,
            fps: config.fps,
            supported_resolutions,
            supported_framerates,
            supported_codec,
            attrs: Default::default(),
        }],
    );
    // A parameter switch the server asked for survives a restart of the agent, so the next
    // session starts from it instead of from the announced defaults.
    client_config.state_path = state_path();
    // What the UI sends while the agent runs: parameter switches and requests to register again.
    client_config.commands = Some(commands_rx);
    client_config.validate().map_err(|error| error.to_string())?;

    let capture_config = CaptureConfig {
        camera_id: Some(config.camera_id),
        width: config.width,
        height: config.height,
        fps: config.fps,
        codec,
        bitrate_kbps,
    };

    // Started before the slot is reserved, so a runtime that cannot start never leaves the slot
    // looking busy.
    let runtime = runtime()?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (generation, previous) = {
        let mut slot = AGENT.lock().unwrap_or_else(|error| error.into_inner());
        if slot.shutdown.is_some() {
            return Err("an agent is already running".to_owned());
        }
        slot.generation += 1;
        slot.shutdown = Some(shutdown_tx);
        slot.commands = Some(commands_tx);
        (slot.generation, slot.task.take())
    };

    // A previous agent that is still winding down may still hold the camera, so wait for it
    // before opening the device again.
    if let Some(task) = previous {
        let _ = task.await;
    }

    // The core reports its lifecycle here; the forwarder below turns that into the events the UI
    // already knows. It ends on its own once the run task drops its configuration.
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel::<CoreEvent>();
    client_config.events = Some(events_tx);
    RECONNECT_ATTEMPTS.store(0, Ordering::Relaxed);
    runtime.spawn(forward_core_events(events_rx));

    // The capture path hands every frame it converts to this sink; the forwarding task turns those
    // into the frames the UI paints. Registered after the previous run was waited for above, so a
    // run that is still winding down cannot clear this one's sink.
    let (preview_tx, preview_rx) = tokio::sync::mpsc::unbounded_channel();
    scwc_core::preview::set_preview_sink(Some(preview_tx));
    runtime.spawn(forward_preview_frames(preview_rx));
    // The first frame of a run needs no consumption report: nothing of this run is in flight yet.
    PREVIEW_READY.notify_one();

    let frames = Arc::new(AtomicU64::new(0));
    let agent = runtime.spawn(async move {
        emit(AgentEvent::Connecting);
        let stats = tokio::spawn(report_stats(Arc::clone(&frames)));

        let result = match open_camera(&capture_config, &shutdown_rx, &frames).await {
            // Asked to stop before the camera could be opened; there is nothing to run.
            None => Ok(()),
            Some(source) => {
                let mut stopped = shutdown_rx.clone();
                let shutdown = async move {
                    let _ = stopped.changed().await;
                };
                run(client_config, source, shutdown)
                    .await
                    .map_err(|error| error.to_string())
            }
        };

        stats.abort();
        if let Err(message) = result {
            emit(AgentEvent::Error { message });
        }

        let mut slot = AGENT.lock().unwrap_or_else(|error| error.into_inner());
        if slot.generation == generation {
            slot.shutdown = None;
            slot.commands = None;
            // The camera is on its way down, so nothing more can be shown; dropping the sender also
            // ends the forwarding task above.
            scwc_core::preview::set_preview_sink(None);
        }
    });

    // The run task's own result is waited for below, by the next start, and only to know when the
    // camera is free again; a panic in it would otherwise leave the UI believing the agent is
    // alive. This monitor is what the slot waits for, and it reports such an ending.
    let task = runtime.spawn(async move {
        if let Err(error) = agent.await {
            emit(AgentEvent::Error {
                message: format!("the agent task ended unexpectedly: {error}"),
            });
        }
    });

    AGENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .task = Some(task);
    Ok(())
}

/// Stops the background agent, if one runs.
///
/// Returns without waiting: the loop notices the signal at its next await point, and a later
/// [`start_agent`] waits for it to wind down before touching the camera again. It also ends the
/// camera retries and any wait for the server, so nothing brings a stopped agent back.
pub fn stop_agent() {
    // The capture path stops converting as soon as the run is asked to stop, rather than when it
    // has finished winding down; the run clears the sink again when it ends.
    scwc_core::preview::set_preview_sink(None);
    let mut slot = AGENT.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(shutdown) = slot.shutdown.take() {
        let _ = shutdown.send(true);
    }
}

/// Moves the running agent's capture pipeline onto new parameters.
///
/// The switch travels the same path a `switch_camera` command from the server does: applied at
/// once while no stream is live, and once the active stream ends otherwise, the last request
/// winning. An empty `bitrate_kbps` — a bitrate field the operator left alone — lets the client
/// derive one from the codec, the resolution and the frame rate. Fails when no agent runs, and
/// when the codec is not one this client encodes.
#[frb(sync)]
pub fn apply_capture_params(
    resolution: String,
    fps: u32,
    codec: String,
    bitrate_kbps: Option<u32>,
) -> Result<(), String> {
    let codec = parse_codec(&codec)?;
    send_command(UiCommand::SwitchParams(SwitchParams {
        camera_enum: 0,
        resolution: Some(resolution),
        fps: Some(fps),
        codec: Some(codec),
        bitrate_kbps,
    }))
}

/// Stores the server identity and asks a running agent to register again with it.
///
/// The identity lives in `<config dir>/sc-webcam/server.json`, next to the capture parameters the
/// core persists, and is always written: an agent that is not running picks it up at its next
/// [`start_agent`]. A running agent reconnects at the first opportunity — right away while no
/// stream is live, and once the live one ends otherwise — so a recording is never cut short by a
/// configuration change.
#[frb(sync)]
pub fn save_server_config(server: String, device_id: String, token: String) -> Result<(), String> {
    let config = ServerConfig {
        server,
        device_id,
        token,
    };
    write_server_config(&config)?;

    // Cloned out of the slot so the lock is not held while the command is handed over; whether an
    // agent is running is not this call's business, the file is written either way.
    let commands = AGENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .commands
        .clone();
    if let Some(commands) = commands {
        let _ = commands.send(UiCommand::Reconnect(config.into_core()));
    }

    Ok(())
}

/// The server identity the client was last configured with.
///
/// `None` means there is none yet — a fresh install — or the stored one cannot be read. This is
/// what lets a launch connect without a click: the UI fills its fields from it and starts the
/// agent.
#[frb(sync)]
pub fn load_server_config() -> Option<ServerConfig> {
    read_server_config()
}

/// Hands `command` to the running agent.
///
/// Fails when no agent runs: a switch has a pipeline to move, so a command nobody can act on is
/// reported rather than dropped silently.
fn send_command(command: UiCommand) -> Result<(), String> {
    let commands = AGENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .commands
        .clone();
    match commands {
        Some(commands) => commands
            .send(command)
            .map_err(|_| "the running agent is gone".to_owned()),
        None => Err("no agent is running".to_owned()),
    }
}

/// Opens the camera, retrying until it works or the agent is asked to stop.
///
/// `open` starts the capture session, which can block for as long as the platform takes to bring a
/// camera up (and, on macOS, to ask for camera access). It also fails for reasons that pass on
/// their own — another client holding the device, a camera the system is still settling — so the
/// agent reports every attempt and waits before the next one instead of ending the run. `None`
/// means the agent was asked to stop while the camera was not up yet.
async fn open_camera(
    config: &CaptureConfig,
    shutdown: &watch::Receiver<bool>,
    frames: &Arc<AtomicU64>,
) -> Option<CountingSource<CaptureSource>> {
    let mut backoff = OPEN_BACKOFF_FIRST;
    loop {
        match open(config.clone()) {
            Ok(source) => {
                return Some(CountingSource {
                    inner: source,
                    frames: Arc::clone(frames),
                });
            }
            Err(error) => {
                let delay = backoff;
                backoff = (backoff * 2).min(OPEN_BACKOFF_MAX);
                emit(AgentEvent::Warn {
                    message: format!(
                        "could not open the camera: {error}; retrying in {}s",
                        delay.as_secs()
                    ),
                });
                if !retry_after(delay, shutdown).await {
                    return None;
                }
            }
        }
    }
}

/// Sleeps for `delay`, reporting `false` when the agent was asked to stop instead.
async fn retry_after(delay: Duration, shutdown: &watch::Receiver<bool>) -> bool {
    let mut shutdown = shutdown.clone();
    tokio::select! {
        _ = sleep(delay) => true,
        _ = shutdown.changed() => false,
    }
}

/// Subscribes to [`AgentEvent`]s; the returned Dart stream is the agent's progress.
///
/// Only events emitted after the call are delivered, and a subscription that Dart cancelled is
/// dropped at the next event.
pub fn agent_events(sink: StreamSink<AgentEvent>) {
    SUBSCRIBERS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(sink);
}

/// Subscribes to live preview frames; the returned Dart stream is the camera picture.
///
/// Frames arrive only while an agent runs and a target size is set. The picture is consumer-paced:
/// a frame is handed over only after the UI reported the previous one through
/// [`preview_frame_consumed`], so a subscription that decodes slower than the camera captures drops
/// frames rather than falling further behind on each of them. Only frames captured after the call
/// are delivered, and a subscription that Dart cancelled is dropped at the next frame.
pub fn preview_stream(sink: StreamSink<PreviewFrame>) {
    PREVIEW_SUBSCRIBERS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(sink);
    // Nothing is in flight for a stream that just subscribed, so its first frame needs no report.
    PREVIEW_READY.notify_one();
}

/// Reports that the UI is done with the frame it was last handed, so the next one may go out.
///
/// This is what paces the picture — see [`preview_stream`] — so the UI calls it once per frame it
/// took in. A report that arrives before the next frame exists waits for it instead of being lost.
///
/// It is also what releases the capture path's permit
/// ([`scwc_core::preview::preview_frame_consumed`]): until the ack arrives, the capture path
/// produces no frame at all — the Apple shim does not even render one — so a UI slower than the
/// camera costs the capture path nothing rather than costing it a converted frame per capture.
#[frb(sync)]
pub fn preview_frame_consumed() {
    scwc_core::preview::preview_frame_consumed();
    PREVIEW_READY.notify_one();
}

/// Sets the size the preview frames are scaled to before they reach Dart.
///
/// The UI sends the pixel size of its preview area — the logical size of the area times the device
/// pixel ratio — on every layout change, and `0x0` while it shows no preview. The capture path
/// scales every frame aspect-locked to fit inside that box, so no letterboxing ever reaches the
/// UI: it centres what it gets.
#[frb(sync)]
pub fn set_preview_target_size(width: u32, height: u32) {
    scwc_core::preview::set_preview_target_size(width, height);
}

/// Turns a frame of the capture path into the frame Dart receives; the pixels move, nothing copies.
fn into_preview_frame(frame: CorePreviewFrame) -> PreviewFrame {
    let CorePreviewFrame {
        rgba,
        width,
        height,
    } = frame;
    PreviewFrame {
        rgba,
        width,
        height,
    }
}

/// Hands preview frames to every live Dart stream, one at a time and only when the UI is ready for
/// the next.
///
/// This is what keeps the picture consumer-paced: every frame the capture path produces is taken
/// off its channel at once, so nothing waits there, but only the newest is kept and only it is
/// handed over on the next [`preview_frame_consumed`]. A UI slower than the camera therefore drops
/// frames instead of building a backlog, and the frame rate follows the capture parameters as soon
/// as it keeps up again.
///
/// The task ends when the capture path drops its sender, which is what ending a run does.
async fn forward_preview_frames(mut frames: UnboundedReceiver<CorePreviewFrame>) {
    let mut latest: Option<PreviewFrame> = None;
    loop {
        tokio::select! {
            // Taking everything that arrives is what keeps the capture path's channel empty; all
            // but the newest frame are dropped right here.
            incoming = frames.recv() => match incoming {
                Some(frame) => latest = Some(into_preview_frame(frame)),
                None => break,
            },
            // What goes out is the newest frame the UI has not seen yet.
            () = PREVIEW_READY.notified(), if latest.is_some() => {
                if let Some(frame) = latest.take() {
                    emit_preview(frame);
                }
            }
        }
    }
}

/// Hands `frame` to every live preview stream.
///
/// A sink whose Dart subscription is gone rejects the frame, and that is how dead subscribers are
/// found and dropped. The last sink gets the frame itself and the earlier ones a copy, so the
/// usual single viewer never copies the pixels.
fn emit_preview(frame: PreviewFrame) {
    let mut subscribers = PREVIEW_SUBSCRIBERS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if subscribers.is_empty() {
        return;
    }

    let last = subscribers.len() - 1;
    let mut shared = Some(frame);
    let mut live = Vec::with_capacity(subscribers.len());
    for (index, sink) in subscribers.drain(..).enumerate() {
        let delivered = if index == last {
            shared.take()
        } else {
            shared.clone()
        };
        let Some(frame) = delivered else { break };
        if sink.add(frame).is_ok() {
            live.push(sink);
        }
    }
    *subscribers = live;
}

/// The template's demo API, kept while the UI lane replaces the demo screen.
pub mod simple;

/// The one background agent of this process.
struct AgentSlot {
    /// Set while a run is live; sending `true` on it asks the run to stop.
    shutdown: Option<watch::Sender<bool>>,
    /// The commands of the run that is live, if any: a parameter switch or a reconnect.
    commands: Option<UnboundedSender<UiCommand>>,
    /// Monitors the most recent run task: the next start waits on it to know that the camera is
    /// free, and it reports a run that ended without an error of its own.
    task: Option<JoinHandle<()>>,
    /// Identifies the run a task belongs to, so a task cannot clear a newer agent's slot.
    generation: u64,
}

static AGENT: Mutex<AgentSlot> = Mutex::new(AgentSlot {
    shutdown: None,
    commands: None,
    task: None,
    generation: 0,
});

/// First delay before the agent tries the camera again after `open` failed.
const OPEN_BACKOFF_FIRST: Duration = Duration::from_secs(1);

/// Ceiling of the camera-open backoff; the same shape as the core's reconnect backoff.
const OPEN_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Runtime the agent runs on, shared by every run of this process.
static RUNTIME: LazyLock<Result<Runtime, String>> =
    LazyLock::new(|| Runtime::new().map_err(|error| error.to_string()));

/// Dart streams subscribed to [`agent_events`].
static SUBSCRIBERS: Mutex<Vec<StreamSink<AgentEvent>>> = Mutex::new(Vec::new());

/// Dart streams subscribed to [`preview_stream`].
static PREVIEW_SUBSCRIBERS: Mutex<Vec<StreamSink<PreviewFrame>>> = Mutex::new(Vec::new());

/// Whether the UI is ready for the next preview frame; see [`preview_frame_consumed`].
///
/// A permit is stored while nothing waits for one, so a consumption report that arrives before the
/// next frame exists waits for it rather than being lost.
static PREVIEW_READY: Notify = Notify::const_new();

/// Guards the one-time installation of the `tracing` subscriber.
static LOGGING: Once = Once::new();

/// Reconnect attempts reported since the last attach of the running agent.
static RECONNECT_ATTEMPTS: AtomicU32 = AtomicU32::new(0);

/// Hands `event` to every live subscriber.
///
/// A sink whose Dart subscription is gone rejects the event; that is how dead subscribers are
/// found and dropped.
fn emit(event: AgentEvent) {
    let mut subscribers = SUBSCRIBERS.lock().unwrap_or_else(|error| error.into_inner());
    subscribers.retain(|sink| sink.add(event.clone()).is_ok());
}

/// Turns the core's lifecycle events into [`AgentEvent`]s, until the core drops its sender.
///
/// A session ending is followed by [`AgentEvent::Reconnecting`] or by the process stopping, and
/// [`AgentEvent::Online`] already proves that a stream is live, so neither needs its own event.
async fn forward_core_events(mut events: UnboundedReceiver<CoreEvent>) {
    while let Some(event) = events.recv().await {
        match event {
            CoreEvent::Registering => emit(AgentEvent::Connecting),
            CoreEvent::Reconnecting { .. } => {
                let attempt = RECONNECT_ATTEMPTS.fetch_add(1, Ordering::Relaxed) + 1;
                emit(AgentEvent::Reconnecting { attempt });
            }
            CoreEvent::Warn { message } => emit(AgentEvent::Warn { message }),
            CoreEvent::ParametersApplied {
                resolution,
                fps,
                codec,
                bitrate_kbps,
                ..
            } => emit(AgentEvent::ParametersApplied {
                resolution,
                fps,
                codec,
                bitrate_kbps,
            }),
            CoreEvent::Notice { message } => emit(AgentEvent::Notice { message }),
            CoreEvent::Attached => {
                RECONNECT_ATTEMPTS.store(0, Ordering::Relaxed);
                // Attached is the proof the device is live on the server; the first-frame
                // signal in CountingSource only ever fires while a recording pulls frames,
                // which would leave a healthy standby reading as "Connecting" forever.
                emit(AgentEvent::Online);
            }
            CoreEvent::SessionStarted | CoreEvent::SessionEnded { .. } => {}
        }
    }
}

/// File the `tracing` subscriber appends to.
fn log_file_path() -> PathBuf {
    std::env::temp_dir().join("scwc-agent.log")
}

/// Directory the client keeps its configuration in, when the platform has one.
fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("sc-webcam"))
}

/// File the core remembers the capture parameters the pipeline was last switched to in.
///
/// `None` when the platform has no configuration directory: the agent then runs stateless, and
/// only the memory of a switch is lost.
fn state_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("capture-params.json"))
}

/// File the server identity the client registers with lives in.
fn server_config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("server.json"))
}

/// Writes the server identity, atomically: a temporary file next to it, then a rename over it, so
/// a reader never sees half a configuration.
fn write_server_config(config: &ServerConfig) -> Result<(), String> {
    let Some(path) = server_config_path() else {
        return Err("this platform has no configuration directory".to_owned());
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }

    let mut temp_name = path.as_os_str().to_owned();
    temp_name.push(".tmp");
    let temp = PathBuf::from(temp_name);

    let json = serde_json::to_vec(config).map_err(|error| error.to_string())?;
    fs::write(&temp, json).map_err(|error| error.to_string())?;
    fs::rename(&temp, &path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        error.to_string()
    })
}

/// Reads the stored server identity.
///
/// `None` covers both "nothing stored yet" and "the stored file is unusable": neither is worth
/// failing a launch over, and the UI simply starts with empty fields.
fn read_server_config() -> Option<ServerConfig> {
    let path = server_config_path()?;
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Installs the process-wide `tracing` subscriber on its first call.
///
/// The core, the capture pipeline and the encoding shims all log through `tracing`, which does
/// nothing at all without a subscriber. The log goes to a file — the app has no console — and
/// warnings and errors of the client crates are additionally pushed into the event stream, so a
/// failure inside capture reaches the UI instead of stopping at a log line nobody reads. The file
/// path is announced once through the stream for the same reason.
fn init_logging() {
    LOGGING.call_once(|| {
        let path = log_file_path();
        let writer = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => BoxMakeWriter::new(Arc::new(file)),
            // An unusable log file must not keep the agent from running; the events still reach
            // the UI.
            Err(error) => {
                eprintln!("scwc-agent: cannot write {}: {error}", path.display());
                BoxMakeWriter::new(std::io::sink)
            }
        };

        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false)
                    .with_filter(LevelFilter::INFO),
            )
            .with(WarnLayer {});
        if let Err(error) = subscriber.try_init() {
            eprintln!("scwc-agent: another tracing subscriber is installed: {error}");
        }

        emit(AgentEvent::Warn {
            message: format!("agent log file: {}", path.display()),
        });
    });
}

/// Forwards the client crates' warnings and errors into the event stream.
///
/// Without it a failure logged inside [`scwc_capture`] — an encoder giving up, a pipeline that
/// refuses to rebuild — would only ever reach the log file.
///
/// Braced rather than a unit struct: the binding generator parses this module and rejects unit
/// structs inside it.
struct WarnLayer {}

/// Crates whose warnings become [`AgentEvent::Warn`]s.
const WARN_TARGETS: [&str; 3] = ["scwc_core", "scwc_capture", "scwc_protocol"];

impl<S: Subscriber> Layer<S> for WarnLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        // tracing orders its levels by verbosity, not by severity — `DEBUG > INFO > WARN > ERROR`
        // — so "at least a warning" is `>=`, and the `<` this used to be let every info and
        // debug record through to the operator as a warning.
        if *metadata.level() > Level::WARN {
            return;
        }
        if !WARN_TARGETS
            .iter()
            .any(|target| metadata.target().starts_with(target))
        {
            return;
        }

        let mut visitor = MessageVisitor { message: None };
        event.record(&mut visitor);
        if let Some(message) = visitor.message {
            emit(AgentEvent::Warn { message });
        }
    }
}

/// Picks the `message` field out of a record and ignores every other field.
struct MessageVisitor {
    /// The record's `message` field, if it has one.
    message: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }
}

/// Reports the frame counter once a second until the run task aborts this task.
///
/// `dropped_frames` is always zero this milestone: the core drops frames in its own media queue
/// and exposes no counter for them.
async fn report_stats(frames: Arc<AtomicU64>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    // The first tick of an interval is immediate; only the following ones report.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        emit(AgentEvent::Stats {
            sent_frames: frames.load(Ordering::Relaxed),
            dropped_frames: 0,
        });
    }
}

/// The runtime the agent runs on, started on first use.
fn runtime() -> Result<&'static Runtime, String> {
    match &*RUNTIME {
        Ok(runtime) => Ok(runtime),
        Err(message) => Err(message.clone()),
    }
}

/// A [`FrameSource`] that counts the frames the core pulls from it.
///
/// The core asks for a frame only while a stream it was told to record is active, so the first
/// frame through here is the earliest proof that registration, attach and media all work.
struct CountingSource<S> {
    inner: S,
    frames: Arc<AtomicU64>,
}

impl<S: FrameSource> FrameSource for CountingSource<S> {
    async fn next_frame(&mut self) -> EncodedFrame {
        let frame = self.inner.next_frame().await;
        if self.frames.fetch_add(1, Ordering::Relaxed) == 0 {
            emit(AgentEvent::Online);
        }
        frame
    }

    fn set_parameters(
        &mut self,
        resolution: &str,
        fps: u32,
        codec: Codec,
        bitrate_kbps: u32,
    ) -> Result<(), scwc_core::SourceError> {
        self.inner
            .set_parameters(resolution, fps, codec, bitrate_kbps)
    }

    async fn take_photo(&mut self) -> Result<Vec<u8>, scwc_core::SourceError> {
        self.inner.take_photo().await
    }

    fn set_encoding(&mut self, enabled: bool) {
        self.inner.set_encoding(enabled);
    }

    fn preferred_codec(&self) -> Codec {
        self.inner.preferred_codec()
    }
}

/// Parses the protocol's codec vocabulary, which is closed and case-sensitive.
fn parse_codec(name: &str) -> Result<Codec, String> {
    match name {
        "h264" => Ok(Codec::H264),
        "h265" => Ok(Codec::H265),
        other => Err(format!(
            "unsupported codec {other:?}: this client encodes h264 or h265"
        )),
    }
}

/// The registration lists announced for the one camera this agent drives.
///
/// The requested pair comes first, followed by everything the camera itself announces, which is
/// the list the server validates a `switch_camera` against. A camera the host cannot enumerate
/// is announced with the requested pair alone.
fn capabilities(camera_id: &str, resolution: &str, fps: u32) -> (Vec<String>, Vec<u32>) {
    let mut resolutions = vec![resolution.to_owned()];
    let mut framerates = vec![fps];

    if let Ok(cameras) = enumerate_cameras() {
        if let Some(camera) = cameras.iter().find(|camera| camera.id == camera_id) {
            for (preset, rates) in &camera.presets {
                if !resolutions.iter().any(|entry| entry == preset) {
                    resolutions.push(preset.clone());
                }
                for rate in rates {
                    if !framerates.contains(rate) {
                        framerates.push(*rate);
                    }
                }
            }
        }
    }

    (resolutions, framerates)
}
