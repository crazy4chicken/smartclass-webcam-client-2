import 'dart:io';

import 'package:flutter/services.dart';

/// The camera's own frames, painted by the platform's renderer instead of by Dart.
///
/// On the Apple platforms the capture pipeline publishes the buffer AVFoundation produced and the
/// plugin in `rust_builder/{macos,ios}` hands that same buffer to a Flutter texture, so a preview
/// costs neither a conversion nor a copy of the picture. On Linux and Windows the pipeline publishes
/// the frame it already converted and scaled to the size the interface asked for, and the plugin in
/// `rust_builder/{linux,windows}` hands that to a texture: the picture still costs one conversion,
/// but it no longer crosses the bridge and no longer has to be decoded in Dart. Either way the
/// interface paints it at the rate the camera captures, with [Texture] scaling and centring it like
/// any other image.
///
/// The stream path ([PreviewFeed]) stays the way every other platform shows a preview: it carries
/// finished RGBA frames through the bridge, which is more work per frame but needs no platform
/// support.
class PreviewTexture {
  PreviewTexture._();

  /// The channel the plugin registers in `ScwcPreviewPlugin.{m,cpp,cc}`.
  static const MethodChannel _channel = MethodChannel('scwc_preview');

  /// Whether this platform paints frames itself: where the plugin and the capture path both live.
  /// Everywhere else the stream path is the preview.
  static bool get available =>
      Platform.isMacOS ||
      Platform.isIOS ||
      Platform.isLinux ||
      Platform.isWindows ||
      Platform.isAndroid;

  /// Whether the platform's texture paints the camera's own buffer, so the capture path produces no
  /// frames at all while it shows.
  ///
  /// True on macOS and iOS, where the plugin hands the buffer the camera produced straight to
  /// Flutter. False on Linux and Windows, where the texture paints the scaled RGBA frame the capture
  /// path published for it — that frame is the stream path's own work, so the layout size keeps
  /// reaching the pipeline there.
  ///
  /// False on Android too, and worth saying why: the camera renders straight into the interface's
  /// texture there, so no frame is converted at all — but a camera that refuses that output
  /// previews from the stream instead, and the layout size has to keep reaching the capture path
  /// or that fallback would have no box to render into.
  static bool get paintsPlatformBuffer => Platform.isMacOS || Platform.isIOS;

  /// Registers a texture and starts publishing frames into it, answering its id — or `null` when
  /// the platform has no such path, or the plugin refused.
  ///
  /// The same texture comes back when it is already registered, so a caller may start on every
  /// rebuild without leaking textures.
  static Future<int?> start() async {
    if (!available) {
      return null;
    }
    try {
      return await _channel.invokeMethod<int>('start');
    } on PlatformException {
      // No texture means no preview here; the caller falls back to the stream.
      return null;
    } on MissingPluginException {
      return null;
    }
  }

  /// The shape of the picture the platform paints, or `null` while none has arrived.
  ///
  /// A camera may negotiate a frame size other than the one that was asked for, and an interface
  /// that letterboxes with the negotiated ratio shows the picture undistorted. What the platform
  /// answers is the size of the frame it paints — the camera's buffer on the Apple platforms, the
  /// scaled frame the capture path published on Linux and Windows — and both carry the camera's own
  /// ratio.
  static Future<(int, int)?> size() async {
    if (!available) {
      return null;
    }
    try {
      final size = await _channel.invokeMethod<List<Object?>>('size');
      if (size == null || size.length != 2) {
        return null;
      }
      final width = size[0];
      final height = size[1];
      if (width is! int || height is! int || width <= 0 || height <= 0) {
        return null;
      }
      return (width, height);
    } on PlatformException {
      return null;
    } on MissingPluginException {
      return null;
    }
  }

  /// The clockwise quarter turns the interface still has to turn the platform's picture by: 0,
  /// 1, 2 or 3, or `null` while the platform does not paint or has not answered yet.
  ///
  /// Zero on Android, and that is not an oversight: the camera renders into a `SurfaceTexture`,
  /// and the engine samples that texture with the buffer's own transform matrix, which already
  /// accounts for the sensor's orientation — the picture arrives upright. Turning it here as
  /// well is what makes a portrait preview show a landscape picture: the correction is applied
  /// twice, and the second one is a quarter turn clockwise that has nothing to correct.
  ///
  /// The platform still reports the sensor's own orientation through `rotation`, and it is worth
  /// reading in a log: a device whose camera service does not fill that transform matrix in is a
  /// device on which this has to be the sensor's quarter turns instead of zero.
  static Future<int?> quarterTurns() async {
    if (!available) {
      return null;
    }
    if (Platform.isAndroid) {
      return 0;
    }
    try {
      final degrees = await _channel.invokeMethod<int>('rotation');
      if (degrees == null) return null;
      return (degrees ~/ 90) % 4;
    } on PlatformException {
      return null;
    } on MissingPluginException {
      return null;
    }
  }

  /// Unregisters the texture: the preview stops and the frame pipeline is released.
  static Future<void> stop() async {
    if (!available) {
      return;
    }
    try {
      await _channel.invokeMethod<void>('stop');
    } on PlatformException {
      // Nothing is left to stop if the plugin is not there.
    } on MissingPluginException {
      // As above.
    }
  }
}
