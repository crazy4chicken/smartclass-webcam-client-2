//! Hand-declared bindings for the Camera2 NDK (`libcamera2ndk.so`).
//!
//! The crate talks to the platform through hand-written FFI rather than a binding
//! generator, for the same reason the Linux backend declares the kernel ABI by hand:
//! `cargo check` has to work on a host without the NDK, its headers or a C toolchain, and
//! the workspace's own `cargo check` runs on Windows, macOS and Linux alike.
//!
//! Every declaration below is transcribed from the NDK r28 headers
//! (`camera/NdkCameraManager.h`, `camera/NdkCameraDevice.h`, `camera/NdkCameraMetadata.h`,
//! `camera/NdkCameraMetadataTags.h`, `camera/NdkCameraError.h`). The metadata tags deserve
//! a note: the headers define them as expressions over a section base
//! (`ACAMERA_SCALER_START + 10`, with `ACAMERA_SCALER_START = ACAMERA_SCALER << 16`), which
//! the C compiler folds for us. Rust has to state the folded value, so each tag constant
//! below spells out the section number and the offset it came from, and the section order
//! it depends on is `acamera_metadata_section` in `NdkCameraMetadataTags.h`.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;
use std::slice;

use tracing::{debug, error};

use crate::error::CaptureError;

// ---------------------------------------------------------------------------------------
// Opaque handle types.
//
// None of these is ever dereferenced by Rust: the NDK owns the storage and hands out
// pointers, so the types exist only to make those pointers distinct from each other.
// ---------------------------------------------------------------------------------------

/// Opaque `ACameraManager`.
#[repr(C)]
pub(crate) struct ACameraManager {
    _private: [u8; 0],
}

/// Opaque `ACameraDevice`: one opened camera.
#[repr(C)]
pub(crate) struct ACameraDevice {
    _private: [u8; 0],
}

/// Opaque `ACameraMetadata`: a camera's characteristics, or a capture request.
#[repr(C)]
pub(crate) struct ACameraMetadata {
    _private: [u8; 0],
}

/// `ACameraIdList`: the camera ids the service reports.
#[repr(C)]
pub(crate) struct ACameraIdList {
    /// Number of ids in `camera_ids`.
    num_cameras: c_int,
    /// Pointer to `num_cameras` NUL-terminated ids, owned by the list.
    camera_ids: *const *const c_char,
}

// ---------------------------------------------------------------------------------------
// Device state callbacks.
// ---------------------------------------------------------------------------------------

/// `ACameraDevice_StateCallback`: the device became unavailable.
///
/// A newer client of higher priority took the camera, or it was unplugged.
pub(crate) type DeviceStateCallback =
    unsafe extern "C" fn(context: *mut c_void, device: *mut ACameraDevice);

/// `ACameraDevice_ErrorStateCallback`: the device hit an unrecoverable error.
pub(crate) type DeviceErrorStateCallback =
    unsafe extern "C" fn(context: *mut c_void, device: *mut ACameraDevice, error: c_int);

/// `ACameraDevice_StateCallbacks`.
///
/// The field order is the header's (`context`, `onDisconnected`, `onError`) — it is a
/// `#[repr(C)]` struct the framework reads, so a reordering here would be a silent memory
/// corruption rather than a compile error.
#[repr(C)]
pub(crate) struct ACameraDevice_StateCallbacks {
    /// Passed back to both callbacks; the backend stores its own state pointer here.
    pub(crate) context: *mut c_void,
    /// Invoked when the device is taken away or disconnected.
    pub(crate) on_disconnected: Option<DeviceStateCallback>,
    /// Invoked when the device fails beyond recovery.
    pub(crate) on_error: Option<DeviceErrorStateCallback>,
}

// ---------------------------------------------------------------------------------------
// Metadata.
// ---------------------------------------------------------------------------------------

/// `ACameraMetadata_rational`: a 32-bit fraction.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ACameraMetadata_rational {
    /// Dividend.
    pub(crate) numerator: i32,
    /// Divisor.
    pub(crate) denominator: i32,
}

/// The `data` union of a metadata entry.
///
/// Which member is valid is decided by [`ConstEntry::type_code`]; reading the wrong one is
/// undefined behaviour, so the union stays private to this module and only the checked
/// accessors below hand out slices.
#[repr(C)]
union EntryData {
    u8_: *const u8,
    i32_: *const i32,
    f: *const f32,
    i64_: *const i64,
    d: *const f64,
    r: *const ACameraMetadata_rational,
}

/// `ACameraMetadata_const_entry`: one read-only metadata value.
#[repr(C)]
pub(crate) struct ConstEntry {
    /// Tag this entry was requested for.
    tag: u32,
    /// One of the `METADATA_TYPE_*` codes; decides which union member is valid.
    type_code: u8,
    /// Number of *elements* (not bytes) in the entry.
    count: u32,
    /// The value, whose member depends on `type_code`.
    data: EntryData,
}

/// `ACAMERA_TYPE_BYTE`.
const METADATA_TYPE_BYTE: u8 = 0;
/// `ACAMERA_TYPE_INT32`.
const METADATA_TYPE_INT32: u8 = 1;
/// `ACAMERA_TYPE_INT64`.
const METADATA_TYPE_INT64: u8 = 3;

impl ConstEntry {
    /// The bytes of a `byte[]` entry, or `None` for any other type.
    pub(crate) fn as_u8(&self) -> Option<&[u8]> {
        if self.type_code != METADATA_TYPE_BYTE {
            return None;
        }
        // SAFETY: the type code says this is the initialized member of the union. The
        // pointer is read out first, because the borrow of a union field through `&self`
        // would tie the slice to this borrow for no reason.
        let pointer = unsafe { self.data.u8_ };
        if pointer.is_null() {
            return None;
        }
        // SAFETY: the framework guarantees `count` elements behind the pointer, valid for
        // as long as the metadata this entry was read from.
        Some(unsafe { slice::from_raw_parts(pointer, self.count as usize) })
    }

    /// The elements of an `int32[]` entry, or `None` for any other type.
    pub(crate) fn as_i32(&self) -> Option<&[i32]> {
        if self.type_code != METADATA_TYPE_INT32 {
            return None;
        }
        // SAFETY: as in `as_u8`, for the `int32[]` member.
        let pointer = unsafe { self.data.i32_ };
        if pointer.is_null() {
            return None;
        }
        // SAFETY: as in `as_u8`.
        Some(unsafe { slice::from_raw_parts(pointer, self.count as usize) })
    }

    /// The elements of an `int64[]` entry, or `None` for any other type.
    pub(crate) fn as_i64(&self) -> Option<&[i64]> {
        if self.type_code != METADATA_TYPE_INT64 {
            return None;
        }
        // SAFETY: as in `as_u8`, for the `int64[]` member.
        let pointer = unsafe { self.data.i64_ };
        if pointer.is_null() {
            return None;
        }
        // SAFETY: as in `as_u8`.
        Some(unsafe { slice::from_raw_parts(pointer, self.count as usize) })
    }
}

// ---------------------------------------------------------------------------------------
// Metadata tags.
//
// Sections are numbered by `acamera_metadata_section` in the tags header; a tag is
// `section << 16 | offset`, with the offset counting up from the section's base.
// ---------------------------------------------------------------------------------------

/// `ACAMERA_CONTROL` section number.
const SECTION_CONTROL: u32 = 1;
/// `ACAMERA_LENS` section number.
const SECTION_LENS: u32 = 8;
/// `ACAMERA_SCALER` section number.
const SECTION_SCALER: u32 = 13;
/// `ACAMERA_SENSOR` section number.
const SECTION_SENSOR: u32 = 14;
/// `ACAMERA_INFO` section number.
const SECTION_INFO: u32 = 21;

/// `ACAMERA_LENS_FACING` — `byte`, one of the [`LensFacing`] values.
pub(crate) const TAG_LENS_FACING: u32 = (SECTION_LENS << 16) + 5;
/// `ACAMERA_SENSOR_ORIENTATION` — `int32`, degrees clockwise to the device's natural
/// orientation.
pub(crate) const TAG_SENSOR_ORIENTATION: u32 = (SECTION_SENSOR << 16) + 14;
/// `ACAMERA_INFO_SUPPORTED_HARDWARE_LEVEL` — `byte`, one of the [`HardwareLevel`] values.
pub(crate) const TAG_INFO_SUPPORTED_HARDWARE_LEVEL: u32 = SECTION_INFO << 16;
/// `ACAMERA_SCALER_AVAILABLE_MIN_FRAME_DURATIONS` — `int64[n*4]`: format, width, height,
/// minimum frame duration in nanoseconds, repeated.
pub(crate) const TAG_SCALER_AVAILABLE_MIN_FRAME_DURATIONS: u32 = (SECTION_SCALER << 16) + 11;
/// `ACAMERA_CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES` — `int32[2*n]`: two frame rates per
/// range, repeated.
pub(crate) const TAG_CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES: u32 = (SECTION_CONTROL << 16) + 20;

// ---------------------------------------------------------------------------------------
// Image formats (`android.graphics.ImageFormat` values the NDK reuses).
// ---------------------------------------------------------------------------------------

/// `AIMAGE_FORMAT_PRIVATE`: opaque buffers a hardware encoder or the display consumes.
pub(crate) const IMAGE_FORMAT_PRIVATE: i32 = 0x22;
/// `AIMAGE_FORMAT_YUV_420_888`: flexible YUV, what a software encoder needs.
pub(crate) const IMAGE_FORMAT_YUV_420_888: i32 = 0x23;

// ---------------------------------------------------------------------------------------
// `camera_status_t` (`NdkCameraError.h`).
// ---------------------------------------------------------------------------------------

/// `ACAMERA_OK`.
pub(crate) const ACAMERA_OK: c_int = 0;
/// `ACAMERA_ERROR_BASE`: the first of the negative codes.
const ACAMERA_ERROR_BASE: c_int = -10_000;
/// `ACAMERA_ERROR_UNKNOWN`.
const ACAMERA_ERROR_UNKNOWN: c_int = ACAMERA_ERROR_BASE;
/// `ACAMERA_ERROR_INVALID_PARAMETER`.
const ACAMERA_ERROR_INVALID_PARAMETER: c_int = ACAMERA_ERROR_BASE - 1;
/// `ACAMERA_ERROR_CAMERA_DISCONNECTED`.
const ACAMERA_ERROR_CAMERA_DISCONNECTED: c_int = ACAMERA_ERROR_BASE - 2;
/// `ACAMERA_ERROR_NOT_ENOUGH_MEMORY`.
const ACAMERA_ERROR_NOT_ENOUGH_MEMORY: c_int = ACAMERA_ERROR_BASE - 3;
/// `ACAMERA_ERROR_METADATA_NOT_FOUND`.
pub(crate) const ACAMERA_ERROR_METADATA_NOT_FOUND: c_int = ACAMERA_ERROR_BASE - 4;
/// `ACAMERA_ERROR_CAMERA_DEVICE`.
const ACAMERA_ERROR_CAMERA_DEVICE: c_int = ACAMERA_ERROR_BASE - 5;
/// `ACAMERA_ERROR_CAMERA_SERVICE`.
const ACAMERA_ERROR_CAMERA_SERVICE: c_int = ACAMERA_ERROR_BASE - 6;
/// `ACAMERA_ERROR_SESSION_CLOSED`.
const ACAMERA_ERROR_SESSION_CLOSED: c_int = ACAMERA_ERROR_BASE - 7;
/// `ACAMERA_ERROR_INVALID_OPERATION`.
const ACAMERA_ERROR_INVALID_OPERATION: c_int = ACAMERA_ERROR_BASE - 8;
/// `ACAMERA_ERROR_STREAM_CONFIGURE_FAIL`.
const ACAMERA_ERROR_STREAM_CONFIGURE_FAIL: c_int = ACAMERA_ERROR_BASE - 9;
/// `ACAMERA_ERROR_CAMERA_IN_USE`.
const ACAMERA_ERROR_CAMERA_IN_USE: c_int = ACAMERA_ERROR_BASE - 10;
/// `ACAMERA_ERROR_MAX_CAMERA_IN_USE`.
const ACAMERA_ERROR_MAX_CAMERA_IN_USE: c_int = ACAMERA_ERROR_BASE - 11;
/// `ACAMERA_ERROR_CAMERA_DISABLED`.
const ACAMERA_ERROR_CAMERA_DISABLED: c_int = ACAMERA_ERROR_BASE - 12;
/// `ACAMERA_ERROR_PERMISSION_DENIED`.
const ACAMERA_ERROR_PERMISSION_DENIED: c_int = ACAMERA_ERROR_BASE - 13;
/// `ACAMERA_ERROR_UNSUPPORTED_OPERATION`.
const ACAMERA_ERROR_UNSUPPORTED_OPERATION: c_int = ACAMERA_ERROR_BASE - 14;

// ---------------------------------------------------------------------------------------
// libcamera2ndk entry points.
// ---------------------------------------------------------------------------------------

#[link(name = "camera2ndk")]
unsafe extern "C" {
    fn ACameraManager_create() -> *mut ACameraManager;
    fn ACameraManager_delete(manager: *mut ACameraManager);
    fn ACameraManager_getCameraIdList(
        manager: *mut ACameraManager,
        camera_id_list: *mut *mut ACameraIdList,
    ) -> c_int;
    fn ACameraManager_deleteCameraIdList(camera_id_list: *mut ACameraIdList);
    fn ACameraManager_getCameraCharacteristics(
        manager: *mut ACameraManager,
        camera_id: *const c_char,
        characteristics: *mut *mut ACameraMetadata,
    ) -> c_int;
    fn ACameraManager_openCamera(
        manager: *mut ACameraManager,
        camera_id: *const c_char,
        callback: *mut ACameraDevice_StateCallbacks,
        device: *mut *mut ACameraDevice,
    ) -> c_int;

    fn ACameraDevice_close(device: *mut ACameraDevice) -> c_int;

    fn ACameraMetadata_getConstEntry(
        metadata: *const ACameraMetadata,
        tag: u32,
        entry: *mut ConstEntry,
    ) -> c_int;
    fn ACameraMetadata_free(metadata: *mut ACameraMetadata);
}

/// The facing of a camera, as `ACAMERA_LENS_FACING` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LensFacing {
    /// Toward the user.
    Front,
    /// Away from the user.
    Back,
    /// A removable or USB camera.
    External,
}

impl LensFacing {
    /// Reads the `byte` the tag carries.
    fn from_byte(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Front),
            1 => Some(Self::Back),
            2 => Some(Self::External),
            _ => None,
        }
    }
}

/// How much of the Camera2 API a camera implements, as
/// `ACAMERA_INFO_SUPPORTED_HARDWARE_LEVEL` reports it.
///
/// The numeric values are the header's enum
/// (`acamera_metadata_enum_android_info_supported_hardware_level`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HardwareLevel {
    /// Only the deprecated camera API is fully supported.
    Limited,
    /// Every Camera2 capability except the highest tier.
    Full,
    /// A camera that predates Camera2 and is emulated through it.
    Legacy,
    /// The full tier: per-frame control and reprocessing.
    Level3,
    /// A removable camera, which may not implement all of `Limited`.
    External,
}

impl HardwareLevel {
    /// Reads the `byte` the tag carries.
    fn from_byte(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Limited),
            1 => Some(Self::Full),
            2 => Some(Self::Legacy),
            3 => Some(Self::Level3),
            4 => Some(Self::External),
            _ => None,
        }
    }
}

/// Turns a `camera_status_t` into a [`CaptureError`], logging the platform's own code.
///
/// Mirrors the Windows backend: the two cases the core reacts to get a coarse variant and
/// no log, everything else names the status so a failure on a device is diagnosable from
/// the client's log alone.
pub(crate) fn status_error(context: &str, status: c_int) -> CaptureError {
    match status {
        ACAMERA_ERROR_PERMISSION_DENIED | ACAMERA_ERROR_CAMERA_DISABLED => {
            // The CAMERA permission was never granted, or a device policy disabled the
            // camera; Android reports both as a refusal to open.
            CaptureError::PermissionDenied
        }
        ACAMERA_ERROR_STREAM_CONFIGURE_FAIL | ACAMERA_ERROR_UNSUPPORTED_OPERATION => {
            CaptureError::UnsupportedParameters
        }
        _ => {
            let name = status_name(status);
            error!(code = status, status = name, "{context}");
            CaptureError::Os(format!("{context}: {name}"))
        }
    }
}

/// The header's name for a status code, for logs.
fn status_name(status: c_int) -> &'static str {
    match status {
        ACAMERA_OK => "ACAMERA_OK",
        ACAMERA_ERROR_UNKNOWN => "ACAMERA_ERROR_UNKNOWN",
        ACAMERA_ERROR_INVALID_PARAMETER => "ACAMERA_ERROR_INVALID_PARAMETER",
        ACAMERA_ERROR_CAMERA_DISCONNECTED => "ACAMERA_ERROR_CAMERA_DISCONNECTED",
        ACAMERA_ERROR_NOT_ENOUGH_MEMORY => "ACAMERA_ERROR_NOT_ENOUGH_MEMORY",
        ACAMERA_ERROR_METADATA_NOT_FOUND => "ACAMERA_ERROR_METADATA_NOT_FOUND",
        ACAMERA_ERROR_CAMERA_DEVICE => "ACAMERA_ERROR_CAMERA_DEVICE",
        ACAMERA_ERROR_CAMERA_SERVICE => "ACAMERA_ERROR_CAMERA_SERVICE",
        ACAMERA_ERROR_SESSION_CLOSED => "ACAMERA_ERROR_SESSION_CLOSED",
        ACAMERA_ERROR_INVALID_OPERATION => "ACAMERA_ERROR_INVALID_OPERATION",
        ACAMERA_ERROR_STREAM_CONFIGURE_FAIL => "ACAMERA_ERROR_STREAM_CONFIGURE_FAIL",
        ACAMERA_ERROR_CAMERA_IN_USE => "ACAMERA_ERROR_CAMERA_IN_USE",
        ACAMERA_ERROR_MAX_CAMERA_IN_USE => "ACAMERA_ERROR_MAX_CAMERA_IN_USE",
        ACAMERA_ERROR_CAMERA_DISABLED => "ACAMERA_ERROR_CAMERA_DISABLED",
        ACAMERA_ERROR_PERMISSION_DENIED => "ACAMERA_ERROR_PERMISSION_DENIED",
        ACAMERA_ERROR_UNSUPPORTED_OPERATION => "ACAMERA_ERROR_UNSUPPORTED_OPERATION",
        _ => "unrecognized camera status",
    }
}

/// One live `ACameraManager`.
///
/// Creating a manager connects to the camera service, which is why the backend makes one
/// and keeps it for the lifetime of the capture it drives.
pub(crate) struct Manager {
    ptr: *mut ACameraManager,
}

impl Manager {
    /// Connects to the camera service.
    pub(crate) fn create() -> Result<Self, CaptureError> {
        // SAFETY: the call takes no arguments and returns an owned pointer, or NULL when
        // the service cannot be reached.
        let ptr = unsafe { ACameraManager_create() };
        if ptr.is_null() {
            return Err(CaptureError::Os(
                "ACameraManager_create returned null".to_owned(),
            ));
        }
        Ok(Self { ptr })
    }

    /// The ids of the cameras the service currently reports.
    ///
    /// This includes cameras another client is using, and, as the header notes, may be a
    /// subset of what the Java API lists: the NDK drops cameras that only the legacy API
    /// can drive.
    pub(crate) fn camera_ids(&self) -> Result<Vec<String>, CaptureError> {
        let mut list: *mut ACameraIdList = ptr::null_mut();
        // SAFETY: the manager is live and the out pointer is a local.
        let status = unsafe { ACameraManager_getCameraIdList(self.ptr, &mut list) };
        if status != ACAMERA_OK {
            return Err(status_error("listing the cameras", status));
        }
        // Owns the allocation from here on, so an early return still frees it.
        let list = CameraIdList(list);

        let ids = match list.0.is_null() {
            true => {
                debug!("the camera service reported no camera list");
                Vec::new()
            }
            // SAFETY: a successful `getCameraIdList` filled the pointer with an owned,
            // initialized list that stays valid until `deleteCameraIdList`.
            false => unsafe {
                let list = &*list.0;
                let count = list.num_cameras.max(0) as usize;
                (0..count)
                    .filter_map(|index| {
                        let id = *list.camera_ids.add(index);
                        (!id.is_null()).then(|| {
                            CStr::from_ptr(id).to_string_lossy().into_owned()
                        })
                    })
                    .collect()
            },
        };

        Ok(ids)
    }

    /// Reads a camera's static characteristics.
    pub(crate) fn characteristics(&self, camera_id: &str) -> Result<Metadata, CaptureError> {
        let id = CString::new(camera_id).map_err(|error| {
            CaptureError::Os(format!("camera id {camera_id:?} is not a C string: {error}"))
        })?;

        let mut metadata: *mut ACameraMetadata = ptr::null_mut();
        // SAFETY: the manager is live, `id` outlives the call and the out pointer is a local.
        let status = unsafe { ACameraManager_getCameraCharacteristics(self.ptr, id.as_ptr(), &mut metadata) };
        if status != ACAMERA_OK {
            return Err(status_error("reading the camera characteristics", status));
        }
        if metadata.is_null() {
            return Err(CaptureError::Os(
                "the camera service returned no characteristics".to_owned(),
            ));
        }

        Ok(Metadata { ptr: metadata })
    }

    /// Opens a camera.
    ///
    /// This is the call Android gates on the `CAMERA` permission: a process that does not
    /// hold it gets [`CaptureError::PermissionDenied`] here rather than a silent failure.
    /// `callbacks` has to outlive the returned device.
    pub(crate) fn open(
        &self,
        camera_id: &str,
        callbacks: &mut ACameraDevice_StateCallbacks,
    ) -> Result<Device, CaptureError> {
        let id = CString::new(camera_id).map_err(|error| {
            CaptureError::Os(format!("camera id {camera_id:?} is not a C string: {error}"))
        })?;

        let mut device: *mut ACameraDevice = ptr::null_mut();
        // SAFETY: the manager is live, `id` outlives the call, `callbacks` is owned by the
        // caller and outlives the device, and the out pointer is a local.
        let status =
            unsafe { ACameraManager_openCamera(self.ptr, id.as_ptr(), callbacks, &mut device) };
        if status != ACAMERA_OK {
            return Err(status_error("opening the camera", status));
        }
        if device.is_null() {
            return Err(CaptureError::Os(
                "the camera service returned no device".to_owned(),
            ));
        }

        Ok(Device { ptr: device })
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `ACameraManager_create` and is not used afterwards.
        unsafe { ACameraManager_delete(self.ptr) };
    }
}

/// Frees a camera id list however the scope is left.
struct CameraIdList(*mut ACameraIdList);

impl Drop for CameraIdList {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `ACameraManager_getCameraIdList` and is not used
        // afterwards. A null list is what the service reports for "no cameras".
        unsafe { ACameraManager_deleteCameraIdList(self.0) };
    }
}

/// One camera's static characteristics.
pub(crate) struct Metadata {
    ptr: *mut ACameraMetadata,
}

impl Metadata {
    /// Reads one metadata entry.
    ///
    /// Returns `None` when the tag is absent, which the NDK reports as an error status
    /// rather than as an empty entry: cameras differ in which optional tags they carry.
    pub(crate) fn entry(&self, tag: u32) -> Option<ConstEntry> {
        let mut entry = ConstEntry {
            tag: 0,
            type_code: 0,
            count: 0,
            data: EntryData { i32_: ptr::null() },
        };
        // SAFETY: the metadata is live, and the out pointer is a local the framework
        // initializes on success. The entry borrows the metadata's storage, which is why
        // the result is only handed out behind `&self`.
        let status = unsafe { ACameraMetadata_getConstEntry(self.ptr, tag, &mut entry) };
        match status {
            ACAMERA_OK => Some(entry),
            ACAMERA_ERROR_METADATA_NOT_FOUND => None,
            _ => {
                debug!(tag, status = %status_name(status), "cannot read this metadata tag");
                None
            }
        }
    }

    /// The facing of the camera, when it reports one.
    pub(crate) fn lens_facing(&self) -> Option<LensFacing> {
        let entry = self.entry(TAG_LENS_FACING)?;
        LensFacing::from_byte(*entry.as_u8()?.first()?)
    }

    /// The sensor orientation in degrees, when the camera reports it.
    pub(crate) fn sensor_orientation(&self) -> Option<i32> {
        self.entry(TAG_SENSOR_ORIENTATION)?.as_i32()?.first().copied()
    }

    /// How much of the Camera2 API this camera implements, when it reports it.
    ///
    /// Worth logging next to the capability matrix: a `LEGACY` camera answers only the old
    /// API's questions, and `LIMITED` is where constrained high-speed capture — the usual
    /// route to 60 fps at a large size — stops being available.
    pub(crate) fn hardware_level(&self) -> Option<HardwareLevel> {
        let entry = self.entry(TAG_INFO_SUPPORTED_HARDWARE_LEVEL)?;
        HardwareLevel::from_byte(*entry.as_u8()?.first()?)
    }
}

impl Drop for Metadata {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `getCameraCharacteristics` and is not used
        // afterwards.
        unsafe { ACameraMetadata_free(self.ptr) };
    }
}

/// One opened camera.
pub(crate) struct Device {
    ptr: *mut ACameraDevice,
}

impl Device {
    /// Closes the device.
    ///
    /// Idempotent, so [`Drop`] and an explicit close can both call it. Closing is what
    /// releases the camera to other clients; the returned status is only logged, because
    /// there is nothing a caller can do about a close that fails.
    pub(crate) fn close(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        // SAFETY: the pointer came from `openCamera` and is not used afterwards.
        let status = unsafe { ACameraDevice_close(self.ptr) };
        if status != ACAMERA_OK {
            debug!(status = %status_name(status), "closing the camera reported a failure");
        }
        self.ptr = ptr::null_mut();
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.close();
    }
}

/// The raw device pointer, for the session and request calls that take one.
impl Device {
    /// The `ACameraDevice*` this handle owns, or `None` once it has been closed.
    pub(crate) fn as_ptr(&self) -> Option<*mut ACameraDevice> {
        (!self.ptr.is_null()).then_some(self.ptr)
    }
}

// ---------------------------------------------------------------------------------------
// Capture session, request and output objects.
//
// A session is a set of outputs plus the repeating request that fills them. The objects below
// form a chain the framework keeps references into — container → output → surface, and
// request → output target → surface — so they have to be freed in the reverse order they were
// created, which is what [`super::session`] does.
// ---------------------------------------------------------------------------------------

/// `ACAMERA_CONTROL_AE_TARGET_FPS_RANGE` — `int32[2]`.
///
/// The request tag that pins auto-exposure to one frame rate. The client sets it to the
/// configured rate so the camera delivers the steady cadence the protocol's segment durations
/// assume, instead of letting auto-exposure pick its own.
pub(crate) const TAG_CONTROL_AE_TARGET_FPS_RANGE: u32 = (SECTION_CONTROL << 16) + 5;

/// Opaque `ACaptureSessionOutputContainer`.
#[repr(C)]
pub(crate) struct ACaptureSessionOutputContainer {
    _private: [u8; 0],
}

/// Opaque `ACaptureSessionOutput`: one surface a session may write to.
#[repr(C)]
pub(crate) struct ACaptureSessionOutput {
    _private: [u8; 0],
}

/// Opaque `ACameraOutputTarget`: one surface a request writes to.
#[repr(C)]
pub(crate) struct ACameraOutputTarget {
    _private: [u8; 0],
}

/// Opaque `ACaptureRequest`.
#[repr(C)]
pub(crate) struct ACaptureRequest {
    _private: [u8; 0],
}

/// Opaque `ACameraCaptureSession`.
#[repr(C)]
pub(crate) struct ACameraCaptureSession {
    _private: [u8; 0],
}

/// `ACameraDevice_request_template`: which of the platform's request presets to build.
pub(crate) type RequestTemplate = c_int;

/// `TEMPLATE_RECORD`: the preset for video recording — a stable frame rate and
/// recording-grade post-processing, which is what a device client wants.
pub(crate) const TEMPLATE_RECORD: RequestTemplate = 3;

/// `TEMPLATE_STILL_CAPTURE`: the preset for one high-quality still.
///
/// Used for the photo channel's one-shot request: it asks the camera to settle exposure for
/// the still, which is exactly the difference between this and the recording template the
/// repeating request carries.
pub(crate) const TEMPLATE_STILL_CAPTURE: RequestTemplate = 2;

/// `ACameraCaptureSession_stateCallback`.
pub(crate) type SessionStateCallback =
    unsafe extern "C" fn(context: *mut c_void, session: *mut ACameraCaptureSession);

/// `ACameraCaptureSession_stateCallbacks`.
///
/// Field order is the header's (`context`, `onClosed`, `onReady`, `onActive`).
#[repr(C)]
pub(crate) struct ACameraCaptureSession_stateCallbacks {
    /// Passed back to every callback; the session stores its own state pointer here.
    pub(crate) context: *mut c_void,
    /// The session is gone and touching it would crash; no further callbacks follow.
    pub(crate) on_closed: Option<SessionStateCallback>,
    /// The session has no more work queued.
    pub(crate) on_ready: Option<SessionStateCallback>,
    /// The session started processing requests.
    pub(crate) on_active: Option<SessionStateCallback>,
}

#[link(name = "camera2ndk")]
unsafe extern "C" {
    fn ACaptureSessionOutputContainer_create(
        container: *mut *mut ACaptureSessionOutputContainer,
    ) -> c_int;
    fn ACaptureSessionOutputContainer_add(
        container: *mut ACaptureSessionOutputContainer,
        output: *const ACaptureSessionOutput,
    ) -> c_int;
    fn ACaptureSessionOutputContainer_free(container: *mut ACaptureSessionOutputContainer);

    fn ACaptureSessionOutput_create(
        window: *mut super::native_window::ANativeWindow,
        output: *mut *mut ACaptureSessionOutput,
    ) -> c_int;
    fn ACaptureSessionOutput_free(output: *mut ACaptureSessionOutput);

    fn ACameraOutputTarget_create(
        window: *mut super::native_window::ANativeWindow,
        target: *mut *mut ACameraOutputTarget,
    ) -> c_int;
    fn ACameraOutputTarget_free(target: *mut ACameraOutputTarget);

    fn ACameraDevice_createCaptureSession(
        device: *mut ACameraDevice,
        outputs: *const ACaptureSessionOutputContainer,
        callbacks: *const ACameraCaptureSession_stateCallbacks,
        session: *mut *mut ACameraCaptureSession,
    ) -> c_int;
    fn ACameraDevice_createCaptureRequest(
        device: *const ACameraDevice,
        template: RequestTemplate,
        request: *mut *mut ACaptureRequest,
    ) -> c_int;

    fn ACaptureRequest_addTarget(
        request: *mut ACaptureRequest,
        target: *const ACameraOutputTarget,
    ) -> c_int;
    fn ACaptureRequest_setEntry_i32(
        request: *mut ACaptureRequest,
        tag: u32,
        count: u32,
        data: *const i32,
    ) -> c_int;
    fn ACaptureRequest_free(request: *mut ACaptureRequest);

    fn ACameraCaptureSession_setRepeatingRequest(
        session: *mut ACameraCaptureSession,
        callbacks: *mut c_void,
        num_requests: c_int,
        requests: *mut *mut ACaptureRequest,
        sequence_id: *mut c_int,
    ) -> c_int;
    fn ACameraCaptureSession_capture(
        session: *mut ACameraCaptureSession,
        callbacks: *mut c_void,
        num_requests: c_int,
        requests: *mut *mut ACaptureRequest,
        sequence_id: *mut c_int,
    ) -> c_int;
    fn ACameraCaptureSession_stopRepeating(session: *mut ACameraCaptureSession) -> c_int;
    fn ACameraCaptureSession_close(session: *mut ACameraCaptureSession);
}

/// Creates a session output container.
pub(crate) fn output_container_create() -> Result<*mut ACaptureSessionOutputContainer, CaptureError> {
    let mut container: *mut ACaptureSessionOutputContainer = ptr::null_mut();
    // SAFETY: the out pointer is a local the framework initializes on success.
    let status = unsafe { ACaptureSessionOutputContainer_create(&mut container) };
    if status != ACAMERA_OK {
        return Err(status_error("creating the session output container", status));
    }
    Ok(container)
}

/// Adds one surface to a container.
pub(crate) fn output_container_add(
    container: *mut ACaptureSessionOutputContainer,
    output: *const ACaptureSessionOutput,
) -> Result<(), CaptureError> {
    // SAFETY: both pointers are live and owned by the caller.
    let status = unsafe { ACaptureSessionOutputContainer_add(container, output) };
    (status == ACAMERA_OK)
        .then_some(())
        .ok_or_else(|| status_error("adding a surface to the session container", status))
}

/// Frees a container.
///
/// # Safety
///
/// `container` must come from [`output_container_create`] and must not be used afterwards.
pub(crate) unsafe fn output_container_free(container: *mut ACaptureSessionOutputContainer) {
    // SAFETY: the caller guarantees the pointer came from the create call above.
    unsafe { ACaptureSessionOutputContainer_free(container) };
}

/// Creates a session output for one surface.
pub(crate) fn output_create(
    window: *mut super::native_window::ANativeWindow,
) -> Result<*mut ACaptureSessionOutput, CaptureError> {
    let mut output: *mut ACaptureSessionOutput = ptr::null_mut();
    // SAFETY: the window is a live surface the caller owns, and the out pointer is a local.
    let status = unsafe { ACaptureSessionOutput_create(window, &mut output) };
    if status != ACAMERA_OK {
        return Err(status_error("creating a session output", status));
    }
    Ok(output)
}

/// Frees one session output.
///
/// # Safety
///
/// `output` must come from [`output_create`] and must not be used afterwards. The surface it
/// was built from stays the caller's to release.
pub(crate) unsafe fn output_free(output: *mut ACaptureSessionOutput) {
    // SAFETY: the caller guarantees the pointer came from the create call above.
    unsafe { ACaptureSessionOutput_free(output) };
}

/// Creates an output target for one surface.
pub(crate) fn output_target_create(
    window: *mut super::native_window::ANativeWindow,
) -> Result<*mut ACameraOutputTarget, CaptureError> {
    let mut target: *mut ACameraOutputTarget = ptr::null_mut();
    // SAFETY: as in `output_create`.
    let status = unsafe { ACameraOutputTarget_create(window, &mut target) };
    if status != ACAMERA_OK {
        return Err(status_error("creating an output target", status));
    }
    Ok(target)
}

/// Frees one output target.
///
/// # Safety
///
/// `target` must come from [`output_target_create`] and must not be used afterwards.
pub(crate) unsafe fn output_target_free(target: *mut ACameraOutputTarget) {
    // SAFETY: the caller guarantees the pointer came from the create call above.
    unsafe { ACameraOutputTarget_free(target) };
}

/// Creates the capture session for a set of outputs.
///
/// # Safety
///
/// `device` must be an open device, `outputs` a live container, and `callbacks` must outlive
/// the session: the framework keeps a pointer to it for as long as the session exists.
pub(crate) unsafe fn capture_session_create(
    device: *mut ACameraDevice,
    outputs: *const ACaptureSessionOutputContainer,
    callbacks: *const ACameraCaptureSession_stateCallbacks,
) -> Result<*mut ACameraCaptureSession, CaptureError> {
    let mut session: *mut ACameraCaptureSession = ptr::null_mut();
    // SAFETY: the caller guarantees the pointers describe live objects.
    let status = unsafe { ACameraDevice_createCaptureSession(device, outputs, callbacks, &mut session) };
    if status != ACAMERA_OK {
        return Err(status_error("creating the capture session", status));
    }
    Ok(session)
}

/// Builds one request from a platform preset.
///
/// # Safety
///
/// `device` must be an open device.
pub(crate) unsafe fn capture_request_create(
    device: *const ACameraDevice,
    template: RequestTemplate,
) -> Result<*mut ACaptureRequest, CaptureError> {
    let mut request: *mut ACaptureRequest = ptr::null_mut();
    // SAFETY: the caller guarantees the device is open, and the out pointer is a local.
    let status = unsafe { ACameraDevice_createCaptureRequest(device, template, &mut request) };
    if status != ACAMERA_OK {
        return Err(status_error("creating a capture request", status));
    }
    Ok(request)
}

/// Adds one output target to a request.
pub(crate) fn request_add_target(
    request: *mut ACaptureRequest,
    target: *const ACameraOutputTarget,
) -> Result<(), CaptureError> {
    // SAFETY: both pointers are live and owned by the caller.
    let status = unsafe { ACaptureRequest_addTarget(request, target) };
    (status == ACAMERA_OK)
        .then_some(())
        .ok_or_else(|| status_error("adding an output target to the request", status))
}

/// Sets an `int32[]` control entry on a request.
pub(crate) fn request_set_i32(
    request: *mut ACaptureRequest,
    tag: u32,
    values: &[i32],
) -> Result<(), CaptureError> {
    // SAFETY: the request is live, and `values` is a live slice of exactly `count` elements.
    let status = unsafe {
        ACaptureRequest_setEntry_i32(request, tag, values.len() as u32, values.as_ptr())
    };
    (status == ACAMERA_OK)
        .then_some(())
        .ok_or_else(|| status_error("setting a capture request entry", status))
}

/// Frees a request.
///
/// # Safety
///
/// `request` must come from [`capture_request_create`] and must not be used afterwards.
pub(crate) unsafe fn capture_request_free(request: *mut ACaptureRequest) {
    // SAFETY: the caller guarantees the pointer came from the create call above.
    unsafe { ACaptureRequest_free(request) };
}

/// Starts a repeating request, which is what makes the camera stream.
///
/// # Safety
///
/// `session` must be live and `requests` must point at live requests that stay valid until
/// repeating is stopped.
pub(crate) unsafe fn set_repeating_request(
    session: *mut ACameraCaptureSession,
    requests: &mut [*mut ACaptureRequest],
) -> Result<(), CaptureError> {
    // SAFETY: the caller guarantees the session and requests are live. No capture callbacks
    // are registered: this client reads frames from the encoder, not from the camera.
    let status = unsafe {
        ACameraCaptureSession_setRepeatingRequest(
            session,
            ptr::null_mut(),
            requests.len() as c_int,
            requests.as_mut_ptr(),
            ptr::null_mut(),
        )
    };
    (status == ACAMERA_OK)
        .then_some(())
        .ok_or_else(|| status_error("starting the repeating request", status))
}

/// Stops the repeating request; called before the session is closed.
///
/// # Safety
///
/// `session` must be live.
pub(crate) unsafe fn stop_repeating(session: *mut ACameraCaptureSession) {
    // SAFETY: the caller guarantees the session is live. A failure is only logged: the
    // session is about to be closed either way.
    let status = unsafe { ACameraCaptureSession_stopRepeating(session) };
    if status != ACAMERA_OK {
        debug!(status = %status_name(status), "stopping the repeating request failed");
    }
}

/// Closes a session.
///
/// # Safety
///
/// `session` must be live and must not be used afterwards.
pub(crate) unsafe fn capture_session_close(session: *mut ACameraCaptureSession) {
    // SAFETY: the caller guarantees the pointer came from `capture_session_create`.
    unsafe { ACameraCaptureSession_close(session) };
}

/// Submits `requests` as a one-shot burst, which is how the photo channel asks for a still.
///
/// # Safety
///
/// `session` must be live, and each request must stay valid until the session stops using it;
/// every target a request names has to be one of the session's own outputs.
pub(crate) unsafe fn capture_once(
    session: *mut ACameraCaptureSession,
    requests: &mut [*mut ACaptureRequest],
) -> Result<(), CaptureError> {
    // SAFETY: the caller guarantees the session and the requests are live. No capture
    // callbacks are registered: the client reads the still out of its own reader rather than
    // out of the framework's result metadata.
    let status = unsafe {
        ACameraCaptureSession_capture(
            session,
            ptr::null_mut(),
            requests.len() as c_int,
            requests.as_mut_ptr(),
            ptr::null_mut(),
        )
    };
    (status == ACAMERA_OK)
        .then_some(())
        .ok_or_else(|| status_error("submitting a still capture", status))
}
