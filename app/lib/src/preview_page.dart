import 'package:app/src/agent_state.dart';
import 'package:app/src/preview_feed.dart';
import 'package:app/src/rust/api.dart';
import 'package:app/src/preview_texture.dart';
import 'package:flutter/material.dart';

/// The main page: the live camera picture and the agent's status.
class PreviewPage extends StatelessWidget {
  const PreviewPage({super.key, required this.state, required this.preview});

  final AgentState state;
  final PreviewFeed preview;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: state,
      builder: (context, _) => Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Expanded(
              child: PreviewView(
                preview: preview,
                // The status line stands in for the picture whenever there is none: before the
                // agent runs, while the camera is coming up, and once it stopped.
                placeholder: state.statusText,
              ),
            ),
            const SizedBox(height: 12),
            StatusCard(state: state),
          ],
        ),
      ),
    );
  }
}

/// The camera picture: the newest decoded frame centred with its aspect preserved, or a dark
/// placeholder with the agent's status while there is none.
///
/// The widget also tells the client which size to scale frames to, following its own layout, so a
/// window resize reaches the capture path within a frame.
class PreviewView extends StatefulWidget {
  const PreviewView({super.key, required this.preview, required this.placeholder});

  final PreviewFeed preview;

  /// Line shown over the dark placeholder while no frame is on screen.
  final String placeholder;

  @override
  State<PreviewView> createState() => _PreviewViewState();
}

class _PreviewViewState extends State<PreviewView> {
  int? _width;
  int? _height;
  bool _ownBuffer = false;

  @override
  void dispose() {
    // Nothing shows frames any more; 0x0 asks the capture path to stop converting them.
    setPreviewTargetSize(width: 0, height: 0);
    super.dispose();
  }

  /// Asks the client to scale frames to `width` x `height` pixels, once per size and source.
  ///
  /// A plain bridge call, not framework state: it only changes what the Rust side sends next. While
  /// the platform paints the camera's own buffer there is no frame path to size, so the capture path
  /// is told `0x0` — and the layout size goes back with the stream as soon as the picture comes from
  /// it again. A texture that paints a frame the capture path produced is the frame path, so it keeps
  /// the layout size: that frame is scaled to exactly this box before it is handed to Flutter.
  void _setTargetSize(int width, int height, {required bool ownBuffer}) {
    if (width == _width && height == _height && ownBuffer == _ownBuffer) return;
    _width = width;
    _height = height;
    _ownBuffer = ownBuffer;
    setPreviewTargetSize(
      width: ownBuffer ? 0 : width,
      height: ownBuffer ? 0 : height,
    );
  }

  @override
  Widget build(BuildContext context) {
    final ratio = MediaQuery.devicePixelRatioOf(context);
    return LayoutBuilder(
      builder: (context, constraints) {
        return ColoredBox(
          color: Colors.black,
          child: ListenableBuilder(
            listenable: widget.preview,
            builder: (context, _) {
              final textureId = widget.preview.textureId;
              // The frames are scaled for the pixels this area is painted with; the UI only centres
              // what it gets. Said here, where a change of source is seen as well as a change of
              // size.
              _setTargetSize(
                (constraints.maxWidth * ratio).round(),
                (constraints.maxHeight * ratio).round(),
                // A texture that paints the platform's own buffer is fed by no frame of ours; one
                // that paints a frame the capture path produced is sized by this box like the
                // stream's frames are.
                ownBuffer: textureId != null && PreviewTexture.paintsPlatformBuffer,
              );
              if (textureId != null) {
                // The platform paints the camera's own buffer, so the widget only has to give it a
                // box of the camera's shape: `AspectRatio` inside `Center` is what `contain` is for
                // the stream path's frames.
                final ratio = widget.preview.textureAspectRatio;
                if (ratio == null) {
                  return SizedBox.expand(child: Texture(textureId: textureId));
                }
                // `RotatedBox` rather than `Transform`: a transform paints the child turned but
                // leaves its layout size alone, so a sensor that lies on its side would keep a
                // landscape box. Rotating the box swaps the constraints it passes down and the
                // size it reports, so the `AspectRatio` below is measured against the turned
                // picture — which is the one on screen.
                return SizedBox.expand(
                  child: Center(
                    child: RotatedBox(
                      quarterTurns: widget.preview.textureQuarterTurns,
                      child: AspectRatio(
                        aspectRatio: ratio,
                        child: Texture(textureId: textureId),
                      ),
                    ),
                  ),
                );
              }
              final image = widget.preview.image;
              if (image == null) {
                return Center(
                  child: Text(
                    widget.placeholder,
                    style: const TextStyle(color: Colors.white70),
                  ),
                );
              }
              // The frame carries the aspect ratio of the camera, so `contain` centres it and
              // keeps it whole.
              return SizedBox.expand(
                child: RawImage(image: image, fit: BoxFit.contain),
              );
            },
          ),
        );
      },
    );
  }
}

/// What the agent is doing: its last state, the frame counters and the parameters in use.
class StatusCard extends StatelessWidget {
  const StatusCard({super.key, required this.state});

  final AgentState state;

  @override
  Widget build(BuildContext context) {
    final scheme = Theme.of(context).colorScheme;
    final color = state.hasError
        ? scheme.error
        : state.isRecording
        ? Colors.green.shade700
        : scheme.onSurface;
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text('Status', style: Theme.of(context).textTheme.titleMedium),
            const SizedBox(height: 8),
            Row(
              children: [
                Icon(Icons.circle, size: 12, color: color),
                const SizedBox(width: 8),
                Expanded(
                  child: Text(
                    state.statusText,
                    style: TextStyle(fontWeight: FontWeight.w600, color: color),
                  ),
                ),
              ],
            ),
            const SizedBox(height: 4),
            Text(state.statsText),
            if (state.appliedText != null) ...[
              const SizedBox(height: 4),
              Text(state.appliedText!),
            ],
          ],
        ),
      ),
    );
  }
}
