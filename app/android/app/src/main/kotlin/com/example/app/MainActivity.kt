package com.example.app

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.os.Bundle
import android.system.Os
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

/**
 * The host end of the runtime camera permission.
 *
 * Android grants `CAMERA` per application at runtime, so the manifest declaration is only half
 * of it: until the user answers the system dialog, `ACameraManager_openCamera` answers
 * `PERMISSION_DENIED`. The Dart side asks through the channel below before the first camera
 * use, and this activity reports back one boolean — whether the app may use the camera now. A
 * denial the user asked not to be asked about again comes back as `false` as well; Dart turns
 * that into the message that points at Settings.
 *
 * The activity also names this app's own data directory in the process environment: the client
 * keeps the server identity, the last capture parameters and its log on disk and asks the
 * platform where to put them, but an Android app process has no `HOME` for that lookup to find.
 * The directory the framework hands out is only reachable from the framework side, and the
 * native code starts after this method runs, so the environment is how it gets there.
 *
 * The framework's own `checkSelfPermission`/`requestPermissions` are used rather than the
 * AndroidX helpers: they arrived in API 23 and this client's floor is above that, while the
 * AndroidX dependency would be an assumption about what the Flutter embedding brings along.
 */
class MainActivity : FlutterActivity() {
    /** Where Dart asks; the name is matched in `lib/src/permissions.dart`. */
    private val channelName = "sc_webcam/permissions"

    /** The request code this activity's result callback is keyed on. */
    private val requestCode = 8101

    /** The Dart side's answer, held until the system dialog comes back. */
    private var pending: MethodChannel.Result? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        // Read by the client's own path lookup, which otherwise falls back to the desktop
        // conventions (`$HOME/.config`) that an app process cannot resolve.
        Os.setenv("SCWC_STORAGE_DIR", filesDir.absolutePath, true)
        super.onCreate(savedInstanceState)
    }

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, channelName)
            .setMethodCallHandler { call, result ->
                when (call.method) {
                    "requestCamera" -> requestCamera(result)
                    else -> result.notImplemented()
                }
            }
    }

    /**
     * Answers immediately when the permission is already held, and otherwise asks and keeps
     * [result] until the dialog is answered. A second call while the dialog is up is answered
     * `false` rather than queued: the platform shows one dialog at a time, and the caller is
     * already waiting on the first one.
     */
    private fun requestCamera(result: MethodChannel.Result) {
        if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED) {
            result.success(true)
            return
        }
        if (pending != null) {
            result.success(false)
            return
        }
        pending = result
        requestPermissions(arrayOf(Manifest.permission.CAMERA), requestCode)
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != this.requestCode) {
            return
        }
        val granted = grantResults.isNotEmpty() && grantResults[0] == PackageManager.PERMISSION_GRANTED
        pending?.success(granted)
        pending = null
    }
}
