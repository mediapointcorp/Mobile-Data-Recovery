//! Turning engine outputs into classified [`RecoveryResult`]s.

use pr_carve::export::ExportInfo;
use pr_carve::scanner::Found;
use pr_carve::EndStatus;
use pr_model::model::{
    Classification, ImageFormat, RecoveryMethod, RecoveryResult, ValidationStatus,
};
use pr_validate::ValidationReport;

/// Complete, decodable carved images no larger than this (longest side)
/// are classified as thumbnails/previews rather than originals.
pub const THUMBNAIL_MAX_DIM: u32 = 320;

/// Where carved data came from.
#[derive(Clone, Debug)]
pub enum CarveContext {
    /// Raw evidence image (disk/partition dump).
    RawImage,
    /// A thumbnail cache container file such as `.thumbdata*`.
    ThumbnailContainer { container_path: String },
}

pub fn from_carved(
    source_id: &str,
    found: &Found,
    export: &ExportInfo,
    output_rel: &str,
    v: &ValidationReport,
    ctx: &CarveContext,
) -> RecoveryResult {
    let c = &found.candidate;
    let complete = c.status == EndStatus::Complete;
    let width = v.width.or(c.width);
    let height = v.height.or(c.height);
    let decoded = matches!(
        v.status,
        ValidationStatus::Valid | ValidationStatus::ValidWithWarnings
    );
    let small = decoded && width.max(height).is_some_and(|d| d <= THUMBNAIL_MAX_DIM);
    let in_container = matches!(ctx, CarveContext::ThumbnailContainer { .. });

    let (classification, method) = if found.embedded_in.is_some() {
        (Classification::Thumbnail, RecoveryMethod::EmbeddedThumbnail)
    } else if in_container {
        (
            Classification::Thumbnail,
            RecoveryMethod::ThumbnailCacheCarve,
        )
    } else if !complete {
        (Classification::PartialImage, RecoveryMethod::RawCarve)
    } else if decoded && small {
        (Classification::Thumbnail, RecoveryMethod::RawCarve)
    } else if decoded {
        (Classification::CarvedImage, RecoveryMethod::RawCarve)
    } else {
        (
            Classification::UnverifiedCandidate,
            RecoveryMethod::RawCarve,
        )
    };

    let mut r = RecoveryResult::new(source_id, method, c.format, classification);
    r.source_offset = Some(c.offset);
    r.candidate_length = Some(c.length);
    r.width = width;
    r.height = height;
    r.sha256 = Some(export.sha256.clone());
    r.perceptual_hash = v.dhash.clone();
    r.output_path = Some(output_rel.to_string());
    r.diagnostics = c.diagnostics.clone();
    r.metadata = v.metadata.clone();
    r.metadata.insert(
        "carve_status".into(),
        format!("{:?}", c.status).to_lowercase(),
    );
    if let CarveContext::ThumbnailContainer { container_path } = ctx {
        r.source_path = Some(container_path.clone());
        r.metadata
            .insert("container_offset".into(), c.offset.to_string());
    }
    // Validation status: a partial file is never "valid", whatever the decoder says.
    r.validation = if complete {
        v.status
    } else if c.status == EndStatus::Truncated {
        ValidationStatus::Truncated
    } else {
        ValidationStatus::Corrupt
    };
    if let Some(why) = &c.end_reason {
        r.diagnostics.push(format!("end: {why}"));
    }
    if let Some(e) = &v.decode_error {
        r.diagnostics.push(format!("decoder: {e}"));
    }
    r.warnings.extend(v.warnings.iter().cloned());
    if export.trailer_appended {
        r.metadata
            .insert("source_sha256".into(), export.source_sha256.clone());
        r.warnings.push("An end-of-image marker was appended on export so viewers can open the partial data; the SHA-256 covers the exported file.".into());
    }
    match classification {
        Classification::PartialImage => {
            r.limitations.push("Only the data before the break was recovered. Missing parts are not reconstructed; if the file was fragmented, the remaining fragments were not located.".into());
        }
        Classification::Thumbnail if found.embedded_in.is_none() && !in_container => {
            r.warnings.push(format!(
                "Small image (≤ {THUMBNAIL_MAX_DIM}px): classified as a thumbnail/preview by size."
            ));
        }
        Classification::UnverifiedCandidate => {
            r.limitations.push(match c.format {
                ImageFormat::Heic | ImageFormat::Avif => "Container structure validated; pixel data could not be decoded with the bundled decoders, so image content is unverified.".into(),
                _ => "Structure ended correctly but the image failed to decode; content may be damaged or from another file.".into(),
            });
        }
        _ => {}
    }
    r
}

/// Result for a file that exists (device copy, local folder, live FS entry
/// or deleted FS record read from an image).
#[allow(clippy::too_many_arguments)]
pub fn from_file_bytes(
    source_id: &str,
    method: RecoveryMethod,
    classification: Classification,
    source_path: &str,
    output_rel: Option<String>,
    bytes: &[u8],
    name_hint: &str,
) -> RecoveryResult {
    let sniffed = ImageFormat::sniff(bytes);
    let format = if sniffed == ImageFormat::Unknown {
        ImageFormat::from_extension(name_hint)
    } else {
        sniffed
    };
    let v = pr_validate::validate_bytes(bytes, format, true);
    let mut r = RecoveryResult::new(source_id, method, format, classification);
    r.source_path = Some(source_path.to_string());
    r.output_path = output_rel;
    r.candidate_length = Some(bytes.len() as u64);
    r.width = v.width;
    r.height = v.height;
    r.sha256 = Some(v.sha256.clone());
    r.perceptual_hash = v.dhash.clone();
    r.validation = v.status;
    r.metadata = v.metadata;
    r.warnings = v.warnings;
    if let Some(e) = v.decode_error {
        r.diagnostics.push(format!("decoder: {e}"));
        r.warnings
            .push("File could not be decoded; it may be damaged or not an image.".into());
    }
    if sniffed != ImageFormat::Unknown
        && ImageFormat::from_extension(name_hint) != ImageFormat::Unknown
        && sniffed != ImageFormat::from_extension(name_hint)
    {
        r.warnings.push(format!(
            "Content is {} but the file name suggests otherwise.",
            sniffed.extension()
        ));
    }
    r
}
