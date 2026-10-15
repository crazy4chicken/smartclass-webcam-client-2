//! Live preview: the camera picture an embedding application paints.
//!
//! A consumer picks one of two paths. The simple one takes finished frames — it registers a sink
//! and the pixel size of the area it wants the picture in ([`set_preview_sink`],
//! [`set_preview_target_size`]), the capture layer asks [`next_frame_size`] where to render the
//! next frame and calls [`publish_bgra`] with it, and the frame is converted to RGBA and scaled
//! *aspect-locked inside* the requested size right here, so the frames themselves carry no
//! letterboxing and the consumer only has to centre what it gets. Everything on that path is
//! converted and copied, so it suits the consumers that cannot take a platform buffer — unless
//! the producer renders the frame itself, converted and already at the right size, and calls
//! [`publish_rgba`]: then the only pass over those pixels is the producer's own.
//!
//! The other path takes the platform's own buffer ([`publish_native_frame`],
//! [`set_native_frame_listener`]) and is what a renderer that understands it wants: the newest
//! frame sits in [`native_frame`], retained, until the consumer renders it or a newer frame
//! replaces it, and nothing is converted, scaled or copied on the way. This module never touches
//! the buffer itself — it is an opaque pointer here — so the platforms keep their own retain and
//! release.
//!
//! A platform whose renderer takes plain pixels gets the same frames without the bridge
//! ([`set_rgba_frame_listener`], [`take_rgba_frame`]): the newest finished frame waits in the slot
//! of [`publish_rgba_frame`] for the texture to paint it, and the slot holds it — one frame, never
//! a queue — until the texture takes it or a newer frame replaces it. That is the stream path's own
//! work at the stream path's own size, with the consumer being a texture instead of a bridge.
//!
//! The RGBA path is consumer-paced. [`next_frame_size`] hands out the size of the next frame only
//! while the consumer is ready for it — no sink, no target box, a texture nobody takes frames from,
//! or a frame the consumer has not reported through [`preview_frame_consumed`] all mean no frame at
//! all — so a producer never renders, converts or scales anything a consumer cannot keep up with.
//! The Apple shim renders at exactly that size, which is what keeps a preview from ever costing the
//! full capture resolution. The native path needs no pacing: the consumer renders the newest buffer
//! whenever it likes, and the frames it misses cost a retain and a release.
//!
//! Nothing on either path can fail the capture: a frame nobody consumes is dropped, and no frame
//! is produced at all while the consumer shows no preview.

use std::ffi::c_void;
use std::mem::replace;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use tokio::sync::mpsc::UnboundedSender;

/// One frame of the live camera picture.
///
/// The pixels are already in the layout an image widget wants: row-major RGBA, four bytes per
/// pixel, no row padding, scaled to fit the size the caller asked for.
#[derive(Debug, Clone)]
pub struct PreviewFrame {
    /// Row-major pixels without padding, four bytes each: red, green, blue, alpha.
    pub rgba: Vec<u8>,
    /// Frame width in pixels, after scaling.
    pub width: u32,
    /// Frame height in pixels, after scaling.
    pub height: u32,
}

/// The sink frames are offered to, or `None` while the caller shows no preview.
static SINK: Mutex<Option<UnboundedSender<PreviewFrame>>> = Mutex::new(None);

/// Width of the box frames are scaled to fit; zero while the preview is off.
static TARGET_WIDTH: AtomicU32 = AtomicU32::new(0);

/// Height of the box frames are scaled to fit; zero while the preview is off.
static TARGET_HEIGHT: AtomicU32 = AtomicU32::new(0);

/// Whether the consumer is ready for another frame.
///
/// Taken when a frame is offered ([`publish_bgra`]) and handed back when the consumer says it is
/// done with it ([`preview_frame_consumed`]) — or when a sink is registered, or a target size is
/// set, both of which mean nothing is in flight and one frame may go out. This is the whole
/// pacing mechanism: a consumer that is behind simply finds the permit gone, and the capture
/// path does no preview work at all until it is back.
static PERMIT: AtomicBool = AtomicBool::new(true);

/// The newest camera frame published as the platform's own buffer, or null while there is none.
///
/// Opaque here on purpose: this module stores the pointer and hands it on, and the platform that
/// produced the buffer is the one that retains and releases it.
static NATIVE_FRAME: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// The consumer told when a native frame is published, if one is registered.
static NATIVE_LISTENER: Mutex<Option<fn()>> = Mutex::new(None);

/// Whether a native-frame consumer is registered, mirrored out of the mutex so that a capture
/// callback can ask without taking a lock on every frame.
static NATIVE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The newest finished frame published for a pixel-buffer texture, or null while there is none.
///
/// Opaque here on purpose, like [`NATIVE_FRAME`]: the frame is a [`PreviewFrame`] boxed by
/// [`publish_rgba_frame`], and freeing a box is the consumer's business — this module only swaps one
/// pointer for another, which is what keeps a texture's read of the slot free of locks.
static RGBA_FRAME: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// The pixel-buffer texture consumer: the code told when a frame is published, and the owner of the
/// frames the slot holds.
static RGBA_CONSUMER: Mutex<Option<RgbaFrameConsumer>> = Mutex::new(None);

/// Whether a pixel-buffer texture consumer is registered, mirrored out of the mutex for the capture
/// path's per-frame question.
static RGBA_ACTIVE: AtomicBool = AtomicBool::new(false);

/// The consumer of the frames published for a pixel-buffer texture.
///
/// The frames are boxed [`PreviewFrame`]s, and the slot hands them over whole: the consumer is the
/// code that knows how to free them, so it is the code that owns the slot's allocations. Both hooks
/// are plain function pointers rather than closures, so that a capture callback can reach them
/// without owning anything.
#[derive(Clone, Copy)]
pub struct RgbaFrameConsumer {
    /// Called once per published frame, from the thread that captured it.
    ///
    /// A texture's answer is a "repaint me" notice, so it must return promptly.
    pub ready: fn(),
    /// Frees one frame the slot hands over.
    ///
    /// The pointer is the one [`publish_rgba_frame`] published — a `Box<PreviewFrame>` — and the
    /// consumer releases it exactly once: when a published frame is replaced before it is taken, and
    /// when the consumer stops with a frame still in the slot.
    pub release: fn(*mut c_void),
}

/// Registers the sink the live preview is offered to; `None` stops it.
///
/// Replacing a sink drops the previous one, which ends the receiver its consumer awaits, and
/// arms the next frame: nothing is in flight for a consumer that just arrived. Every frame is
/// offered without waiting, so a slow consumer loses frames rather than slowing the camera down.
pub fn set_preview_sink(sink: Option<UnboundedSender<PreviewFrame>>) {
    *sink_slot() = sink;
    arm();
}

/// Sets the pixel size the preview frames are scaled to fit; `0x0` disables the preview.
///
/// The picture keeps its aspect ratio and is scaled to fit *inside* this box, so the frames a
/// caller receives are as tall or as wide as the camera's own picture allows — never the box
/// itself with the letterbox baked in. Like a new sink, this arms the next frame: a caller that
/// resizes its preview is laying it out again and wants a frame for it.
pub fn set_preview_target_size(width: u32, height: u32) {
    TARGET_WIDTH.store(width, Ordering::Relaxed);
    TARGET_HEIGHT.store(height, Ordering::Relaxed);
    arm();
}

/// Whether a preview is configured at all: a target size is set and something shows the picture —
/// a sink, or a pixel-buffer texture that paints frames produced for it.
///
/// This says nothing about whether a frame may be produced *now* — a consumer that has not
/// reported the frame it was handed is behind — so a producer asks [`next_frame_size`] rather
/// than this.
pub fn preview_enabled() -> bool {
    target_size().is_some() && (sink_slot().is_some() || rgba_preview_active())
}

/// The size to render the next frame at, or `None` when no frame may be produced now.
///
/// `None` means one of three things, and a producer that gets it does nothing at all: no sink is
/// registered, no target box is set, or the consumer has not finished with the frame it was
/// handed. A frame without pixels answers `None` too, since there is nothing to render from it.
/// Otherwise the answer is the size to produce the frame *at*: `source_width` and
/// `source_height` are the frame the producer holds, and the result is that frame's aspect
/// ratio fitted inside the target box — so a producer that renders at this size does the work
/// of a preview and never the work of a full-resolution capture frame.
pub fn next_frame_size(source_width: u32, source_height: u32) -> Option<(u32, u32)> {
    if source_width == 0 || source_height == 0 {
        return None;
    }
    if !preview_enabled() || !permit_available() {
        return None;
    }

    let (target_width, target_height) = target_size()?;
    Some(fit(source_width, source_height, target_width, target_height))
}

/// The consumer is done with the frame it was handed, so one more may be produced.
///
/// The other half of the pacing [`next_frame_size`] starts: without this, exactly one frame
/// every time a sink or a target size is set, and nothing while the consumer is behind.
pub fn preview_frame_consumed() {
    arm();
}

/// Routes one finished frame to whichever consumer is registered.
///
/// Shared by [`publish_bgra`] and [`publish_rgba`]: once the pixels are in the layout the
/// consumer wants the routing is the same — the pixel-buffer texture wins while one is
/// registered, the channel is unbounded, and a frame a texture never took is handed back to
/// the consumer to free. The permit is the caller's to take before it produces the pixels;
/// here a frame simply goes out, or is dropped where a consumer that left receives nothing.
fn publish_converted_frame(frame: PreviewFrame) {
    let texture = rgba_preview_active();
    let sink = if texture {
        None
    } else {
        sink_slot().as_ref().cloned()
    };
    if sink.is_none() && !texture {
        return;
    }
    if texture {
        if let Some(replaced) = publish_rgba_frame(frame) {
            // A frame the texture never took: the slot kept it, and the consumer is what frees it.
            release_rgba_frame(replaced);
        }
        return;
    }
    // The channel is unbounded, so this only fails once the consumer is gone.
    if let Some(sender) = sink {
        let _ = sender.send(frame);
    }
}

/// Converts one captured BGRA frame and offers it to the consumer.
///
/// `pixels` is the frame as the backend hands it over: tightly packed BGRA, four bytes per
/// pixel, row-major, `width * height` of them. The frame is scaled aspect-locked into the
/// target size, its channels swapped to RGBA, and offered without waiting — a caller that is
/// behind loses this frame instead of holding up the capture.
///
/// The frame goes to the pixel-buffer texture while one is registered, and to the sink otherwise:
/// both want the same picture at the same size, and a texture is only registered while the
/// application paints what it says — the two paths are alternatives, and the texture's is the
/// cheaper one.
///
/// Does nothing while no frame may be produced — see [`next_frame_size`] — which is what lets a
/// producer call this unconditionally. A backend that can render the frame itself, already
/// converted and already at the size [`next_frame_size`] names, calls [`publish_rgba`] instead
/// and pays for no second pass over every pixel.
pub fn publish_bgra(pixels: &[u8], width: u32, height: u32) {
    let Some((target_width, target_height)) = target_size() else {
        return;
    };
    let Some(source) = Bgra::new(pixels, width, height) else {
        return;
    };
    // Looked at before the conversion, so a caller that stops listening (the sink was
    // replaced, or the run ended) neither waits for a frame being converted nor pays for one
    // it will never see; the sender itself is looked up again by the routing, which is where
    // the frame actually goes.
    if sink_slot().is_none() && !rgba_preview_active() {
        return;
    }
    // The frame is a frame, so it takes the consumer's permit — and gives it back only when the
    // consumer reports it, by taking it or through [`preview_frame_consumed`]. Until then, nothing
    // more is produced for it, exactly as if the queue were full.
    if !consume_permit() {
        return;
    }

    let (out_width, out_height) = fit(source.width, source.height, target_width, target_height);
    let frame = PreviewFrame {
        rgba: source.scale_to(out_width, out_height),
        width: out_width,
        height: out_height,
    };
    publish_converted_frame(frame);
}

/// Offers one already-converted RGBA frame to the consumer.
///
/// `rgba` is row-major RGBA, four bytes per pixel, no row padding, `width` x `height` of them —
/// the layout [`PreviewFrame`] carries, so the pixels move exactly once, from the producer's
/// buffer into the frame that goes out. Nothing here converts, rescales or letterboxes: a
/// producer that renders the frame itself at the size [`next_frame_size`] names — a fused
/// colour conversion, a scaled hardware path — hands the finished picture over, and the
/// consumer sees the frame [`publish_bgra`] would have produced minus the second pass.
///
/// The same permit, the same consumers and the same drop policy as [`publish_bgra`] apply.
/// `false` means no frame may be produced now — no target box, a geometry that does not
/// describe its pixels, no consumer, or a consumer that has not reported the frame it was
/// handed — and nothing is copied for one.
pub fn publish_rgba(rgba: &[u8], width: u32, height: u32) -> bool {
    if target_size().is_none() {
        return false;
    }
    let needed = usize::try_from(u64::from(width) * u64::from(height) * 4).ok();
    if width == 0 || height == 0 || needed != Some(rgba.len()) {
        return false;
    }
    if sink_slot().is_none() && !rgba_preview_active() {
        return false;
    }
    if !consume_permit() {
        return false;
    }
    let frame = PreviewFrame {
        rgba: rgba.to_vec(),
        width,
        height,
    };
    publish_converted_frame(frame);
    true
}

/// Registers the consumer told when a new native frame is published; `None` stops it.
///
/// A native consumer is what [`native_preview_active`] reports and what [`publish_native_frame`]
/// notifies; the buffer itself is the consumer's to take with [`native_frame`]. Registering a
/// listener does not publish a frame: the next captured one arrives within a frame time.
pub fn set_native_frame_listener(listener: Option<fn()>) {
    *listener_slot() = listener;
    NATIVE_ACTIVE.store(listener.is_some(), Ordering::Release);
}

/// Whether a consumer of native frames is registered.
///
/// Cheap enough for a capture callback to check per frame, which is the point: a pipeline that
/// publishes native frames does the retain and the release only while something takes them.
pub fn native_preview_active() -> bool {
    NATIVE_ACTIVE.load(Ordering::Acquire)
}

/// Publishes `frame` as the newest native frame, returning the one it replaced.
///
/// `frame` is a platform buffer the caller has retained for the slot: this module only keeps the
/// pointer, and hands the consumer the same pointer through [`native_frame`]. A `Some` answer is
/// the buffer this publish replaced, whose reference is the caller's to release through its own
/// platform API — nothing is released here, because nothing here knows what a refcount is. The
/// registered listener, if any, is told once.
pub fn publish_native_frame(frame: *mut c_void) -> Option<*mut c_void> {
    let replaced = NATIVE_FRAME.swap(frame, Ordering::AcqRel);
    let listener = *listener_slot();
    if let Some(listener) = listener {
        listener();
    }
    (!replaced.is_null()).then_some(replaced)
}

/// The newest native frame, or null while none has been published.
///
/// The pointer is the published one, not a reference of the caller's: a consumer that keeps it
/// past the next publish has to retain it through its own platform API.
pub fn native_frame() -> *mut c_void {
    NATIVE_FRAME.load(Ordering::Acquire)
}

/// Removes the newest native frame, handing its reference to the caller to release.
///
/// This is what a consumer calls when it stops: afterwards no frame is published until the next
/// capture frame arrives.
pub fn take_native_frame() -> Option<*mut c_void> {
    let frame = NATIVE_FRAME.swap(ptr::null_mut(), Ordering::AcqRel);
    (!frame.is_null()).then_some(frame)
}

/// The listener slot, with a poisoned lock treated as an empty one.
fn listener_slot() -> MutexGuard<'static, Option<fn()>> {
    NATIVE_LISTENER.lock().unwrap_or_else(|error| error.into_inner())
}

/// Registers the consumer of the frames published for a pixel-buffer texture; `None` stops it.
///
/// A registered consumer is what makes the capture path produce a preview at all where nothing is
/// subscribed to the sink, which is the point: the frames are the same ones, converted and scaled
/// once, for a texture to paint instead of for a bridge to carry. The frames in the slot when a
/// consumer goes are handed back to it to free — nothing may paint them, and this module does not
/// own them.
pub fn set_rgba_frame_listener(consumer: Option<RgbaFrameConsumer>) {
    let previous = replace(&mut *consumer_slot(), consumer);
    RGBA_ACTIVE.store(consumer.is_some(), Ordering::Release);
    if let Some(previous) = previous {
        if let Some(frame) = clear_rgba_frame() {
            (previous.release)(frame);
        }
    }
}

/// Whether a consumer of pixel-buffer frames is registered.
///
/// Cheap enough for a capture callback to check per frame, and the question a producer asks before
/// it converts anything for a texture nobody paints.
pub fn rgba_preview_active() -> bool {
    RGBA_ACTIVE.load(Ordering::Acquire)
}

/// Publishes `frame` as the newest pixel-buffer frame, returning the one it replaced.
///
/// The frame is boxed here and handed over as an opaque pointer, because the consumer frees it: a
/// `Some` answer is the frame this publish replaced, which the registered consumer's `release` hook
/// gets when [`publish_bgra`] is what published. The registered listener, if any, is told once.
pub fn publish_rgba_frame(frame: PreviewFrame) -> Option<*mut c_void> {
    let frame = Box::into_raw(Box::new(frame)).cast::<c_void>();
    let replaced = RGBA_FRAME.swap(frame, Ordering::AcqRel);
    let consumer = *consumer_slot();
    if let Some(consumer) = consumer {
        (consumer.ready)();
    }
    (!replaced.is_null()).then_some(replaced)
}

/// Takes the newest pixel-buffer frame for the consumer, which owns it from here on.
///
/// The read is what the pacing waits for, exactly as [`preview_frame_consumed`] is on the stream
/// path: a producer may publish one more frame from here on, and nothing is converted for a texture
/// that paints less often than the camera captures. `None` means no frame has been published since
/// the last one was taken — a texture paints the frame it holds again rather than clearing itself.
pub fn take_rgba_frame() -> Option<*mut c_void> {
    let frame = RGBA_FRAME.swap(ptr::null_mut(), Ordering::AcqRel);
    if frame.is_null() {
        return None;
    }
    arm();
    Some(frame)
}

/// Empties the slot without arming a frame, for a consumer that stops.
fn clear_rgba_frame() -> Option<*mut c_void> {
    let frame = RGBA_FRAME.swap(ptr::null_mut(), Ordering::AcqRel);
    (!frame.is_null()).then_some(frame)
}

/// Frees one frame the slot replaced, through the consumer that owns the allocation.
fn release_rgba_frame(frame: *mut c_void) {
    if let Some(consumer) = *consumer_slot() {
        (consumer.release)(frame);
    }
}

/// The texture-consumer slot, with a poisoned lock treated as an empty one.
fn consumer_slot() -> MutexGuard<'static, Option<RgbaFrameConsumer>> {
    RGBA_CONSUMER.lock().unwrap_or_else(|error| error.into_inner())
}

/// Arms the next frame: nothing is in flight, so one more may be produced.
fn arm() {
    PERMIT.store(true, Ordering::Release);
}

/// Whether the consumer is ready for a frame.
fn permit_available() -> bool {
    PERMIT.load(Ordering::Acquire)
}

/// Takes the consumer's permit, or `false` when it is not ready for another frame.
fn consume_permit() -> bool {
    PERMIT
        .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// The frame a backend handed over, checked against the geometry it claims.
struct Bgra<'a> {
    pixels: &'a [u8],
    width: u32,
    height: u32,
}

impl<'a> Bgra<'a> {
    /// Borrows a packed BGRA frame, or `None` when its geometry does not describe `pixels`.
    fn new(pixels: &'a [u8], width: u32, height: u32) -> Option<Self> {
        let needed = usize::try_from(u64::from(width) * u64::from(height) * 4).ok()?;
        (width > 0 && height > 0 && pixels.len() >= needed).then_some(Self {
            pixels,
            width,
            height,
        })
    }

    /// Scales the frame to exactly `width`x`height` and swaps it to RGBA.
    ///
    /// Nearest neighbour, sampling directly out of the source: the capture thread is a camera
    /// callback that must not block, and a preview is read by an eye that cannot tell the
    /// difference at the sizes this is used at. Alpha is written opaque — a camera picture has
    /// no transparency.
    fn scale_to(&self, width: u32, height: u32) -> Vec<u8> {
        let source_width = self.width as usize;
        let source_height = self.height as usize;
        let width = width as usize;
        let height = height as usize;
        let mut rgba = vec![0u8; width * height * 4];

        for row in 0..height {
            let source_row = row * source_height / height;
            let source_base = source_row * source_width * 4;
            for column in 0..width {
                let source_column = column * source_width / width;
                let source = source_base + source_column * 4;
                let target = (row * width + column) * 4;
                rgba[target] = self.pixels[source + 2];
                rgba[target + 1] = self.pixels[source + 1];
                rgba[target + 2] = self.pixels[source];
                rgba[target + 3] = u8::MAX;
            }
        }
        rgba
    }
}

/// The box frames are scaled to fit, or `None` while the preview is off.
fn target_size() -> Option<(u32, u32)> {
    let width = TARGET_WIDTH.load(Ordering::Relaxed);
    let height = TARGET_HEIGHT.load(Ordering::Relaxed);
    (width > 0 && height > 0).then_some((width, height))
}

/// The size a `source` frame is scaled to, aspect-locked inside `target`.
///
/// The picture is fitted, never stretched: the result touches two opposite sides of the target
/// box and is smaller on the other two. Cross-multiplied integers rather than floating point,
/// so the sizes a caller reads back are exact.
fn fit(source_width: u32, source_height: u32, target_width: u32, target_height: u32) -> (u32, u32) {
    let source_width = u64::from(source_width);
    let source_height = u64::from(source_height);
    let target_width = u64::from(target_width);
    let target_height = u64::from(target_height);

    if target_width * source_height <= target_height * source_width {
        // The target is relatively wider than the picture: the width is what limits it.
        let height = ((target_width * source_height + source_width / 2) / source_width).max(1);
        (target_width as u32, height.min(target_height) as u32)
    } else {
        let width = ((target_height * source_width + source_height / 2) / source_height).max(1);
        (width.min(target_width) as u32, target_height as u32)
    }
}

/// The sink slot, with a poisoned lock treated as an empty one.
fn sink_slot() -> MutexGuard<'static, Option<UnboundedSender<PreviewFrame>>> {
    SINK.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::{
        fit, native_frame, native_preview_active, next_frame_size, preview_enabled,
        preview_frame_consumed, publish_bgra, publish_native_frame, publish_rgba,
        rgba_preview_active, set_native_frame_listener, set_preview_sink, set_preview_target_size,
        set_rgba_frame_listener, take_native_frame, take_rgba_frame, PreviewFrame,
        RgbaFrameConsumer,
    };
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc::UnboundedReceiver;

    /// Serializes the tests below: the preview's sink and target size are process-wide, so two
    /// tests touching them at once would see each other's state.
    static PREVIEW_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Takes the lock the stateful tests share, and clears the state they leave behind.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        let guard = PREVIEW_TESTS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        set_preview_sink(None);
        set_preview_target_size(0, 0);
        set_native_frame_listener(None);
        // The frames these tests publish are invented pointers, and the platform that would own
        // their references is not here, so the slot is simply emptied.
        take_native_frame();
        // A texture consumer is cleared through its own hook, which is what frees the frame the
        // slot still holds; the previous test's counter is what that costs, not a leak.
        set_rgba_frame_listener(None);
        take_rgba_frame();
        guard
    }

    /// Registers a sink and a target size, and hands back the receiver.
    fn preview(width: u32, height: u32) -> UnboundedReceiver<PreviewFrame> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        set_preview_sink(Some(sender));
        set_preview_target_size(width, height);
        receiver
    }

    #[test]
    fn a_picture_is_fitted_inside_the_target_never_stretched() {
        // Wider than the box: the width is the limit.
        assert_eq!(fit(1920, 1080, 640, 480), (640, 360));
        // Taller than the box: the height is the limit.
        assert_eq!(fit(1080, 1920, 640, 480), (270, 480));
        // The same aspect ratio: the box, exactly.
        assert_eq!(fit(1920, 1080, 1920, 1080), (1920, 1080));
        // A box narrower than one pixel still yields a picture.
        assert_eq!(fit(4000, 3000, 1, 1), (1, 1));
        // Rounding lands on the nearer side.
        assert_eq!(fit(3, 2, 5, 5), (5, 3));
    }

    #[test]
    fn a_frame_is_fitted_for_the_shape_it_is_shown_in() {
        // The Android backend turns its sensor's frame before showing it, and a quarter turn
        // swaps the source's sides: a 1280x720 landscape frame is a 720x1280 portrait picture.
        // The backend therefore fits the *shown* shape, so the size it renders at is already
        // the final one and fitting it again — which is what a second pass would do — is the
        // same size. Regression guard: fitting the un-turned shape first and rotating the
        // result afterwards leaves a frame that no longer matches the box, and a producer
        // that re-fits it then rescales every pixel a second time, into a picture nobody
        // asked to be any larger.
        let _guard = exclusive();
        let mut frames = preview(1176, 1645);
        assert_eq!(next_frame_size(720, 1280), Some((925, 1645)));

        let rgba = vec![0u8; 925 * 1645 * 4];
        assert!(publish_rgba(&rgba, 925, 1645));
        let frame = frames.try_recv().expect("the frame the producer made");
        assert_eq!(
            (frame.width, frame.height),
            (925, 1645),
            "the frame goes out at the size it was produced at"
        );
    }

    #[test]
    fn a_published_frame_arrives_scaled_and_swapped_to_rgba() {
        let _guard = exclusive();
        // 4x2 BGRA, one distinct colour per column, so the swap is visible in every pixel.
        let source: Vec<u8> = [10u8, 20, 30, 0, 40, 50, 60, 0]
            .iter()
            .copied()
            .cycle()
            .take(4 * 2 * 4)
            .collect();
        let mut frames = preview(8, 8);
        assert!(preview_enabled());

        publish_bgra(&source, 4, 2);

        let frame = frames.try_recv().expect("one frame was offered");
        assert_eq!((frame.width, frame.height), (8, 4), "4:2 -> 8x8 fits at 8x4");
        assert_eq!(frame.rgba.len(), 8 * 4 * 4);
        // The first source pixel is B=10, G=20, R=30; RGBA wants it the other way around.
        assert_eq!(&frame.rgba[0..4], &[30, 20, 10, u8::MAX]);
    }

    #[test]
    fn an_already_converted_frame_goes_out_untouched() {
        let _guard = exclusive();
        let mut frames = preview(8, 8);

        // A producer that rendered the frame itself hands over exactly the pixels it made, at
        // exactly the size it made them at: no second pass, no re-fit, no re-scale.
        let rgba: Vec<u8> = (0..(8 * 4 * 4)).map(|i| i as u8).collect();
        assert!(publish_rgba(&rgba, 8, 4));

        let frame = frames.try_recv().expect("one frame was offered");
        assert_eq!((frame.width, frame.height), (8, 4), "the frame keeps the size it was given");
        assert_eq!(frame.rgba, rgba, "the pixels move once and land unmodified");

        // The same pacing the BGRA path answers to: nothing goes out while the permit is out.
        assert!(!publish_rgba(&rgba, 8, 4), "the consumer holds the frame it was given");
        preview_frame_consumed();
        assert!(publish_rgba(&rgba, 8, 4));
        assert!(frames.try_recv().is_ok());
    }

    #[test]
    fn an_already_converted_frame_is_refused_without_a_consumer_or_a_box() {
        let _guard = exclusive();
        set_preview_target_size(0, 0);
        let rgba = [0u8; 8 * 4 * 4];
        assert!(!publish_rgba(&rgba, 8, 4), "no box, no frame");

        // A sender that is dropped at once: the point is that a texture-less, sink-less preview
        // is no consumer, whichever way the frame arrives.
        let (_sender, _receiver) = tokio::sync::mpsc::unbounded_channel::<PreviewFrame>();
        set_preview_target_size(8, 8);
        // No sink and no texture is no consumer, whatever the frame is.
        assert!(!publish_rgba(&rgba, 8, 4), "no consumer, no frame");
        // A geometry that does not describe its pixels is not a frame.
        let mut frames = preview(8, 8);
        assert!(!publish_rgba(&[0u8; 15], 8, 4));
        assert!(!publish_rgba(&rgba, 0, 4));
        assert!(frames.try_recv().is_err(), "nothing went out");
    }

    #[test]
    fn nothing_is_converted_while_the_preview_is_off() {
        let _guard = exclusive();
        let mut frames = preview(0, 0);
        assert!(!preview_enabled(), "a zero-sized target means no preview");
        publish_bgra(&[0, 0, 0, 0], 1, 1);
        assert!(frames.try_recv().is_err(), "a disabled preview gets no frame");

        // A target with one axis at zero is no target either.
        for (width, height) in [(0, 2), (4, 0)] {
            set_preview_target_size(width, height);
            assert!(!preview_enabled(), "{width}x{height} is not a box");
            publish_bgra(&[0u8; 32], 4, 2);
            assert!(frames.try_recv().is_err(), "{width}x{height} gets no frame");
        }

        // A target nobody is waiting for is no preview either.
        set_preview_target_size(4, 2);
        set_preview_sink(None);
        assert!(!preview_enabled(), "no sink means no preview");
        publish_bgra(&[0u8; 32], 4, 2);
        assert!(frames.try_recv().is_err(), "a dropped sink gets no frame");
    }

    #[test]
    fn a_frame_is_only_produced_while_the_consumer_is_ready() {
        let _guard = exclusive();
        let mut frames = preview(8, 8);
        // What a producer renders at: the source's aspect ratio inside the box, not the source.
        let frame = vec![0u8; 8 * 4 * 4];

        assert_eq!(next_frame_size(4, 2), Some((8, 4)));
        publish_bgra(&frame, 8, 4);
        assert!(frames.try_recv().is_ok(), "the first frame goes out");

        // The consumer has it, so nothing is produced and nothing is even rendered until it
        // says it is done.
        assert_eq!(next_frame_size(4, 2), None, "no permit, no size to render at");
        publish_bgra(&frame, 8, 4);
        assert!(frames.try_recv().is_err(), "a busy consumer gets no frame");

        preview_frame_consumed();
        assert_eq!(next_frame_size(4, 2), Some((8, 4)));
        publish_bgra(&frame, 8, 4);
        assert!(frames.try_recv().is_ok(), "the next frame goes out");
    }

    #[test]
    fn a_new_sink_or_a_resize_arms_the_next_frame() {
        let _guard = exclusive();
        let mut frames = preview(8, 8);
        let frame = vec![0u8; 16 * 8 * 4];

        publish_bgra(&frame, 8, 4);
        assert!(frames.try_recv().is_ok());
        assert_eq!(next_frame_size(4, 2), None, "the consumer has that one");

        // A caller that resizes its preview is laying it out again: one frame may go out, at
        // the new box's size.
        set_preview_target_size(16, 16);
        assert_eq!(next_frame_size(4, 2), Some((16, 8)));

        // So is a caller that registers a sink.
        publish_bgra(&frame, 16, 8);
        assert!(frames.try_recv().is_ok());
        let (sender, mut replacement) = tokio::sync::mpsc::unbounded_channel();
        set_preview_sink(Some(sender));
        assert_eq!(next_frame_size(4, 2), Some((16, 8)));
        publish_bgra(&frame, 16, 8);
        assert!(replacement.try_recv().is_ok(), "the new sink gets a frame");
    }

    #[test]
    fn a_frame_whose_geometry_does_not_match_its_pixels_is_ignored() {
        let _guard = exclusive();
        let mut frames = preview(64, 64);
        publish_bgra(&[0u8; 15], 4, 4);
        publish_bgra(&[], 0, 0);
        assert!(frames.try_recv().is_err());
    }

    #[test]
    fn a_native_frame_replaces_the_one_before_it_and_is_handed_back_to_be_released() {
        let _guard = exclusive();
        static NOTIFIED: AtomicUsize = AtomicUsize::new(0);
        fn notify() {
            NOTIFIED.fetch_add(1, Ordering::Relaxed);
        }

        // Nothing consumes native frames until a listener is registered, and that is what the
        // capture path asks before it retains anything.
        assert!(!native_preview_active());
        assert_eq!(native_frame(), ptr::null_mut());

        set_native_frame_listener(Some(notify));
        assert!(native_preview_active());

        let first = 0x1000usize as *mut std::ffi::c_void;
        let second = 0x2000usize as *mut std::ffi::c_void;
        assert_eq!(
            publish_native_frame(first),
            None,
            "nothing was published before"
        );
        assert_eq!(NOTIFIED.load(Ordering::Relaxed), 1, "the listener was told");
        assert_eq!(native_frame(), first);

        // A newer frame takes the slot, and the one it took is handed back for the publisher to
        // release through its own platform API.
        assert_eq!(publish_native_frame(second), Some(first));
        assert_eq!(NOTIFIED.load(Ordering::Relaxed), 2);
        assert_eq!(native_frame(), second);

        // Taking the newest frame empties the slot, so nothing is handed out twice.
        assert_eq!(take_native_frame(), Some(second));
        assert_eq!(native_frame(), ptr::null_mut());
        assert_eq!(take_native_frame(), None);

        // A publisher that keeps publishing while nothing listens swaps the slot but tells
        // nobody — which is why it asks `native_preview_active` first.
        set_native_frame_listener(None);
        assert!(!native_preview_active());
        assert_eq!(publish_native_frame(first), None);
        assert_eq!(NOTIFIED.load(Ordering::Relaxed), 2, "no listener, no notice");
        assert_eq!(take_native_frame(), Some(first));
    }

    #[test]
    fn a_texture_consumer_is_a_consumer_like_any_other() {
        let _guard = exclusive();
        static READY: AtomicUsize = AtomicUsize::new(0);
        static RELEASED: AtomicUsize = AtomicUsize::new(0);
        fn ready() {
            READY.fetch_add(1, Ordering::Relaxed);
        }
        fn release(_frame: *mut c_void) {
            RELEASED.fetch_add(1, Ordering::Relaxed);
        }
        let source = [0u8; 4 * 2 * 4];

        // A texture and a sink are the same kind of consumer, so a texture alone is a preview:
        // that is what makes the capture path produce frames for it with nothing subscribed.
        set_preview_target_size(8, 8);
        assert!(!preview_enabled(), "no consumer and no texture is no preview");
        assert_eq!(next_frame_size(4, 2), None);

        set_rgba_frame_listener(Some(RgbaFrameConsumer { ready, release }));
        assert!(rgba_preview_active());
        assert!(preview_enabled(), "a texture is what the frames are for");
        assert_eq!(next_frame_size(4, 2), Some((8, 4)));

        publish_bgra(&source, 4, 2);
        assert_eq!(READY.load(Ordering::Relaxed), 1, "the texture was told");
        let first = take_rgba_frame().expect("the frame the texture paints");
        assert_eq!(take_rgba_frame(), None, "a frame is handed out once");
        assert_eq!(next_frame_size(4, 2), Some((8, 4)), "taking is the report");

        publish_bgra(&source, 4, 2);
        let second = take_rgba_frame().expect("the frame after it");
        assert_ne!(first, second, "a newer frame is its own frame");
        assert_eq!(RELEASED.load(Ordering::Relaxed), 0, "both were taken");

        // The slot keeps one frame: a newer frame replaces the one nobody took, and the replaced
        // frame goes back to the consumer to free.
        publish_bgra(&source, 4, 2);
        preview_frame_consumed();
        publish_bgra(&source, 4, 2);
        assert_eq!(RELEASED.load(Ordering::Relaxed), 1, "the replaced frame went");
        assert!(take_rgba_frame().is_some(), "the newest frame was waiting");

        // Stopping hands the frame in the slot back and stops the path.
        publish_bgra(&source, 4, 2);
        set_rgba_frame_listener(None);
        assert!(!rgba_preview_active());
        assert_eq!(RELEASED.load(Ordering::Relaxed), 2, "the last frame went");
        assert_eq!(take_rgba_frame(), None);
        assert!(!preview_enabled(), "no consumer, no preview");
        set_preview_target_size(8, 8);
        publish_bgra(&source, 4, 2);
        assert_eq!(
            RELEASED.load(Ordering::Relaxed),
            2,
            "nothing is produced once the texture is gone"
        );
    }
}
