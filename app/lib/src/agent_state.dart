import 'dart:async';
import 'dart:collection';

import 'package:app/src/rust/api.dart';
import 'package:flutter/widgets.dart';
import 'package:shared_preferences/shared_preferences.dart';

/// One timestamped line of the event log.
class LogEntry {
  const LogEntry(this.time, this.message);

  final DateTime time;
  final String message;
}

/// One capture-parameter set: what the pickers stage and what the agent runs.
///
/// The bitrate is optional because the protocol has no field for it: an empty field means "let
/// the client derive one", which the core does from the other three values.
class CaptureParams {
  const CaptureParams({
    required this.resolution,
    required this.fps,
    required this.codec,
    required this.bitrateKbps,
  });

  /// Protocol resolution, for example `1920x1080`.
  final String resolution;

  /// Frame rate in frames per second.
  final int fps;

  /// `h264` or `h265`.
  final String codec;

  /// Encoder bitrate in kilobits per second; null means the client derives one.
  final int? bitrateKbps;

  @override
  bool operator ==(Object other) =>
      other is CaptureParams &&
      other.resolution == resolution &&
      other.fps == fps &&
      other.codec == codec &&
      other.bitrateKbps == bitrateKbps;

  @override
  int get hashCode => Object.hash(resolution, fps, codec, bitrateKbps);

  @override
  String toString() =>
      '$resolution @ ${fps}fps $codec ${bitrateKbps ?? 'auto'}kbps';
}

/// Resolutions of [camera], largest first, as announced by the platform.
List<String> _resolutionsOf(CameraInfoDto camera) => [
  for (final preset in camera.presets) preset.$1,
];

/// Frame rates the platform announces for one `(camera, resolution)` pair.
List<int> _frameRatesOf(CameraInfoDto camera, String? resolution) {
  if (resolution == null) return const [];
  for (final preset in camera.presets) {
    if (preset.$1 == resolution) return preset.$2.toList();
  }
  return const [];
}

/// The two sides of a `WIDTHxHEIGHT` resolution, or null when it cannot be read.
(int, int)? _parseResolution(String? resolution) {
  final parts = resolution?.split('x');
  if (parts == null || parts.length != 2) return null;
  final width = int.tryParse(parts[0]);
  final height = int.tryParse(parts[1]);
  if (width == null || height == null) return null;
  return (width, height);
}

/// How much a resolution's aspect ratio is preferred: 4:3 first, then 16:9, then anything else.
///
/// The ratio is read with the longer side first, so the portrait preset of a 16:9 sensor still
/// counts as 16:9.
int _aspectRank(int width, int height) {
  const tolerance = 0.01;
  final ratio = width >= height ? width / height : height / width;
  if ((ratio - 4 / 3).abs() <= tolerance) return 0;
  if ((ratio - 16 / 9).abs() <= tolerance) return 1;
  return 2;
}

/// The preset a camera starts on: [1] the best-ranked aspect ratio, then its largest resolution,
/// as `(resolution, frame rate)`. The frame rate is the highest that resolution announces, and
/// the result is null when the camera announces nothing usable.
(String, int)? _defaultPreset(CameraInfoDto camera) {
  (String, int)? best;
  var bestRank = 0;
  var bestPixels = 0;

  for (final preset in camera.presets) {
    final (resolution, rates) = preset;
    final size = _parseResolution(resolution);
    if (size == null || rates.isEmpty) continue;

    final (width, height) = size;
    final rank = _aspectRank(width, height);
    final pixels = width * height;
    // Rank first, then the largest resolution within the class. Presets arrive largest first, but
    // that is the platform's own order, not something to rely on.
    final better =
        best == null || rank < bestRank || (rank == bestRank && pixels > bestPixels);
    if (!better) continue;

    best = (resolution, rates.reduce((a, b) => a > b ? a : b));
    bestRank = rank;
    bestPixels = pixels;
  }

  return best;
}

/// Everything the UI does with the agent: the connection fields, the staged capture selection, the
/// run's status and the event log.
class AgentState extends ChangeNotifier {
  static const List<String> codecs = ['h264', 'h265'];

  /// Bits per pixel the encoders are budgeted, mirrored from the core
  /// (`crates/core/src/media.rs`): 3840x2160 at 60 fps lands at about 53 Mbps for H.264 and
  /// 30 Mbps for every other codec.
  static const double _h264BitsPerPixel = 0.107;
  static const double _hevcBitsPerPixel = 0.060;

  static const int _maxLogEntries = 100;

  static const String _kCodec = 'codec';
  static const String _kBitrate = 'bitrate';
  static const String _kCameraId = 'camera_id';
  static const String _kResolution = 'resolution';
  static const String _kFps = 'fps';

  final TextEditingController serverController = TextEditingController();
  final TextEditingController deviceIdController = TextEditingController();
  final TextEditingController tokenController = TextEditingController();
  final TextEditingController bitrateController = TextEditingController();

  final List<LogEntry> _log = [];

  String _codec = codecs.first;
  List<CameraInfoDto> _cameras = const [];
  CameraInfoDto? _camera;
  String? _resolution;
  int? _fps;
  AgentEvent? _status;
  BigInt? _sentFrames;
  BigInt? _droppedFrames;
  bool _running = false;
  bool _busy = false;
  bool _disposed = false;
  bool _restoring = false;

  /// The parameters the running agent uses, as the core last reported them; null while no agent
  /// runs. The core has the last word — a switch from an earlier run, or one the server issued,
  /// wins over what the pickers ask for.
  CaptureParams? _liveParams;

  /// The camera the running agent captures from.
  String? _liveCameraId;

  /// Bumped whenever the capture pipeline behind the preview is started, restarted or torn down.
  ///
  /// A picture belongs to the pipeline that produced it, so the preview drops the one it shows as
  /// soon as this changes: no frame of an earlier run survives into a later one.
  int _previewEpoch = 0;

  SharedPreferences? _prefs;
  StreamSubscription<AgentEvent>? _events;
  String? _savedCameraId;
  String? _savedResolution;
  int? _savedFps;

  AgentState() {
    for (final controller in [
      serverController,
      deviceIdController,
      tokenController,
      bitrateController,
    ]) {
      controller.addListener(_onFieldChanged);
    }
  }

  /// Restores the persisted selection, loads the stored server identity, subscribes to the
  /// agent's events, lists the cameras, and connects on its own when the identity is there.
  Future<void> load() async {
    _prefs = await SharedPreferences.getInstance();
    _restore();
    final hasServerConfig = _restoreServerConfig();
    _notify();
    _events = agentEvents().listen(_onEvent, onError: _onStreamError);
    refreshCameras();
    await _autoStart(hasServerConfig);
  }

  @override
  void dispose() {
    _disposed = true;
    final events = _events;
    _events = null;
    if (events != null) {
      unawaited(events.cancel());
    }
    serverController.dispose();
    deviceIdController.dispose();
    tokenController.dispose();
    bitrateController.dispose();
    super.dispose();
  }

  /// Cameras of the host, empty until [refreshCameras] ran.
  List<CameraInfoDto> get cameras => _cameras;

  CameraInfoDto? get camera => _camera;

  String? get resolution => _resolution;

  int? get fps => _fps;

  String get codec => _codec;

  /// Parsed bitrate, null while the field is empty (the client derives one) or not a number.
  int? get bitrate {
    final value = int.tryParse(bitrateController.text.trim());
    return value != null && value > 0 ? value : null;
  }

  /// True while the field holds something that is not a positive number.
  ///
  /// An empty field is not an error: it asks the client to derive the bitrate from the codec, the
  /// resolution and the frame rate.
  bool get bitrateInvalid =>
      bitrateController.text.trim().isNotEmpty && bitrate == null;

  /// The capture parameters the pickers stage; null until a resolution and frame rate are picked.
  CaptureParams? get stagedParams {
    final resolution = _resolution;
    final fps = _fps;
    if (resolution == null || fps == null) return null;
    return CaptureParams(
      resolution: resolution,
      fps: fps,
      codec: _codec,
      bitrateKbps: bitrate,
    );
  }

  /// The parameters the running agent uses, if any.
  CaptureParams? get liveParams => _liveParams;

  /// Identifies the capture pipeline the preview may show: it changes whenever the agent starts,
  /// restarts or stops, so a picture can be told apart from one of an earlier pipeline.
  int get previewEpoch => _previewEpoch;

  /// Width over height of the picture the camera produces, or null while no size is known.
  ///
  /// The stream path's frames carry their own size, so only the native texture needs to be told the
  /// shape to letterbox its picture in; this is the resolution the capture path asks the camera
  /// for.
  double? get previewAspectRatio {
    final size = _parseResolution(_resolution);
    if (size == null || size.$2 == 0) return null;
    return size.$1 / size.$2;
  }

  /// True while the staged selection differs from what the running agent uses.
  bool get stagedDiffers {
    final staged = stagedParams;
    if (staged == null) return false;
    final camera = _camera?.id;
    return staged != _liveParams || (camera != null && camera != _liveCameraId);
  }

  /// True when [apply] has something to send.
  bool get canApply => _running && !_busy && !bitrateInvalid && stagedDiffers;

  /// The bitrate the client derives for the staged selection, or null while there is nothing to
  /// derive it from.
  ///
  /// Mirrors the core's formula (`crates/core/src/media.rs`): `width * height * fps` pixels per
  /// second at a codec-specific budget of bits per pixel, rounded up.
  int? get computedBitrateKbps {
    final size = _parseResolution(_resolution);
    final fps = _fps;
    if (size == null || fps == null) return null;

    final bitsPerPixel = _codec == 'h264'
        ? _h264BitsPerPixel
        : _hevcBitsPerPixel;
    final kbps = (size.$1 * size.$2 * fps * bitsPerPixel / 1000).ceil();
    return kbps < 1 ? 1 : kbps;
  }

  /// What the bitrate field shows while it is empty.
  String get bitrateHint {
    final kbps = computedBitrateKbps;
    return kbps == null ? 'auto' : 'auto: $kbps';
  }

  /// Resolutions offered by [camera].
  List<String> get resolutions =>
      _camera == null ? const [] : _resolutionsOf(_camera!);

  /// Frame rates offered by the selected `(camera, resolution)` pair.
  List<int> get frameRates =>
      _camera == null ? const [] : _frameRatesOf(_camera!, _resolution);

  bool get running => _running;

  /// True while [_connect], [stop] or [apply] is in flight.
  bool get busy => _busy;

  /// Newest first, at most [_maxLogEntries] lines.
  List<LogEntry> get log => UnmodifiableListView(_log);

  /// The last state-changing event; stats only feed the counters and are not kept here.
  AgentEvent? get lastStatus => _status;

  bool get hasError => _status is AgentEvent_Error;

  /// True while the server's stream is being recorded, which also means the agent runs.
  bool get isRecording => _running && _status is AgentEvent_Recording;

  /// Human-readable form of [lastStatus], as the truth about the agent now.
  ///
  /// A run that ended leaves its last event behind — the online or recording state it was in — and
  /// that must not read as if the client were still live; only a failure is worth keeping once
  /// nothing runs, because it is what the operator has to act on.
  String get statusText {
    if (!_running && _status is! AgentEvent_Error) return 'Idle';
    return switch (_status) {
      null => 'Idle',
      AgentEvent_Connecting() => 'Connecting',
      AgentEvent_Online() => 'Online',
      AgentEvent_Recording(:final streamId) => 'Recording ($streamId)',
      AgentEvent_RecordingStopped() => 'Online, stream stopped',
      AgentEvent_Reconnecting(:final attempt) =>
        'Reconnecting (attempt $attempt)',
      AgentEvent_Warn(:final message) => 'Warning: $message',
      AgentEvent_ParametersApplied() => 'Online, parameters applied',
      AgentEvent_Notice() => 'Online',
      AgentEvent_Error(:final message) => 'Error: $message',
      AgentEvent_Stats() => 'Idle',
    };
  }

  String get statsText => _sentFrames == null
      ? 'No stats yet'
      : 'Sent $_sentFrames frames, dropped ${_droppedFrames ?? BigInt.zero}';

  /// The applied parameters, and the staged ones while they still differ, for the status card.
  String? get appliedText {
    final live = _liveParams;
    if (live == null) return null;

    final staged = stagedParams;
    if (canApply && staged != null) {
      return 'Capture: $live  |  apply: $staged';
    }
    return 'Capture: $live';
  }

  /// True when [_connect] has everything it needs: an identity, a camera and a full selection.
  ///
  /// Not a user action of its own — there is no start control: the two ways in are the launch,
  /// when a stored identity exists, and [saveAndReconnect], which the operator uses for a
  /// first-time or changed configuration.
  bool get _canConnect =>
      !_busy &&
      !_running &&
      serverController.text.trim().isNotEmpty &&
      deviceIdController.text.trim().isNotEmpty &&
      tokenController.text.isNotEmpty &&
      _camera != null &&
      _resolution != null &&
      _fps != null &&
      !bitrateInvalid;

  /// Lists cameras and re-applies the persisted selection.
  void refreshCameras() {
    final List<CameraInfoDto> cameras;
    try {
      cameras = listCameras();
    } catch (error) {
      _addLog('listCameras failed: $error');
      _notify();
      return;
    }
    // Re-apply the current pick, falling back to the persisted one on the first refresh.
    final wantedId = _camera?.id ?? _savedCameraId;
    final wantedResolution = _resolution ?? _savedResolution;
    final wantedFps = _fps ?? _savedFps;
    _cameras = cameras;
    _addLog('Found ${cameras.length} camera(s)');
    CameraInfoDto? wanted;
    for (final camera in cameras) {
      if (camera.id == wantedId) {
        wanted = camera;
        break;
      }
    }
    _applySelection(
      wanted ?? (cameras.isEmpty ? null : cameras.first),
      preferResolution: wantedResolution,
      preferFps: wantedFps,
    );
    _savedCameraId = null;
    _savedResolution = null;
    _savedFps = null;
    _persistSelection();
    _notify();
  }

  void setCodec(String codec) {
    _codec = codec;
    _persist();
    _notify();
  }

  void setCamera(String cameraId) {
    for (final camera in _cameras) {
      if (camera.id == cameraId) {
        _applySelection(camera);
        _persist();
        _notify();
        return;
      }
    }
  }

  void setResolution(String resolution) {
    _applySelection(_camera, preferResolution: resolution);
    _persist();
    _notify();
  }

  void setFps(int fps) {
    if (!frameRates.contains(fps)) return;
    _fps = fps;
    _persist();
    _notify();
  }

  /// Records `message` in the event log, for the reports that come from outside the agent stream.
  void addLog(String message) {
    _addLog(message);
    _notify();
  }

  /// Starts the agent with the staged selection.
  ///
  /// Internal on purpose, and reached two ways only: [load] connects the launch when a stored
  /// identity exists, and [saveAndReconnect] connects after the operator configured the client.
  /// A request that cannot be honoured is reported rather than silently ignored, because both
  /// callers looked like they asked for a connection.
  Future<void> _connect() async {
    final staged = stagedParams;
    final camera = _camera;
    final size = _parseResolution(staged?.resolution);
    if (!_canConnect || staged == null || camera == null || size == null) {
      _addLog(
        _busy
            ? 'not connecting now: another agent operation is in flight'
            : 'nothing to connect with yet: pick a camera and its resolution',
      );
      _notify();
      return;
    }

    _busy = true;
    // A run that is about to start is a new pipeline: what the preview shows belongs to an earlier
    // one, or to none, and goes now.
    _previewEpoch++;
    _notify();
    try {
      await startAgent(
        config: AgentConfig(
          server: serverController.text.trim(),
          deviceId: deviceIdController.text.trim(),
          token: tokenController.text.trim(),
          cameraId: camera.id,
          width: size.$1,
          height: size.$2,
          fps: staged.fps,
          codec: staged.codec,
          bitrateKbps: staged.bitrateKbps,
        ),
      );
      _running = true;
      // Optimistic until the core reports the set it actually applied: a switch remembered from
      // an earlier run takes precedence over the announced defaults.
      _liveParams = staged;
      _liveCameraId = camera.id;
      _addLog('agent started: ${camera.name} $staged');
    } catch (error) {
      _addLog('start failed: $error');
    } finally {
      _busy = false;
      _notify();
    }
  }

  /// Asks the agent to wind down.
  ///
  /// The agent cancels everything it is doing, including the retries that would otherwise bring it
  /// back.
  Future<void> stop() async {
    // The pipeline is going away, so the picture it produced goes with it.
    _previewEpoch++;
    _busy = true;
    _notify();
    try {
      await stopAgent();
      _running = false;
      _liveParams = null;
      _liveCameraId = null;
      _addLog('agent stopped');
    } catch (error) {
      _addLog('stop failed: $error');
    } finally {
      _busy = false;
      _notify();
    }
  }

  /// Sends the staged selection to the running agent.
  ///
  /// Resolution, frame rate, codec and bitrate travel the same path a server-issued
  /// `switch_camera` takes: applied at once while no stream is live, and once the live one ends
  /// otherwise, so a recording keeps the parameters it started with. A camera change cannot travel
  /// that path — the agent holds one open device — so it restarts the agent with the staged set.
  Future<void> apply() async {
    final staged = stagedParams;
    if (!canApply || staged == null) return;

    if (_camera != null && _camera!.id != _liveCameraId) {
      await _restart();
      return;
    }

    _busy = true;
    _notify();
    try {
      applyCaptureParams(
        resolution: staged.resolution,
        fps: staged.fps,
        codec: staged.codec,
        bitrateKbps: staged.bitrateKbps,
      );
      _addLog('apply requested: $staged');
    } catch (error) {
      _addLog('apply failed: $error');
    } finally {
      _busy = false;
      _notify();
    }
  }

  /// Restarts the agent so it captures from the staged camera with the staged parameters.
  Future<void> _restart() async {
    _addLog('camera changed; restarting the agent');
    await stop();
    await _connect();
  }

  /// Stores the server identity and connects with it.
  ///
  /// The one way the client is configured: the identity is always written to
  /// `<config dir>/sc-webcam/server.json`, and then used — starting the agent when none runs, and
  /// asking the running one to register again at its first opportunity otherwise. The core
  /// decides when that is: right away while no stream is live, and once the live one ends
  /// otherwise, so a recording is never cut short by the change.
  Future<void> saveAndReconnect() async {
    // The client is about to register with a different identity, so an earlier picture must not
    // stand in for what the new configuration produces.
    _previewEpoch++;
    try {
      saveServerConfig(
        server: serverController.text.trim(),
        deviceId: deviceIdController.text.trim(),
        token: tokenController.text.trim(),
      );
      _addLog(
        _running
            ? 'server configuration saved; the agent reconnects when it can'
            : 'server configuration saved; connecting',
      );
    } catch (error) {
      // Storing the identity must not keep the client from connecting: the operator asked for a
      // connection, and the log says what became of the file.
      _addLog('could not store the server configuration: $error');
    }
    _notify();

    if (!_running) await _connect();
  }

  void _onEvent(AgentEvent event) {
    switch (event) {
      case AgentEvent_Stats(:final sentFrames, :final droppedFrames):
        _sentFrames = sentFrames;
        _droppedFrames = droppedFrames;
      case AgentEvent_Connecting():
        _running = true;
        _status = event;
        _addLog('connecting');
      case AgentEvent_Online():
        _running = true;
        _status = event;
        _addLog('online');
      case AgentEvent_Recording(:final streamId):
        _running = true;
        _status = event;
        _addLog('recording $streamId');
      case AgentEvent_RecordingStopped():
        _running = true;
        _status = event;
        _addLog('recording stopped');
      case AgentEvent_Reconnecting(:final attempt):
        _running = true;
        _status = event;
        _addLog('reconnecting (attempt $attempt)');
      case AgentEvent_ParametersApplied(
        :final resolution,
        :final fps,
        :final codec,
        :final bitrateKbps,
      ):
        // The core has the last word on what the camera runs, so the staged selection is measured
        // against this rather than against what it was asked for.
        //
        // A new resolution rebuilds the capture pipeline — a capture session's outputs are fixed
        // when it is built — so the picture the preview is showing belongs to the one that is
        // going away: a texture the camera rendered into at the old geometry, or the frames a
        // reader of it produced. The picture therefore has to be taken again, from the pipeline
        // this switch brought up, which is what the epoch is for.
        if (_liveParams?.resolution != resolution) {
          _previewEpoch++;
        }
        _liveParams = CaptureParams(
          resolution: resolution,
          fps: fps,
          codec: codec,
          bitrateKbps: bitrateKbps,
        );
        _addLog('parameters applied: $_liveParams');
      case AgentEvent_Notice(:final message):
        // A request the core had to postpone, a switch or a reconnect that waits for a stream. It
        // says nothing about the run itself, so the status stays where it was.
        _addLog(message);
      case AgentEvent_Warn(:final message):
        // A warning says nothing about the run itself, so it only lands in the log; the status
        // stays with whichever event described the agent's state.
        _addLog('warning: $message');
      case AgentEvent_Error(:final message):
        _running = false;
        _status = event;
        _addLog('error: $message');
    }
    _notify();
  }

  void _onStreamError(Object error) {
    _addLog('event stream error: $error');
    _notify();
  }

  /// Picks the camera plus a resolution/frame rate pair, preferring the persisted (or previously
  /// chosen) values when the camera still offers them and the spec's default otherwise: the
  /// preferred aspect ratio, its largest resolution, and the highest rate that resolution
  /// announces.
  void _applySelection(
    CameraInfoDto? camera, {
    String? preferResolution,
    int? preferFps,
  }) {
    _camera = camera;
    if (camera == null) {
      _resolution = null;
      _fps = null;
      return;
    }

    final preferred = _defaultPreset(camera);
    final resolutions = _resolutionsOf(camera);
    _resolution = resolutions.contains(preferResolution)
        ? preferResolution
        : (preferred?.$1 ?? (resolutions.isEmpty ? null : resolutions.first));

    final rates = _frameRatesOf(camera, _resolution);
    final preferredFps = _resolution == preferred?.$1 ? preferred?.$2 : null;
    _fps = rates.contains(preferFps)
        ? preferFps
        : (preferredFps ?? (rates.isEmpty ? null : rates.reduce((a, b) => a > b ? a : b)));
  }

  void _onFieldChanged() {
    // The controllers are written during _restore; persisting half-restored fields would wipe
    // the stored ones.
    if (!_restoring) _persist();
    _notify();
  }

  void _restore() {
    final prefs = _prefs!;
    _restoring = true;
    try {
      // The server identity is not restored from here: the client stores it itself, which is what
      // lets a launch connect before the operator touches anything (see loadServerConfig).
      final codec = prefs.getString(_kCodec);
      if (codec != null && codecs.contains(codec)) {
        _codec = codec;
      }
      bitrateController.text = prefs.getString(_kBitrate) ?? '';
      _savedCameraId = prefs.getString(_kCameraId);
      _savedResolution = prefs.getString(_kResolution);
      _savedFps = prefs.getInt(_kFps);
    } finally {
      _restoring = false;
    }
  }

  /// Fills the server fields from the identity the client has stored; `false` when there is none.
  bool _restoreServerConfig() {
    final config = _storedServerConfig();
    if (config == null) return false;

    _restoring = true;
    try {
      serverController.text = config.server;
      deviceIdController.text = config.deviceId;
      tokenController.text = config.token;
    } finally {
      _restoring = false;
    }
    return true;
  }

  /// The identity the client stored, or null when it stored none yet or it cannot be read.
  ServerConfig? _storedServerConfig() {
    try {
      return loadServerConfig();
    } catch (error) {
      _addLog('loadServerConfig failed: $error');
      return null;
    }
  }

  /// Connects without a click when a stored server identity and a camera are both there.
  Future<void> _autoStart(bool hasServerConfig) async {
    if (!hasServerConfig || _camera == null) return;

    _addLog('saved server configuration found; connecting');
    await _connect();
  }

  void _persist() {
    final prefs = _prefs;
    if (prefs == null) return;
    unawaited(prefs.setString(_kCodec, _codec));
    // Stored as it stands, so an empty field — the client derives the bitrate — survives a
    // restart as an empty field.
    unawaited(prefs.setString(_kBitrate, bitrateController.text.trim()));
    _persistSelection();
  }

  void _persistSelection() {
    final prefs = _prefs;
    final camera = _camera;
    // A host without cameras (or before the first refresh) must not drop the stored selection.
    if (prefs == null || camera == null) return;
    unawaited(prefs.setString(_kCameraId, camera.id));
    final resolution = _resolution;
    if (resolution == null) {
      unawaited(prefs.remove(_kResolution));
    } else {
      unawaited(prefs.setString(_kResolution, resolution));
    }
    final fps = _fps;
    if (fps == null) {
      unawaited(prefs.remove(_kFps));
    } else {
      unawaited(prefs.setInt(_kFps, fps));
    }
  }

  void _addLog(String message) {
    _log.insert(0, LogEntry(DateTime.now(), message));
    if (_log.length > _maxLogEntries) {
      _log.removeRange(_maxLogEntries, _log.length);
    }
  }

  void _notify() {
    if (!_disposed) notifyListeners();
  }
}
