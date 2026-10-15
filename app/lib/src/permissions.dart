import 'package:flutter/services.dart';

/// The runtime camera permission, asked for by the host platform.
///
/// Android grants `CAMERA` per application at runtime: the manifest declaration only makes the
/// app *eligible*, and `ACameraManager_openCamera` fails with `PERMISSION_DENIED` until the
/// user has answered the system dialog. The capture backend reports that failure as
/// `CaptureError::PermissionDenied`, which reaches the UI as a plain error — correct, but not
/// something an operator can act on. So the app asks before the first camera use, and turns the
/// answer into a message that says what to do about a refusal.
///
/// iOS and the desktop platforms bring their own prompts (`NSCameraUsageDescription` and the
/// system privacy settings), and the same channel is simply absent there, so
/// [ensureCameraPermission] succeeds and the platform's own path is the one that runs.
class CameraPermission {
  const CameraPermission._();

  /// The channel the host platform registers; see `MainActivity.kt`.
  static const MethodChannel _channel = MethodChannel('sc_webcam/permissions');

  /// Asks for the camera permission if it has not been granted yet.
  ///
  /// Returns `null` when the app may use the camera, and a message for the operator otherwise.
  /// A caller shows that message and does not go on to open a camera, because the open would
  /// fail anyway and the platform's silence is what the message exists to replace.
  static Future<String?> ensureCameraPermission() async {
    try {
      final granted = await _channel.invokeMethod<bool>('requestCamera');
      if (granted == true) {
        return null;
      }
      return 'Camera permission denied: allow it in Settings > Apps > SC Webcam > '
          'Permissions, then try again.';
    } on MissingPluginException {
      // No permission channel on this platform: the platform's own prompt (or its absence) is
      // the only mechanism there is, and the capture backend still reports a denial.
      return null;
    } on PlatformException catch (error) {
      return 'Camera permission could not be requested: ${error.message ?? error.code}';
    }
  }
}
