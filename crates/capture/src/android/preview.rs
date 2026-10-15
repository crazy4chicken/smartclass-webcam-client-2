//! The live preview's image source: a `YUV_420_888` reader, its window riding in the capture
//! session beside the encoder surface, and the conversion that turns its planes into the RGBA
//! the core's preview path carries.
//!
//! # Why a third output
//!
//! The encoder's input surface cannot be read back: from the moment the camera writes into it,
//! the buffers live in the codec's own queue and nothing this backend holds can map them. The
//! JPEG reader the still path uses is equally unreadable as a picture — its plane is a
//! compressed frame, and decoding one per preview frame to throw it away is worse than the
//! conversion done here. So the preview gets its own output: the camera renders into it with
//! every capture request, and the reader hands the planes over as a plain image.
//!
//! # Reading without waiting
//!
//! A preview frame that is late is worthless — the next capture request is 16 ms away — so the
//! reader never blocks and never queues. One frame is acquired per call and deleted right after
//! the conversion; when the camera has not finished a frame yet, the call reports nothing and the
//! capture thread moves on. Frames the UI never consumed are dropped by the core, not here.
//!
//! # One pass, at the size the interface asked for
//!
//! The frame is converted exactly once: the camera's YUV planes are read where they lie — the
//! image is alive for the whole of the conversion, so nothing is copied out of it — and written
//! straight into RGBA at the size the consumer's box fits, turned by the sensor's orientation on
//! the way. The size is fitted for the picture *as it will be shown*: a quarter turn swaps the
//! source's sides before the fit, so the result is already the final frame and the core hands it
//! on without a second pass over any pixel.

use std::ptr;
use std::slice;

use tracing::{debug, warn};

use super::image_reader::{
    AImage, AImageReader, AImageReader_acquireNextImage, AImageReader_delete,
    AImageReader_getWindow, AImageReader_new, AImage_delete, AImage_getHeight,
    AImage_getPlaneData, AImage_getPlanePixelStride, AImage_getPlaneRowStride, AImage_getWidth,
};
use super::native_window::ANativeWindow;
use super::still::{AMEDIA_ERROR_INVALID_PARAMETER, AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE, AMEDIA_OK, status_error};
use crate::error::CaptureError;

/// `AIMAGE_FORMAT_YUV_420_888`: the flexible YUV format every camera can produce.
pub(crate) const IMAGE_FORMAT_YUV_420_888: i32 = 0x23;

/// How many frames the reader may hold at once.
///
/// Two, like the still reader: one the camera is filling and one waiting to be read. More would
/// only queue pictures nobody asked to keep.
const MAX_IMAGES: i32 = 2;

/// The widest the preview reader is created at, whatever the stream's resolution is.
///
/// The preview is drawn in a box a few hundred pixels wide, and the core scales the frame to it
/// anyway, so asking the camera to render the stream's full size into a third output buys
/// nothing but camera bandwidth — for a 16:9 stream that is several times the pixels the picture
/// can show, and on a device whose HAL is already refusing awkward output combinations it is the
/// difference between a preview and none.
const MAX_PREVIEW_WIDTH: u32 = 1280;

/// The camera's YUV output as a preview source.
pub(crate) struct PreviewCapture {
    /// The reader the camera writes frames into.
    reader: *mut AImageReader,
    /// The reader's window, borrowed until the reader is deleted.
    window: *mut ANativeWindow,
    /// Reused RGBA output buffer, so a preview per frame does not allocate one.
    rgba: Vec<u8>,
    /// Whether a conversion has ever produced a frame, for the one-time log.
    reported: bool,
    /// Clockwise degrees to turn the frame by before it is shown.
    ///
    /// The camera hands over frames in its sensor's own orientation, and a device whose sensor
    /// is mounted sideways delivers a picture that is turned against the interface — the phone
    /// this backend was developed on reports 90. Nothing upstream rotates a frame: the protocol
    /// carries no orientation field, and the macOS shim documents the rotation as the app
    /// layer's job. On Android that layer is this backend, and a preview is the one consumer
    /// that can be turned without touching what the server is sent.
    rotation_degrees: u16,
}

impl PreviewCapture {
    /// Creates a YUV image reader for `width`x`height`.
    ///
    /// The reader is created at the preview's own size rather than the stream's — see
    /// [`MAX_PREVIEW_WIDTH`] — so the camera renders only the pixels the picture can show.
    /// `rotation_degrees` is the sensor's orientation, applied when a frame is converted.
    pub(crate) fn new(
        width: u32,
        height: u32,
        rotation_degrees: u16,
    ) -> Result<Self, CaptureError> {
        let (width, height) = preview_size(width, height);
        let width = i32::try_from(width).map_err(|_| CaptureError::UnsupportedParameters)?;
        let height = i32::try_from(height).map_err(|_| CaptureError::UnsupportedParameters)?;

        let mut reader: *mut AImageReader = ptr::null_mut();
        // SAFETY: the out pointer is a live local; the format and image count are values the
        // header accepts; on a non-OK status the pointer is left untouched and nothing is owned.
        let status = unsafe {
            AImageReader_new(
                width,
                height,
                IMAGE_FORMAT_YUV_420_888,
                MAX_IMAGES,
                &mut reader,
            )
        };
        if status != AMEDIA_OK {
            if status == AMEDIA_ERROR_INVALID_PARAMETER {
                return Err(CaptureError::UnsupportedParameters);
            }
            return Err(status_error("AImageReader_new (preview)", status));
        }
        if reader.is_null() {
            return Err(CaptureError::Os(
                "AImageReader_new reported success but returned no preview reader".to_owned(),
            ));
        }

        let mut window: *mut ANativeWindow = ptr::null_mut();
        // SAFETY: the reader was created just above and is live; the out pointer is a local.
        let status = unsafe { AImageReader_getWindow(reader, &mut window) };
        if status != AMEDIA_OK || window.is_null() {
            // SAFETY: this value is the reader's only owner at this point.
            unsafe { AImageReader_delete(reader) };
            if status != AMEDIA_OK {
                return Err(status_error("AImageReader_getWindow (preview)", status));
            }
            return Err(CaptureError::Os(
                "AImageReader_getWindow reported success but returned no preview window".to_owned(),
            ));
        }

        debug!(width, height, "created the YUV image reader for the preview output");
        Ok(Self {
            reader,
            window,
            rgba: Vec::new(),
            reported: false,
            rotation_degrees,
        })
    }

    /// The window the capture session has to include as an output.
    pub(crate) fn window(&self) -> *mut ANativeWindow {
        self.window
    }

    /// Offers the newest frame the camera has written to the core's preview, if any.
    ///
    /// Called once per capture-thread loop: it takes at most one frame, converts it to RGBA at
    /// the size the preview asked for, and publishes it. Nothing is converted while the preview
    /// is off — [`scwc_core::preview::next_frame_size`] answers `None` and the frame is dropped
    /// unread, which is what keeps a preview nobody is watching from costing anything.
    pub(crate) fn publish(&mut self) {
        let mut image = self.acquire();
        if image.is_null() {
            return;
        }
        // Whatever the camera wrote while the last frame was being converted is already older
        // than the picture about to be shown, so it is dropped rather than queued: a preview
        // that plays through a backlog is the lag this loop exists to avoid.
        loop {
            let newer = self.acquire();
            if newer.is_null() {
                break;
            }
            // SAFETY: the image was acquired above and is not used again.
            unsafe { AImage_delete(image) };
            image = newer;
        }

        let converted = self.convert_and_publish(image);

        // SAFETY: the image was acquired above and is deleted exactly once, here; nothing reads
        // the planes after this, and the conversion copies what it needs before returning.
        unsafe { AImage_delete(image) };

        match converted {
            Ok(true) => {
                if !self.reported {
                    self.reported = true;
                    debug!("the camera preview is now producing frames");
                }
            }
            Ok(false) => {}
            Err(error) => warn!(%error, "could not build a preview frame"),
        }
    }

    /// Takes the next frame the camera has written, or null when there is none.
    fn acquire(&self) -> *mut AImage {
        let mut image: *mut AImage = ptr::null_mut();
        // SAFETY: the reader is live and owned by this value; the out pointer is a local. On
        // `AMEDIA_OK` the platform hands over one image the caller then owns.
        let status = unsafe { AImageReader_acquireNextImage(self.reader, &mut image) };
        if status != AMEDIA_OK {
            // The ordinary case by far: the camera has not finished a frame since the last call.
            if status != AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE {
                debug!(status, "the preview reader had no usable frame");
            }
            return ptr::null_mut();
        }
        image
    }

    /// Converts `image` to RGBA at the preview's size and publishes it; `false` when there is
    /// nothing to publish.
    fn convert_and_publish(&mut self, image: *mut AImage) -> Result<bool, CaptureError> {
        let (width, height) = image_size(image)?;
        // The picture is turned before it is fitted, so the fitted size is computed from the
        // shape the operator will see: a sensor mounted sideways reports its frame one way and
        // the interface is the other. Fitting the *shown* shape makes the result the final
        // frame — the core hands a frame of exactly that size on without rescaling it, which is
        // what keeps the preview to one pass over the pixels.
        let (source_width, source_height) = match self.rotation_degrees % 360 {
            90 | 270 => (height, width),
            _ => (width, height),
        };
        let Some((out_width, out_height)) =
            scwc_core::preview::next_frame_size(source_width, source_height)
        else {
            // No preview is being watched, or the consumer is still behind its last frame.
            return Ok(false);
        };

        self.rgba
            .resize(out_width as usize * out_height as usize * 4, 0);
        // SAFETY: the image is alive for the whole of this call — the caller deletes it only
        // after this returns — and the conversion reads every byte it needs from the planes
        // before that, which is why nothing is copied out of them here.
        unsafe {
            let planes = PlaneViews::read(image, width, height)?;
            planes.to_rgba(
                out_width,
                out_height,
                self.rotation_degrees,
                &mut self.rgba,
            )?;
        }
        // The frame is finished RGBA at exactly the size the consumer asked for: the core's
        // only remaining work is handing it over.
        Ok(scwc_core::preview::publish_rgba(
            &self.rgba,
            out_width,
            out_height,
        ))
    }
}

impl Drop for PreviewCapture {
    fn drop(&mut self) {
        if !self.reader.is_null() {
            // SAFETY: this value owns the reader for its whole life, so the delete happens once.
            unsafe { AImageReader_delete(self.reader) };
            self.reader = ptr::null_mut();
        }
        self.window = ptr::null_mut();
    }
}

// SAFETY: the reader is created, used and deleted on the capture thread alone, and the window
// is only handed back to that same thread's session.
unsafe impl Send for PreviewCapture {}

/// The size a preview output is created at: `width`x`height` capped to [`MAX_PREVIEW_WIDTH`].
///
/// Shared by the reader this backend makes and by the interface's own window, which is a preview
/// like any other: both want the camera's shape at no more than the pixels a picture can show.
///
/// Keeps the picture's shape while it shrinks it, so the frame the core scales to the UI box
/// keeps the camera's aspect ratio. Sizes the camera would not accept are rounded down to even
/// numbers, which every YUV output requires.
pub(crate) fn preview_size(width: u32, height: u32) -> (u32, u32) {
    if width <= MAX_PREVIEW_WIDTH {
        return (width & !1, height & !1);
    }
    let height = (height as u64 * MAX_PREVIEW_WIDTH as u64 / width as u64) as u32;
    (MAX_PREVIEW_WIDTH & !1, height & !1)
}

/// The pixel size of an acquired image.
fn image_size(image: *const AImage) -> Result<(u32, u32), CaptureError> {
    let (mut width, mut height) = (0i32, 0i32);
    // SAFETY: the image is live for the caller's duration and both out pointers are locals.
    let status = unsafe { AImage_getWidth(image, &mut width) };
    if status != AMEDIA_OK {
        return Err(status_error("AImage_getWidth", status));
    }
    // SAFETY: as above.
    let status = unsafe { AImage_getHeight(image, &mut height) };
    if status != AMEDIA_OK {
        return Err(status_error("AImage_getHeight", status));
    }
    if width <= 0 || height <= 0 {
        return Err(CaptureError::Os(
            "the preview image reported an empty size".to_owned(),
        ));
    }
    Ok((width as u32, height as u32))
}

/// One plane of a YUV image: the bytes the platform points at, and how they are laid out.
struct Plane<'a> {
    /// The plane's bytes.
    data: &'a [u8],
    /// Bytes from one row to the next.
    row_stride: usize,
    /// Bytes from one sample to the next within a row.
    pixel_stride: usize,
}

impl Plane<'_> {
    fn rows(&self) -> usize {
        if self.row_stride == 0 {
            0
        } else {
            self.data.len().div_ceil(self.row_stride.max(1))
        }
    }

    fn columns(&self) -> usize {
        if self.pixel_stride == 0 {
            0
        } else {
            self.row_stride / self.pixel_stride
        }
    }
}

/// A plane of a YUV image laid out for reading: the plane's bytes with the row and pixel strides
/// already applied.
///
/// The conversion walks a plane once per frame and reads every pixel of it, so the two things a
/// per-sample read must not do are recompute the plane's geometry and bounds-check: the strides
/// are folded into `base`/`step` here, once, and the index is clamped against the plane's own
/// length there. A plane whose geometry does not describe its bytes is refused rather than read
/// out of range.
struct PlaneView<'a> {
    /// The plane's bytes.
    data: &'a [u8],
    /// Index of the sample at the plane's corner.
    base: usize,
    /// Bytes from one sample to the next within a row.
    step: usize,
    /// Bytes from the first sample of one row to the first of the next.
    row_step: usize,
}

impl<'a> PlaneView<'a> {
    /// Borrows a plane for reading, or `None` when its geometry cannot describe its bytes.
    ///
    /// A zero pixel stride would make every sample the same byte, and a row stride narrower than
    /// the row it holds would make successive rows overlap, so the plane is walked with the
    /// platform's own claim only when that claim is coherent; the YUV planes this backend reads
    /// always carry a stride of at least one pixel.
    fn new(plane: &Plane<'a>) -> Option<Self> {
        let height = plane.rows();
        let width = plane.columns();
        if width == 0 || height == 0 || plane.pixel_stride == 0 || plane.row_stride == 0 {
            return None;
        }
        let last = (height - 1) * plane.row_stride + (width - 1) * plane.pixel_stride;
        if last >= plane.data.len() {
            return None;
        }
        Some(Self {
            data: plane.data,
            base: 0,
            step: plane.pixel_stride,
            row_step: plane.row_stride,
        })
    }

    /// The sample at `(x, y)` of the plane.
    ///
    /// The index is clamped into the plane rather than bounds-checked, so a caller that walks
    /// with the plane's own strides cannot panic a capture thread on a frame the platform
    /// described oddly; a read is then a single index into the slice.
    #[inline]
    fn at(&self, x: usize, y: usize) -> u8 {
        let index = self.base + y * self.row_step + x * self.step;
        self.data.get(index).copied().unwrap_or(0)
    }
}

/// The three planes of a live YUV image, borrowed for one conversion.
///
/// The camera's buffers are only valid while the image is held, and the conversion runs while
/// it is — the caller deletes the image after the frame is built — so the planes are borrowed
/// where they lie instead of being copied out. A per-frame preview therefore allocates nothing
/// on the way in.
struct PlaneViews<'a> {
    /// Full-resolution luma, one byte per pixel.
    y: PlaneView<'a>,
    /// Quarter-resolution chroma.
    u: PlaneView<'a>,
    /// Quarter-resolution chroma.
    v: PlaneView<'a>,
    /// The image's pixel size.
    width: u32,
    /// The image's pixel size.
    height: u32,
}

impl<'a> PlaneViews<'a> {
    /// Borrows the three planes of a live image.
    ///
    /// # Safety
    ///
    /// `image` must be live and the caller must keep it alive for as long as the returned views
    /// are read: the planes point into the image's own buffers.
    unsafe fn read(image: *const AImage, width: u32, height: u32) -> Result<Self, CaptureError> {
        /// Borrows one plane of a live image.
        ///
        /// # Safety
        ///
        /// As [`PlaneViews::read`]: the image must outlive the returned view.
        unsafe fn plane<'a>(image: *const AImage, index: i32) -> Result<PlaneView<'a>, CaptureError> {
            let mut data: *mut u8 = ptr::null_mut();
            let mut length: i32 = 0;
            // SAFETY: the image is live for the caller's duration and both out pointers are locals.
            let status = unsafe { AImage_getPlaneData(image, index, &mut data, &mut length) };
            if status != AMEDIA_OK {
                return Err(status_error("AImage_getPlaneData (preview)", status));
            }
            if data.is_null() || length <= 0 {
                return Err(CaptureError::Os(format!(
                    "the preview image's plane {index} carried no readable data"
                )));
            }
            let mut row_stride: i32 = 0;
            // SAFETY: as above.
            let status = unsafe { AImage_getPlaneRowStride(image, index, &mut row_stride) };
            if status != AMEDIA_OK {
                return Err(status_error("AImage_getPlaneRowStride (preview)", status));
            }
            let mut pixel_stride: i32 = 0;
            // SAFETY: as above.
            let status = unsafe { AImage_getPlanePixelStride(image, index, &mut pixel_stride) };
            if status != AMEDIA_OK {
                return Err(status_error("AImage_getPlanePixelStride (preview)", status));
            }
            // SAFETY: the platform filled `data` with a pointer into the image's plane and
            // `length` with its valid length in bytes; the image is alive for the whole of the
            // borrow, which is what makes the range readable here.
            let bytes = unsafe { slice::from_raw_parts(data, length as usize) };
            let geometry = Plane {
                data: bytes,
                row_stride: row_stride.max(0) as usize,
                pixel_stride: pixel_stride.max(0) as usize,
            };
            PlaneView::new(&geometry).ok_or_else(|| {
                CaptureError::Os(format!(
                    "the preview image's plane {index} has a geometry that cannot describe its bytes"
                ))
            })
        }

        Ok(Self {
            // SAFETY: the image is live and owned by the caller for the whole borrow.
            y: unsafe { plane(image, 0)? },
            u: unsafe { plane(image, 1)? },
            v: unsafe { plane(image, 2)? },
            width,
            height,
        })
    }

    /// Converts to tightly packed RGBA at `out_width`x`out_height`, rotated by `rotation`.
    ///
    /// This is the preview's only pass over the pixels: the colour conversion, the scale and the
    /// turn happen together, and the channels are written in the order the consumer wants, so
    /// neither an intermediate BGRA frame nor a second resample exists. Chroma is sampled at a
    /// quarter resolution, so the subsampling is done by index rather than by averaging — the
    /// picture the operator gets is what the camera produced, at the size it was asked for.
    ///
    /// `rotation` is the clockwise turn the sensor needs (0, 90, 180 or 270); the source is
    /// walked in the destination's order, so one pass both turns and scales and no intermediate
    /// frame is allocated.
    fn to_rgba(
        &self,
        out_width: u32,
        out_height: u32,
        rotation: u16,
        out: &mut [u8],
    ) -> Result<(), CaptureError> {
        if self.width == 0 || self.height == 0 || out_width == 0 || out_height == 0 {
            return Err(CaptureError::UnsupportedParameters);
        }

        let (out_width, out_height) = (out_width as usize, out_height as usize);
        let (source_width, source_height) = (self.width as usize, self.height as usize);
        let quarter = rotation % 360;

        // Every source coordinate is a ratio of a destination coordinate, and the same ratio
        // serves a whole row or column, so the maps are built once per frame instead of being
        // recomputed per pixel: the inner loop then only indexes them. Two maps per axis are
        // enough, because each of the four turns takes its source coordinate from one of them.
        let source_x_of_row: Vec<usize> = (0..out_height)
            .map(|row| row * source_width / out_height)
            .collect();
        let source_y_of_row: Vec<usize> = (0..out_height)
            .map(|row| row * source_height / out_height)
            .collect();
        let source_x_of_column: Vec<usize> = (0..out_width)
            .map(|column| column * source_width / out_width)
            .collect();
        let source_y_of_column: Vec<usize> = (0..out_width)
            .map(|column| column * source_height / out_width)
            .collect();

        for row in 0..out_height {
            // The source row this destination row reads, per turn. A 90/270 turn takes its
            // source row from the destination column and vice versa, which is the whole of what
            // a turn does to the address.
            let base = row * out_width * 4;
            let row_x = source_x_of_row[row];
            let row_y = source_y_of_row[row];
            let row_y_flipped = source_y_of_row[out_height - 1 - row];
            let row_x_flipped = source_x_of_row[out_height - 1 - row];
            for column in 0..out_width {
                let (source_x, source_y) = match quarter {
                    90 => (row_x, source_y_of_column[out_width - 1 - column]),
                    180 => (
                        source_x_of_column[out_width - 1 - column],
                        row_y_flipped,
                    ),
                    270 => (row_x_flipped, source_y_of_column[column]),
                    // 0, and anything malformed, which is drawn as it came.
                    _ => (source_x_of_column[column], row_y),
                };
                // Chroma planes are half the size in both directions.
                let chroma_x = source_x / 2;
                let chroma_y = source_y / 2;

                let luma = i32::from(self.y.at(source_x, source_y));
                let cb = i32::from(self.u.at(chroma_x, chroma_y)) - 128;
                let cr = i32::from(self.v.at(chroma_x, chroma_y)) - 128;

                let pixel = base + column * 4;
                out[pixel] = clamp_bg((luma << SHIFT) + R_CR * cr);
                out[pixel + 1] = clamp_bg((luma << SHIFT) - G_CB * cb - G_CR * cr);
                out[pixel + 2] = clamp_bg((luma << SHIFT) + B_CB * cb);
                out[pixel + 3] = u8::MAX;
            }
        }
        Ok(())
    }
}

/// The BT.601 full-range conversion, which is what `YUV_420_888` carries, in fixed point so the
/// inner loop is integer arithmetic. The factors are 1.402, 0.344136, 0.714136 and 1.772, scaled
/// by 2^[`SHIFT`] and rounded to the nearest integer.
const R_CR: i32 = 91_880;
/// Green's chroma-blue factor.
const G_CB: i32 = 22_554;
/// Green's chroma-red factor.
const G_CR: i32 = 46_803;
/// Blue's chroma-blue factor.
const B_CB: i32 = 116_131;
/// Bit shift the colour components are scaled by.
const SHIFT: u32 = 16;
/// Rounding bias for [`SHIFT`].
const HALF: i32 = 1 << (SHIFT - 1);

/// Rounds one fixed-point colour component to a byte.
#[inline]
fn clamp_bg(scaled: i32) -> u8 {
    (((scaled + HALF) >> SHIFT).clamp(0, 255)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Borrows the three planes of a frame that is already in memory, the way
    /// [`PlaneViews::read`] borrows them out of a live image.
    fn views(width: u32, height: u32, luma: impl Fn(u32, u32) -> u8) -> PlaneViews<'static> {
        let mut y = Vec::with_capacity((width * height) as usize);
        for row in 0..height {
            for column in 0..width {
                y.push(luma(column, row));
            }
        }
        // The Y plane is walked with the source's own strides, and building it row-major with
        // a stride of one pixel per sample is what `AImage_getPlaneRowStride` reports for the
        // planes this backend reads on the devices it was developed on.
        let u = vec![128u8; (width * height / 4) as usize];
        let v = vec![128u8; (width * height / 4) as usize];
        let y: &'static [u8] = Box::leak(y.into_boxed_slice());
        let u: &'static [u8] = Box::leak(u.into_boxed_slice());
        let v: &'static [u8] = Box::leak(v.into_boxed_slice());
        PlaneViews {
            y: PlaneView::new(&Plane {
                data: y,
                row_stride: width as usize,
                pixel_stride: 1,
            })
            .expect("y plane"),
            u: PlaneView::new(&Plane {
                data: u,
                row_stride: (width / 2) as usize,
                pixel_stride: 1,
            })
            .expect("u plane"),
            v: PlaneView::new(&Plane {
                data: v,
                row_stride: (width / 2) as usize,
                pixel_stride: 1,
            })
            .expect("v plane"),
            width,
            height,
        }
    }

    fn convert(width: u32, height: u32, rotation: u16) -> (Vec<u8>, u32, u32) {
        // A marker in the top-left and a different one in the top-right, so a turn is visible
        // in the output regardless of scaling.
        let source = views(width, height, |x, y| {
            if y < height / 2 && x < width / 2 {
                255
            } else if y < height / 2 {
                64
            } else if x < width / 2 {
                32
            } else {
                0
            }
        });
        let mut out = vec![0u8; (width * height * 4) as usize];
        source
            .to_rgba(width, height, rotation, &mut out)
            .expect("conversion succeeds");
        (out, width, height)
    }

    /// The red channel of one output pixel: with neutral chroma the red of a luma `l` is `l`,
    /// so the marker values come through unchanged.
    fn red_at(out: &[u8], width: u32, x: u32, y: u32) -> u8 {
        out[((y * width + x) * 4) as usize]
    }

    #[test]
    fn a_zero_rotation_leaves_the_frame_as_it_came() {
        let (out, width, _) = convert(8, 8, 0);
        assert_eq!(red_at(&out, width, 1, 1), 255, "top-left stays top-left");
        assert_eq!(red_at(&out, width, 6, 1), 64, "top-right stays top-right");
        assert_eq!(red_at(&out, width, 1, 6), 32);
        assert_eq!(red_at(&out, width, 6, 6), 0);
    }

    #[test]
    fn a_quarter_turn_moves_the_top_left_corner_to_the_top_right() {
        // Turning clockwise: the source's top-left lands at the destination's top-right.
        let (out, width, _) = convert(8, 8, 90);
        assert_eq!(red_at(&out, width, 6, 1), 255, "source top-left → top-right");
        assert_eq!(red_at(&out, width, 6, 6), 64, "source top-right → bottom-right");
        assert_eq!(red_at(&out, width, 1, 1), 32, "source bottom-left → top-left");
        assert_eq!(red_at(&out, width, 1, 6), 0);
    }

    #[test]
    fn a_half_turn_moves_the_corner_diagonally() {
        let (out, width, _) = convert(8, 8, 180);
        assert_eq!(red_at(&out, width, 6, 6), 255, "top-left → bottom-right");
        assert_eq!(red_at(&out, width, 1, 6), 64);
        assert_eq!(red_at(&out, width, 6, 1), 32);
        assert_eq!(red_at(&out, width, 1, 1), 0);
    }

    #[test]
    fn a_three_quarter_turn_moves_the_top_left_corner_to_the_bottom_left() {
        let (out, width, _) = convert(8, 8, 270);
        assert_eq!(red_at(&out, width, 1, 6), 255, "source top-left → bottom-left");
        assert_eq!(red_at(&out, width, 1, 1), 64);
        assert_eq!(red_at(&out, width, 6, 6), 32);
        assert_eq!(red_at(&out, width, 6, 1), 0);
    }

    #[test]
    fn a_quarter_turn_swaps_the_side_lengths() {
        // A wide source turned a quarter stands tall: the destination is the source's shape
        // transposed, which is what keeps a landscape sensor's picture filling a portrait box.
        let source = views(16, 4, |_, _| 128);
        let mut out = vec![0u8; 16 * 4 * 4];
        // 4 wide by 16 tall, i.e. the source's sides exchanged at the same pixel count.
        source
            .to_rgba(4, 16, 90, &mut out)
            .expect("conversion succeeds");
        assert!(!out.is_empty());
    }

    #[test]
    fn alpha_is_opaque_and_chroma_of_128_is_a_pure_luma_pass() {
        let source = views(4, 4, |x, y| (x + y) as u8 * 30);
        let mut out = vec![0u8; 4 * 4 * 4];
        source.to_rgba(4, 4, 0, &mut out).expect("conversion succeeds");
        for pixel in 0..16 {
            let y = (pixel / 4 + pixel % 4) as u8 * 30;
            assert_eq!(&out[pixel * 4..pixel * 4 + 3], &[y, y, y]);
            assert_eq!(out[pixel * 4 + 3], u8::MAX, "a camera picture has no transparency");
        }
    }

    #[test]
    fn the_reader_is_capped_at_the_preview_width() {
        assert_eq!(preview_size(1920, 1620), (1280, 1080), "scaled, shape kept, even");
        assert_eq!(preview_size(640, 480), (640, 480), "already small enough");
        assert_eq!(preview_size(1921, 1081), (1280, 720), "odd input, even output");
    }
}
