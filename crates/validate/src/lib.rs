//! Validation and deduplication engine (spec 4.G) and thumbnail matching
//! (spec 4.F).
//!
//! * [`validate_bytes`] decodes a candidate with the maintained `image`
//!   crate, extracts dimensions/EXIF, and computes SHA-256 and a 64-bit
//!   difference hash (dHash).
//! * [`dedupe`] marks exact duplicates (SHA-256) and visually similar
//!   images (dHash Hamming distance) without changing provenance.
//! * [`match_thumbnails`] links thumbnails to originals using aspect ratio,
//!   EXIF timestamps and perceptual hashes. Thumbnails are never upscaled
//!   or relabelled as originals.

pub mod dedupe;
pub mod thumbs;

pub use dedupe::{dedupe, DedupeStats};
pub use thumbs::match_thumbnails;

use image::{ImageFormat as ImgFmt, ImageReader, Limits};
use pr_model::model::{ImageFormat, ValidationStatus};
use std::collections::BTreeMap;
use std::io::Cursor;

/// Maximum bytes of decoder allocation per image (bounded memory).
pub const MAX_DECODE_ALLOC: u64 = 1024 * 1024 * 1024;
pub const MAX_DIMENSION: u32 = 30_000;

#[derive(Debug, Clone, PartialEq)]
pub struct ValidationReport {
    pub status: ValidationStatus,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sha256: String,
    /// 64-bit dHash as 16 hex digits.
    pub dhash: Option<String>,
    pub decode_error: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub warnings: Vec<String>,
}

fn to_image_format(f: ImageFormat) -> Option<ImgFmt> {
    match f {
        ImageFormat::Jpeg => Some(ImgFmt::Jpeg),
        ImageFormat::Png => Some(ImgFmt::Png),
        ImageFormat::Webp => Some(ImgFmt::WebP),
        ImageFormat::Gif => Some(ImgFmt::Gif),
        ImageFormat::Bmp => Some(ImgFmt::Bmp),
        _ => None,
    }
}

/// Validate an in-memory candidate. `structurally_complete` tells the
/// validator whether the carver reached the format's end structure; a
/// partial file that happens to decode is still reported as truncated.
pub fn validate_bytes(data: &[u8], format: ImageFormat, structurally_complete: bool) -> ValidationReport {
    let sha256 = pr_model::evidence::sha256_bytes(data);
    let mut rep = ValidationReport {
        status: ValidationStatus::NotValidated,
        width: None,
        height: None,
        sha256,
        dhash: None,
        decode_error: None,
        metadata: read_exif(data),
        warnings: Vec::new(),
    };
    let Some(fmt) = to_image_format(format) else {
        rep.status = if structurally_complete {
            ValidationStatus::StructureOnly
        } else {
            ValidationStatus::Truncated
        };
        rep.warnings.push(format!(
            "{} pixel decoding is not available in the bundled decoders; structure validated only",
            format.extension().to_uppercase()
        ));
        return rep;
    };
    let mut reader = ImageReader::with_format(Cursor::new(data), fmt);
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    reader.limits(limits);
    match reader.decode() {
        Ok(img) => {
            rep.width = Some(img.width());
            rep.height = Some(img.height());
            rep.dhash = Some(format!("{:016x}", dhash(&img)));
            rep.status = if structurally_complete {
                ValidationStatus::Valid
            } else {
                ValidationStatus::Truncated
            };
            if !structurally_complete {
                rep.warnings.push("decoder accepted partial data; missing regions are rendered by the decoder, not recovered".into());
            }
        }
        Err(e) => {
            rep.decode_error = Some(e.to_string());
            rep.status = if structurally_complete {
                ValidationStatus::DecodeFailed
            } else {
                ValidationStatus::Corrupt
            };
        }
    }
    rep
}

/// 64-bit difference hash: 9x8 grayscale, compare horizontal neighbours.
pub fn dhash(img: &image::DynamicImage) -> u64 {
    let small = img.resize_exact(9, 8, image::imageops::FilterType::Triangle).to_luma8();
    let mut h = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            let l = small.get_pixel(x, y)[0];
            let r = small.get_pixel(x + 1, y)[0];
            h = (h << 1) | (l > r) as u64;
        }
    }
    h
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

pub fn parse_hash(h: &str) -> Option<u64> {
    u64::from_str_radix(h, 16).ok()
}

/// Read a few useful EXIF fields. Errors are ignored: absence of EXIF is normal.
pub fn read_exif(data: &[u8]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(ex) = exif::Reader::new().read_from_container(&mut Cursor::new(data)) else {
        return out;
    };
    let fields = [
        (exif::Tag::DateTimeOriginal, "exif_datetime_original"),
        (exif::Tag::DateTime, "exif_datetime"),
        (exif::Tag::Make, "exif_make"),
        (exif::Tag::Model, "exif_model"),
        (exif::Tag::Orientation, "exif_orientation"),
        (exif::Tag::PixelXDimension, "exif_pixel_x"),
        (exif::Tag::PixelYDimension, "exif_pixel_y"),
    ];
    for (tag, key) in fields {
        if let Some(f) = ex.get_field(tag, exif::In::PRIMARY) {
            let v = f.display_value().to_string().trim_matches('"').trim().to_string();
            if !v.is_empty() {
                out.insert(key.to_string(), v);
            }
        }
    }
    if ex.get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL).is_some() {
        out.insert("exif_has_thumbnail".into(), "true".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pr_testkit::images::{self, JpegOpts};

    #[test]
    fn valid_jpeg_with_exif() {
        let j = images::jpeg(320, 200, 3, &JpegOpts { exif_datetime: Some("2022:02:03 04:05:06".into()), exif_thumbnail: true, ..Default::default() });
        let r = validate_bytes(&j, ImageFormat::Jpeg, true);
        assert_eq!(r.status, ValidationStatus::Valid);
        assert_eq!((r.width, r.height), (Some(320), Some(200)));
        assert_eq!(r.metadata.get("exif_datetime_original").map(String::as_str), Some("2022-02-03 04:05:06"));
        assert_eq!(r.metadata.get("exif_has_thumbnail").map(String::as_str), Some("true"));
        assert!(r.dhash.is_some());
    }

    #[test]
    fn partial_is_never_valid() {
        let j = images::jpeg(320, 200, 3, &JpegOpts::default());
        let mut cut = j[..j.len() / 2].to_vec();
        cut.extend_from_slice(&[0xFF, 0xD9]);
        let r = validate_bytes(&cut, ImageFormat::Jpeg, false);
        assert!(matches!(r.status, ValidationStatus::Truncated | ValidationStatus::Corrupt), "{r:?}");
    }

    #[test]
    fn garbage_fails_decode() {
        let mut g = vec![0xFF, 0xD8, 0xFF, 0xE0];
        g.extend_from_slice(&[0x55; 300]);
        let r = validate_bytes(&g, ImageFormat::Jpeg, true);
        assert_eq!(r.status, ValidationStatus::DecodeFailed);
        assert!(r.decode_error.is_some());
    }

    #[test]
    fn heic_is_structure_only() {
        let h = images::heif_like(10, 10, 100, 1);
        let r = validate_bytes(&h, ImageFormat::Heic, true);
        assert_eq!(r.status, ValidationStatus::StructureOnly);
    }

    #[test]
    fn dhash_similar_for_reencoded_and_different_for_other() {
        let a = image::load_from_memory(&images::jpeg(400, 300, 11, &JpegOpts { quality: 90, ..Default::default() })).unwrap();
        let b = image::load_from_memory(&images::jpeg(400, 300, 11, &JpegOpts { quality: 40, ..Default::default() })).unwrap();
        let t = image::load_from_memory(&images::embedded_thumbnail(400, 300, 11)).unwrap();
        let c = image::load_from_memory(&images::jpeg(400, 300, 12, &JpegOpts::default())).unwrap();
        assert!(hamming(dhash(&a), dhash(&b)) <= 6);
        assert!(hamming(dhash(&a), dhash(&t)) <= 10);
        assert!(hamming(dhash(&a), dhash(&c)) > 12);
    }
}
