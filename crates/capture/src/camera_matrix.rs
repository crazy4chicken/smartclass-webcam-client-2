//! The resolution / frame-rate matrix a camera announces, and the arithmetic that reads it.
//!
//! Camera APIs hand the matrix over as flat arrays of numbers — Android's
//! `SCALER_AVAILABLE_MIN_FRAME_DURATIONS` is `int64[4*n]` groups of
//! `(format, width, height, duration_ns)`, and its frame-rate ranges are `int32[2*n]` pairs.
//! The conversion from those arrays to `(resolution, frame rates)` is the same arithmetic on
//! every platform, and it is the part most likely to be wrong in a way only a device would
//! show, so it lives here, in a module the host build compiles and tests, rather than inside
//! the platform module that can only be built for Android.

// The Android backend is the only reader so far, so a build that excludes it sees this module
// as unused. Its tests still run everywhere, which is the reason the arithmetic is here.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

/// The highest frame rate a camera is believed to reach.
///
/// The arrays are vendor data: a camera that publishes a minimum frame duration of zero, or
/// of a couple of nanoseconds, would otherwise be announced as capable of hundreds of
/// thousands of frames per second, and the registration claims are stored by the server.
pub(crate) const MAX_PLAUSIBLE_FPS: u32 = 480;

/// The frame rate a minimum frame duration allows, when the number is plausible.
///
/// A duration is the time one frame occupies, so the rate is its reciprocal; anything
/// outside `1..=MAX_PLAUSIBLE_FPS` is treated as absent rather than clamped, because a
/// camera that publishes nonsense for a format should not have that format announced.
pub(crate) fn fps_from_duration(nanoseconds: i64) -> Option<u32> {
    if nanoseconds <= 0 {
        return None;
    }
    let fps = (1_000_000_000f64 / nanoseconds as f64).round();
    (fps >= 1.0 && fps <= MAX_PLAUSIBLE_FPS as f64).then_some(fps as u32)
}

/// Groups the `int64[4*n]` minimum frame durations into a resolution → frame rates matrix.
///
/// Only groups whose format is in `formats` are considered, which is how the caller keeps
/// the matrix to the formats its encoder can actually be fed from. A shorter duration means
/// a higher reachable rate, so the fastest claim wins per resolution. The result is sorted
/// with the largest resolution first, which is the order [`CameraInfo::presets`] documents.
///
/// [`CameraInfo::presets`]: crate::CameraInfo
pub(crate) fn fps_by_resolution(durations: &[i64], formats: &[i32]) -> Vec<(String, Vec<u32>)> {
    let mut rows: Vec<(u32, u32, u32)> = Vec::new();

    for group in durations.chunks_exact(4) {
        let (format, width, height, duration) = (group[0] as i32, group[1], group[2], group[3]);
        if !formats.contains(&format) || width <= 0 || height <= 0 {
            continue;
        }
        let Some(fps) = fps_from_duration(duration) else {
            continue;
        };

        let (width, height) = (width as u32, height as u32);
        match rows.iter_mut().find(|row| row.0 == width && row.1 == height) {
            Some(row) => row.2 = row.2.max(fps),
            None => rows.push((width, height, fps)),
        }
    }

    rows.sort_by(|a, b| (b.0 * b.1, b.0).cmp(&(a.0 * a.1, a.0)));
    rows.into_iter()
        .map(|(width, height, fps)| (format!("{width}x{height}"), vec![fps]))
        .collect()
}

/// Turns the `int32[2*n]` frame-rate tag into ascending, de-duplicated ranges.
///
/// The metadata header stores two frame rates per range without saying which bound comes
/// first, and the platform's own tooling has read them both ways, so each pair is taken as
/// `min`/`max` instead of trusting an order that cannot be verified from here. A pair whose
/// upper bound is zero or negative is dropped: it describes no usable range.
pub(crate) fn fps_ranges(values: &[i32]) -> Vec<(u32, u32)> {
    let mut ranges: Vec<(u32, u32)> = values
        .chunks_exact(2)
        .filter_map(|pair| {
            let first = u32::try_from(pair[0]).ok()?;
            let second = u32::try_from(pair[1]).ok()?;
            Some((first.min(second), first.max(second)))
        })
        .filter(|(lower, upper)| *upper > 0 && *lower <= *upper)
        .collect();
    ranges.sort_unstable();
    ranges.dedup();
    ranges
}

#[cfg(test)]
mod tests {
    use super::{fps_by_resolution, fps_from_duration, fps_ranges};

    /// Android's `AIMAGE_FORMAT_PRIVATE`: what feeds a hardware encoder.
    const PRIVATE: i32 = 0x22;
    /// Android's `AIMAGE_FORMAT_YUV_420_888`: what feeds a software encoder.
    const YUV: i32 = 0x23;
    /// Android's `AIMAGE_FORMAT_BLOB`, a format this backend never streams.
    const BLOB: i32 = 0x21;

    #[test]
    fn duration_becomes_frame_rate() {
        assert_eq!(fps_from_duration(33_333_333), Some(30));
        assert_eq!(fps_from_duration(16_666_667), Some(60));
        // A camera that cannot stream at all, and one that claims a gigahertz.
        assert_eq!(fps_from_duration(0), None);
        assert_eq!(fps_from_duration(-1), None);
        assert_eq!(fps_from_duration(1), None);
    }

    #[test]
    fn ranges_are_normalized_and_deduplicated() {
        // The pairs arrive in both orders; both have to read as the same range.
        assert_eq!(
            fps_ranges(&[30, 30, 15, 30, 60, 15, 30, 30]),
            vec![(15, 30), (15, 60), (30, 30)]
        );
    }

    #[test]
    fn malformed_ranges_produce_nothing() {
        assert_eq!(fps_ranges(&[]), Vec::new());
        // A trailing half-pair is not a range.
        assert_eq!(fps_ranges(&[30]), Vec::new());
        // Negative bounds are not frame rates.
        assert_eq!(fps_ranges(&[-1, 0]), Vec::new());
    }

    #[test]
    fn matrix_keeps_the_fastest_rate_per_resolution() {
        let durations = [
            // Private 4K is the fast claim; the YUV row for the same size must not win.
            PRIVATE as i64,
            3840,
            2160,
            16_666_667,
            YUV as i64,
            3840,
            2160,
            33_333_333,
            PRIVATE as i64,
            1920,
            1080,
            33_333_333,
            // A format this backend does not stream is ignored entirely.
            BLOB as i64,
            640,
            480,
            16_666_667,
        ];

        assert_eq!(
            fps_by_resolution(&durations, &[PRIVATE, YUV]),
            vec![
                ("3840x2160".to_owned(), vec![60]),
                ("1920x1080".to_owned(), vec![30]),
            ]
        );
    }

    #[test]
    fn matrix_ignores_incomplete_groups() {
        // Three elements where the tag promises four: not a row.
        assert_eq!(fps_by_resolution(&[PRIVATE as i64, 1280, 720], &[PRIVATE]), Vec::new());
        // A zero-sized frame is not a resolution.
        assert_eq!(
            fps_by_resolution(&[PRIVATE as i64, 0, 0, 33_333_333], &[PRIVATE]),
            Vec::new()
        );
    }
}
