import 'dart:async';
import 'dart:ui' as ui;

import 'package:app/src/agent_state.dart';
import 'package:app/src/preview_texture.dart';
import 'package:app/src/rust/api.dart';
import 'package:flutter/foundation.dart';

/// The live camera picture: the newest frame of the bridge's preview stream, decoded and ready to
/// paint, or the platform's own texture where it has one.
///
/// The feed follows one [AgentState], and the state is the only thing that clears the picture: it
/// goes the moment the pipeline behind it is replaced or torn down, and a frame that lands after
/// that is not painted at all. A pause in the frames of a running pipeline is not a reason to clear
/// it — frames are paced to what the UI takes in, so a pause means the picture is waiting on the
/// pipeline rather than that there is nothing to show — and the picture waiting through a pause
/// reads as a live view where a blink to the placeholder reads as a fault. Only one frame is in
/// flight at a time: the feed reports each frame it took in back to the client
/// ([previewFrameConsumed]) — every one of them, including a frame it drops rather than paints,
/// since the capture path produces nothing while its permit is out — which keeps the picture at the
/// capture rate instead of letting a slower decode build up a backlog, and a frame that lands while
/// the previous one is still decoding replaces it rather than queueing behind it.
///
/// Where the platform paints the camera's own buffer ([PreviewTexture]), that texture is the source
/// instead, and the stream is not subscribed: the frame path is more work per frame, so it is the
/// fallback rather than the way the picture is shown there.
class PreviewFeed extends ChangeNotifier {
  PreviewFeed({this.onError});

  /// Called when the frame stream fails; the picture then keeps the last frame it decoded.
  final void Function(Object error)? onError;

  AgentState? _state;

  /// The pipeline the picture on screen belongs to; see [AgentState.previewEpoch].
  int _epoch = 0;

  ui.Image? _image;
  PreviewFrame? _pending;
  bool _decoding = false;
  bool _disposed = false;
  StreamSubscription<PreviewFrame>? _frames;

  /// The platform texture the picture comes from, and the run it belongs to, while there is one.
  int? _textureId;
  int _textureEpoch = -1;
  bool _sourcing = false;
  bool _sourceAgain = false;

  /// The shape of the picture the pipeline produces, measured from the frame the platform paints.
  ///
  /// The texture paints a frame of the camera's shape — the camera's own buffer on the Apple
  /// platforms, the scaled frame the capture path published on Linux and Windows — and Flutter scales
  /// it to whatever box it is given, so its aspect ratio is the one thing the interface must know
  /// about a picture that never arrives here as a decoded image. A camera may negotiate a size other
  /// than the one it was asked for, which is why the platform's own answer is preferred over the
  /// configured resolution.
  double? _sourceRatio;

  /// Clockwise quarter turns the platform's picture needs to stand up, or 0 while it needs none.
  ///
  /// A camera whose sensor is mounted sideways writes a picture that lies on its side, and the
  /// orientation is a property of the camera, not of a frame — so the platform is asked for it
  /// once, alongside the shape above, and the interface turns the whole texture by it.
  int _quarterTurns = 0;

  /// The newest frame of the pipeline the feed follows, or null while there is none to show.
  ui.Image? get image => _image;

  /// The platform's own texture the picture is painted from, or null while the frame stream is what
  /// the picture comes from.
  int? get textureId => _textureId;

  /// Clockwise quarter turns to turn that texture's picture by, so it stands up.
  int get textureQuarterTurns => _quarterTurns;

  /// Width over height of that texture's picture, or null while its shape is not known at all.
  ///
  /// The frames the pipeline produced are the truth about it; the resolution the capture path was
  /// asked for is the guess to paint with until one of them has been seen.
  double? get textureAspectRatio => _sourceRatio ?? _state?.previewAspectRatio;

  /// Subscribes to [state], which decides which frames may be shown; calling it again is a no-op.
  void start(AgentState state) {
    if (_state != null) return;
    _state = state;
    state.addListener(_onStateChanged);
    _onStateChanged();
  }

  @override
  void dispose() {
    _disposed = true;
    _state?.removeListener(_onStateChanged);
    _state = null;
    unawaited(_frames?.cancel());
    _frames = null;
    _pending = null;
    _dropImage();
    // The platform's frame is the plugin's to release, and it stops publishing with the texture.
    unawaited(PreviewTexture.stop());
    super.dispose();
  }

  /// True while frames may be shown: only the running pipeline is allowed to fill the preview.
  bool get _accepting {
    final state = _state;
    return !_disposed &&
        state != null &&
        state.running &&
        state.previewEpoch == _epoch;
  }

  /// Follows the agent: the picture goes as soon as the pipeline behind it is replaced or torn
  /// down — the one thing that clears it.
  ///
  /// Nothing else does. A picture whose frames pause stays up: frames are paced to what the UI takes
  /// in, so a pause means the picture is waiting on the pipeline rather than that there is nothing
  /// to show, and one that blinks away and back on every pause reads as a broken preview where one
  /// that waits through it reads as a live view. A frame that arrives after a pause replaces the
  /// held one, so the picture resumes where the camera is.
  void _onStateChanged() {
    final state = _state;
    if (state == null) return;

    if (!state.running || state.previewEpoch != _epoch) {
      _epoch = state.previewEpoch;
      _drop();
    }
    _pointSource();
  }

  /// Points the picture at the source this run has: the platform's own texture where there is one,
  /// the paced frame stream everywhere else.
  ///
  /// Only one of them consumes frames, so the stream stops while the texture is the source — the
  /// picture is not cheaper if converted frames keep coming for nobody. The platform calls are
  /// asynchronous and the agent can move on inside them, so one pass runs at a time and a change
  /// that arrived meanwhile runs once more, against the state as it is then.
  void _pointSource() {
    if (_sourcing) {
      _sourceAgain = true;
      return;
    }
    _sourcing = true;
    unawaited(_pointSourcePasses());
  }

  Future<void> _pointSourcePasses() async {
    try {
      do {
        _sourceAgain = false;
        await _pointSourceOnce();
      } while (_sourceAgain && !_disposed);
    } finally {
      _sourcing = false;
    }
  }

  Future<void> _pointSourceOnce() async {
    final state = _state;
    if (state == null || _disposed) return;

    final wantsTexture = state.running && PreviewTexture.available;
    _followStream(!wantsTexture);
    if (!wantsTexture) {
      if (_textureId == null) return;
      _forgetTexture();
      await PreviewTexture.stop();
      return;
    }

    final epoch = state.previewEpoch;
    if (_textureId != null && _textureEpoch == epoch) return;
    if (_textureId != null) {
      // The texture of the run that ended: its frame must not stand in for this run's picture.
      _forgetTexture();
      await PreviewTexture.stop();
    }

    final id = await PreviewTexture.start();
    final now = _state;
    if (_disposed ||
        now == null ||
        !now.running ||
        now.previewEpoch != epoch ||
        !PreviewTexture.available) {
      // The run moved on inside the call: this texture is not the one to paint, and a later pass
      // takes the picture from wherever the agent is by then.
      if (id != null) await PreviewTexture.stop();
      return;
    }
    if (id == null) {
      // No native path here after all, so the stream is the source as before; the next state change
      // asks again.
      _followStream(true);
      return;
    }
    _textureId = id;
    _textureEpoch = epoch;
    notifyListeners();
    unawaited(_measureSource(id));
  }

  /// Asks the platform for the shape of the picture it paints — and for the turn it needs to stand
  /// up — which is what the texture is letterboxed and turned by.
  ///
  /// The buffer's own size is the truth about it, and it is only there once a frame has been
  /// captured, so the question is asked again a few times; until it answers, the resolution the
  /// capture path was asked for paints the picture. The turn is a property of the camera and is
  /// answered whether a frame has been captured or not, so it is taken from the first pass that
  /// gets one.
  Future<void> _measureSource(int textureId) async {
    for (var attempt = 0; attempt < 20; attempt++) {
      final size = await PreviewTexture.size();
      final turns = await PreviewTexture.quarterTurns();
      if (_disposed || _textureId != textureId) return;
      if (turns != null && turns != _quarterTurns) {
        _quarterTurns = turns;
        notifyListeners();
      }
      if (size != null) {
        _sourceRatio = size.$1 / size.$2;
        notifyListeners();
        return;
      }
      await Future<void>.delayed(const Duration(milliseconds: 200));
    }
  }

  /// Clears the picture the texture was painted from; the texture itself is the caller's to give
  /// back, since the platform call is asynchronous and the picture must not wait for it.
  void _forgetTexture() {
    _textureId = null;
    _textureEpoch = -1;
    _quarterTurns = 0;
    notifyListeners();
  }

  /// Starts or stops the frame stream; it is the source only while no texture is.
  void _followStream(bool wanted) {
    if (wanted == (_frames != null)) return;
    if (!wanted) {
      final frames = _frames;
      _frames = null;
      unawaited(frames?.cancel());
      return;
    }
    if (_disposed) return;
    _frames = previewStream().listen(_onFrame, onError: _onStreamError);
  }

  /// Tells the client the frame it handed over is done with, painted or not.
  ///
  /// The client holds the next frame until it hears back about the one it gave out — here, in the
  /// feed's own pacing, and again in the capture path, which produces nothing while its permit is
  /// out — so every frame that arrives is reported exactly once, including the ones a newer frame,
  /// a cleared preview or a replaced pipeline discards. Leaving a discarded frame unreported would
  /// strand that permit and leave the preview stuck on the frame after it.
  void _consumed() => previewFrameConsumed();

  void _onFrame(PreviewFrame frame) {
    // A frame of a pipeline the feed no longer follows — one that was already on its way when the
    // run stopped or the sink was cleared — is not painted, but it is still reported: a pipeline
    // that replaced this one needs the permit this frame holds.
    if (!_accepting) {
      _consumed();
      return;
    }
    // A frame still waiting to be decoded is superseded by this one: it never reaches a decode, so
    // it is reported here instead.
    if (_pending != null) _consumed();
    _pending = frame;
    if (!_decoding) _decodeNext();
  }

  void _onStreamError(Object error) {
    // The stream carries frames rather than state, so a failure of its own leaves the picture as it
    // is — it is the agent state, not a silent stream, that clears it; the report belongs in the
    // event log.
    onError?.call(error);
  }

  /// Decodes the newest frame that arrived, unless one is already decoding.
  void _decodeNext() {
    final frame = _pending;
    if (frame == null) {
      _decoding = false;
      return;
    }
    _pending = null;
    _decoding = true;
    // The client converts and scales on the Rust side, so these pixels are painted as they are.
    ui.decodeImageFromPixels(
      frame.rgba,
      frame.width,
      frame.height,
      ui.PixelFormat.rgba8888,
      _onDecoded,
      rowBytes: frame.width * 4,
    );
  }

  void _onDecoded(ui.Image image) {
    _decoding = false;
    if (!_accepting) {
      // The pipeline this frame belongs to is gone; it must not be painted over the one that
      // replaced it, but it is still reported so the pipeline that replaced it is not left without
      // a permit.
      _pending = null;
      image.dispose();
      _consumed();
      return;
    }
    final previous = _image;
    _image = image;
    // Whatever painted the previous frame holds a handle of its own on it, so this one goes as soon
    // as it is replaced rather than piling up at the capture rate.
    previous?.dispose();
    notifyListeners();
    // The picture is consumer-paced: this is what asks the client for the next frame, so the camera
    // never runs ahead of what is taken in here and no backlog can build up behind a busy decode.
    _consumed();
    // A frame that landed while this one decoded is newer than what is painted now.
    if (_pending != null) _decodeNext();
  }

  /// Takes the picture off screen: the handle goes, the frames queued behind it go, and the picture
  /// is forgotten until a pipeline is followed again.
  void _drop() {
    // A frame that was waiting to be decoded goes with the picture; it is reported like any other,
    // so clearing the preview cannot cost the next pipeline its first frame.
    if (_pending != null) {
      _pending = null;
      _consumed();
    }
    if (_image == null) return;
    _dropImage();
    notifyListeners();
  }

  void _dropImage() {
    final image = _image;
    _image = null;
    image?.dispose();
  }
}
