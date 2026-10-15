//! The `AImageReader` and `AImage` entry points, shared by every reader output.
//!
//! Two outputs of a capture session read images rather than surfaces: the still path's JPEG
//! reader and the preview's YUV reader. Both hand the camera a window and both copy a plane out
//! of what it wrote, so the declarations live here once and the paths above them differ only in
//! the format they ask for and what they do with the bytes.
//!
//! `# Safety`
//!
//! Every function below is only sound against objects that are live and owned by the caller:
//! a reader from [`AImageReader_new`] until its single [`AImageReader_delete`], and an image
//! from [`AImageReader_acquireNextImage`] until its single [`AImage_delete`]. The bytes a plane
//! points at are valid only while its image is held.

use std::ffi::c_int;

use super::native_window::ANativeWindow;

/// Opaque `AImageReader` (`media/NdkImageReader.h`).
#[repr(C)]
pub(crate) struct AImageReader {
    _private: [u8; 0],
}

/// Opaque `AImage` (`media/NdkImage.h`).
#[repr(C)]
pub(crate) struct AImage {
    _private: [u8; 0],
}

// The reader and the image it hands out live in `libmediandk`, which `media.rs` already links.
#[link(name = "mediandk")]
unsafe extern "C" {
    pub(crate) fn AImageReader_new(
        width: i32,
        height: i32,
        format: i32,
        max_images: i32,
        reader: *mut *mut AImageReader,
    ) -> c_int;
    pub(crate) fn AImageReader_delete(reader: *mut AImageReader);
    pub(crate) fn AImageReader_getWindow(
        reader: *mut AImageReader,
        window: *mut *mut ANativeWindow,
    ) -> c_int;
    pub(crate) fn AImageReader_acquireNextImage(
        reader: *mut AImageReader,
        image: *mut *mut AImage,
    ) -> c_int;
    pub(crate) fn AImage_delete(image: *mut AImage);
    pub(crate) fn AImage_getPlaneData(
        image: *const AImage,
        plane_index: c_int,
        data: *mut *mut u8,
        data_length: *mut c_int,
    ) -> c_int;
    pub(crate) fn AImage_getPlaneRowStride(
        image: *const AImage,
        plane_index: c_int,
        row_stride: *mut c_int,
    ) -> c_int;
    pub(crate) fn AImage_getPlanePixelStride(
        image: *const AImage,
        plane_index: c_int,
        pixel_stride: *mut c_int,
    ) -> c_int;
    pub(crate) fn AImage_getWidth(image: *const AImage, width: *mut c_int) -> c_int;
    pub(crate) fn AImage_getHeight(image: *const AImage, height: *mut c_int) -> c_int;
}
