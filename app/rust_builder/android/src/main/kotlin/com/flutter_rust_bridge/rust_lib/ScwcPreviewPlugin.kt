package com.flutter_rust_bridge.rust_lib

import android.view.Surface
import io.flutter.embedding.engine.plugins.FlutterPlugin
import io.flutter.plugin.common.MethodCall
import io.flutter.plugin.common.MethodChannel
import io.flutter.view.TextureRegistry

/**
 * The camera preview as a Flutter texture the camera writes into directly.
 *
 * The texture is created when the engine attaches and is never unregistered by `stop`: a capture
 * session's outputs are fixed when it is built, so handing the interface a different texture per
 * run would leave the session rendering into a surface nobody shows. [start] is therefore
 * idempotent and answers the same id every time.
 *
 * Nothing here runs per frame. The engine's own [TextureRegistry.SurfaceTextureEntry] installs
 * the frame-available listener that marks the texture dirty and schedules an engine frame, and
 * the engine calls `updateTexImage` itself — so attaching the window is the whole of the work,
 * and setting a listener here would replace the engine's and freeze the picture.
 */
class ScwcPreviewPlugin : FlutterPlugin, MethodChannel.MethodCallHandler {
    private var channel: MethodChannel? = null
    private var textures: TextureRegistry? = null
    private var entry: TextureRegistry.SurfaceTextureEntry? = null
    private var surface: Surface? = null

    override fun onAttachedToEngine(binding: FlutterPlugin.FlutterPluginBinding) {
        textures = binding.textureRegistry
        val channel = MethodChannel(binding.binaryMessenger, CHANNEL)
        channel.setMethodCallHandler(this)
        this.channel = channel
        createTexture()
    }

    override fun onDetachedFromEngine(binding: FlutterPlugin.FlutterPluginBinding) {
        channel?.setMethodCallHandler(null)
        channel = null
        releaseTexture()
        textures = null
    }

    /**
     * The texture, and the window behind it that the camera renders into.
     *
     * The default buffer size is a placeholder: Rust sets the size the camera actually renders
     * at on the window it is handed, from the stream's own geometry, before any session is built.
     */
    private fun createTexture() {
        val registry = textures ?: return
        // Must happen on a thread with a `Looper`: the engine's entry registers its
        // frame-available listener with a `Handler` built from `Looper.myLooper()`.
        val created = registry.createSurfaceTexture()
        created.surfaceTexture().setDefaultBufferSize(DEFAULT_WIDTH, DEFAULT_HEIGHT)
        val surface = Surface(created.surfaceTexture())
        entry = created
        this.surface = surface
        nativeAttachSurface(surface)
    }

    private fun releaseTexture() {
        nativeDetachSurface()
        surface?.release()
        surface = null
        entry?.release()
        entry = null
    }

    override fun onMethodCall(call: MethodCall, result: MethodChannel.Result) {
        when (call.method) {
            "start" -> {
                val id = entry?.id()
                // An id is only worth anything while the camera is rendering into this window:
                // a camera that refused the output combination previews from a YUV reader, and
                // the interface has to fall back to its stream rather than show a black box.
                if (id == null || !nativePreviewActive()) {
                    result.error("no-preview", "the camera is not rendering into this texture", null)
                } else {
                    result.success(id)
                }
            }
            // The texture outlives a run: see the class comment.
            "stop" -> result.success(null)
            "size" -> {
                val packed = nativePreviewSize()
                val width = (packed ushr 32).toInt()
                val height = (packed and 0xFFFFFFFFL).toInt()
                if (width <= 0 || height <= 0) result.success(null)
                else result.success(listOf(width, height))
            }
            "rotation" -> result.success(nativePreviewRotation())
            else -> result.notImplemented()
        }
    }

    private external fun nativeAttachSurface(surface: Surface)
    private external fun nativeDetachSurface()
    private external fun nativePreviewActive(): Boolean
    private external fun nativePreviewSize(): Long
    private external fun nativePreviewRotation(): Int

    companion object {
        private const val CHANNEL = "scwc_preview"
        private const val DEFAULT_WIDTH = 1280
        private const val DEFAULT_HEIGHT = 720

        init {
            // Dart loads this library through `dart:ffi`, whose `dlopen` does not register it
            // with the VM — without this, the `external` methods above stay unresolvable.
            // Loading a library the process already holds costs one more reference.
            System.loadLibrary("rust_lib")
        }
    }
}
