//! Recovery result model (spec section 4.H).
//!
//! Every engine — existing-file discovery, filesystem recovery, carving and
//! thumbnail recovery — produces [`RecoveryResult`] values. Classifications
//! are deliberately conservative: a file that still exists on the device is
//! never reported as "recovered deleted data", and a thumbnail is never
//! reported as an original.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How a result should be interpreted by the user.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// A file that still exists in an accessible location (not deleted-data recovery).
    ExistingOriginal,
    /// A byte-identical copy of another existing file.
    ExistingDuplicate,
    /// Recovered through a deleted filesystem record whose data validated.
    DeletedFileRecord,
    /// A complete, decodable image carved from raw data.
    CarvedImage,
    /// A preview / thumbnail. Never a substitute for the original.
    Thumbnail,
    /// Truncated, corrupted or fragmented data exported as a partial image.
    PartialImage,
    /// Structurally plausible but could not be fully validated.
    UnverifiedCandidate,
    /// A record exists but its data is overwritten, encrypted or inaccessible.
    UnavailableSourceData,
}

impl Classification {
    pub const ALL: [Classification; 8] = [
        Classification::ExistingOriginal,
        Classification::ExistingDuplicate,
        Classification::DeletedFileRecord,
        Classification::CarvedImage,
        Classification::Thumbnail,
        Classification::PartialImage,
        Classification::UnverifiedCandidate,
        Classification::UnavailableSourceData,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Classification::ExistingOriginal => "Existing original",
            Classification::ExistingDuplicate => "Existing duplicate",
            Classification::DeletedFileRecord => "Deleted-file record",
            Classification::CarvedImage => "Carved image",
            Classification::Thumbnail => "Thumbnail",
            Classification::PartialImage => "Partial image",
            Classification::UnverifiedCandidate => "Unverified candidate",
            Classification::UnavailableSourceData => "Unavailable source data",
        }
    }

    /// Directory name used under `recovered/`.
    pub fn dir_name(self) -> &'static str {
        match self {
            Classification::ExistingOriginal => "existing-originals",
            Classification::ExistingDuplicate => "existing-duplicates",
            Classification::DeletedFileRecord => "deleted-file-records",
            Classification::CarvedImage => "carved",
            Classification::Thumbnail => "thumbnails",
            Classification::PartialImage => "partial",
            Classification::UnverifiedCandidate => "unverified",
            Classification::UnavailableSourceData => "unavailable",
        }
    }

    /// True only for results that represent data recovered from deleted or
    /// unallocated storage (as opposed to files that still exist).
    pub fn is_deleted_data_recovery(self) -> bool {
        matches!(
            self,
            Classification::DeletedFileRecord
                | Classification::CarvedImage
                | Classification::PartialImage
        )
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryMethod {
    /// File found in a user-selected local folder (e.g. an existing backup).
    LocalDirectoryScan,
    /// Live (allocated) file read from a parsed filesystem in an image.
    FilesystemLiveEntry,
    /// Deleted directory entry recovered from a parsed filesystem.
    FilesystemDeletedEntry,
    /// Signature + structure based carving of raw data.
    RawCarve,
    /// Thumbnail embedded in another image (e.g. EXIF APP1).
    EmbeddedThumbnail,
    /// Carved out of a thumbnail cache container (e.g. `.thumbdata`).
    ThumbnailCacheCarve,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    Jpeg,
    Png,
    Webp,
    Heic,
    Avif,
    Gif,
    Bmp,
    Dng,
    Unknown,
}

impl ImageFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
            ImageFormat::Webp => "webp",
            ImageFormat::Heic => "heic",
            ImageFormat::Avif => "avif",
            ImageFormat::Gif => "gif",
            ImageFormat::Bmp => "bmp",
            ImageFormat::Dng => "dng",
            ImageFormat::Unknown => "bin",
        }
    }

    /// Guess a format from a file name's extension.
    pub fn from_extension(name: &str) -> ImageFormat {
        let lower = name.to_ascii_lowercase();
        let ext = lower.rsplit('.').next().unwrap_or("");
        match ext {
            "jpg" | "jpeg" | "jpe" | "jfif" => ImageFormat::Jpeg,
            "png" => ImageFormat::Png,
            "webp" => ImageFormat::Webp,
            "heic" | "heif" | "hif" => ImageFormat::Heic,
            "avif" => ImageFormat::Avif,
            "gif" => ImageFormat::Gif,
            "bmp" => ImageFormat::Bmp,
            "dng" => ImageFormat::Dng,
            _ => ImageFormat::Unknown,
        }
    }

    /// Sniff a format from the first bytes of a file.
    pub fn sniff(head: &[u8]) -> ImageFormat {
        if head.len() >= 3 && head[..3] == [0xFF, 0xD8, 0xFF] {
            ImageFormat::Jpeg
        } else if head.len() >= 8 && head[..8] == *b"\x89PNG\r\n\x1a\n" {
            ImageFormat::Png
        } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
            ImageFormat::Webp
        } else if head.len() >= 12 && &head[4..8] == b"ftyp" {
            match &head[8..12] {
                b"avif" | b"avis" => ImageFormat::Avif,
                b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" | b"heim" | b"heis" => {
                    ImageFormat::Heic
                }
                _ => ImageFormat::Unknown,
            }
        } else if head.len() >= 6 && (&head[..6] == b"GIF87a" || &head[..6] == b"GIF89a") {
            ImageFormat::Gif
        } else if head.len() >= 2 && &head[..2] == b"BM" {
            ImageFormat::Bmp
        } else {
            ImageFormat::Unknown
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStatus {
    /// Structure valid and fully decoded by a maintained image library.
    Valid,
    /// Decoded, but with warnings (e.g. restart marker anomalies).
    ValidWithWarnings,
    /// Data ends before the format's end marker.
    Truncated,
    /// Structure or entropy data is damaged.
    Corrupt,
    /// Container structure validated; pixel decoding is not available for
    /// this format in the bundled decoders (e.g. HEIC).
    StructureOnly,
    /// Structure looked valid but decoding failed.
    DecodeFailed,
    /// Not validated (e.g. no data available).
    NotValidated,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    EvidenceImage,
    LocalDirectory,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SourceRef {
    /// Stable identifier, e.g. `drive:E:` or `image:<sha256 prefix>`.
    pub source_id: String,
    pub kind: SourceKind,
    pub description: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    /// This result is a thumbnail of the referenced result.
    ThumbnailOf,
    /// The referenced result is a thumbnail of this one.
    HasThumbnail,
    /// Byte-identical (same SHA-256) to the referenced result.
    DuplicateOf,
    /// Visually similar (perceptual hash) to the referenced result.
    SimilarTo,
    /// This result was found inside the referenced result's bytes.
    EmbeddedIn,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Association {
    pub result_id: String,
    pub relation: Relation,
    /// Human-readable basis for the association (e.g. "dHash distance 3").
    pub basis: String,
}

/// One recovery result (spec 4.H).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RecoveryResult {
    pub id: String,
    pub source_id: String,
    pub method: RecoveryMethod,
    pub format: ImageFormat,
    /// Byte offset in the source image / container, if known.
    pub source_offset: Option<u64>,
    /// Path on the source (device path or filesystem path), if known.
    pub source_path: Option<String>,
    pub candidate_length: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sha256: Option<String>,
    /// 64-bit difference hash (hex) for visual similarity.
    pub perceptual_hash: Option<String>,
    pub validation: ValidationStatus,
    pub classification: Classification,
    /// Exported file, relative to the case directory.
    pub output_path: Option<String>,
    pub warnings: Vec<String>,
    pub limitations: Vec<String>,
    /// Parser diagnostics (segment layout, failure reasons...).
    pub diagnostics: Vec<String>,
    pub associations: Vec<Association>,
    /// Extra metadata: EXIF date, camera model, filesystem timestamps, ...
    pub metadata: BTreeMap<String, String>,
}

impl RecoveryResult {
    pub fn new(
        source_id: impl Into<String>,
        method: RecoveryMethod,
        format: ImageFormat,
        classification: Classification,
    ) -> Self {
        RecoveryResult {
            id: uuid::Uuid::new_v4().to_string(),
            source_id: source_id.into(),
            method,
            format,
            source_offset: None,
            source_path: None,
            candidate_length: None,
            width: None,
            height: None,
            sha256: None,
            perceptual_hash: None,
            validation: ValidationStatus::NotValidated,
            classification,
            output_path: None,
            warnings: Vec::new(),
            limitations: Vec::new(),
            diagnostics: Vec::new(),
            associations: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    pub fn associate(&mut self, result_id: &str, relation: Relation, basis: impl Into<String>) {
        let basis = basis.into();
        if !self
            .associations
            .iter()
            .any(|a| a.result_id == result_id && a.relation == relation)
        {
            self.associations.push(Association {
                result_id: result_id.to_string(),
                relation,
                basis,
            });
        }
    }
}

/// Aggregate counts that never mix inaccessible data with scanned data.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ResultSummary {
    pub total: usize,
    pub by_classification: BTreeMap<String, usize>,
    pub by_format: BTreeMap<String, usize>,
    /// Results that are genuinely recovered deleted/unallocated data.
    pub deleted_data_recovered: usize,
    /// Results that are files which still exist (originals and duplicates).
    pub existing_files: usize,
    pub thumbnails: usize,
    /// Records whose data is not available (never counted as recovered).
    pub unavailable: usize,
}

impl ResultSummary {
    pub fn from_results(results: &[RecoveryResult]) -> Self {
        let mut s = ResultSummary {
            total: results.len(),
            ..Default::default()
        };
        for r in results {
            *s.by_classification
                .entry(r.classification.label().to_string())
                .or_default() += 1;
            *s.by_format
                .entry(r.format.extension().to_string())
                .or_default() += 1;
            match r.classification {
                Classification::ExistingOriginal | Classification::ExistingDuplicate => {
                    s.existing_files += 1
                }
                Classification::Thumbnail => s.thumbnails += 1,
                Classification::UnavailableSourceData => s.unavailable += 1,
                c if c.is_deleted_data_recovery() => s.deleted_data_recovered += 1,
                _ => {}
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_formats() {
        assert_eq!(
            ImageFormat::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]),
            ImageFormat::Jpeg
        );
        assert_eq!(
            ImageFormat::sniff(b"\x89PNG\r\n\x1a\n...."),
            ImageFormat::Png
        );
        assert_eq!(
            ImageFormat::sniff(b"RIFF\0\0\0\0WEBPVP8 "),
            ImageFormat::Webp
        );
        assert_eq!(ImageFormat::sniff(b"\0\0\0\x18ftypheic"), ImageFormat::Heic);
        assert_eq!(
            ImageFormat::sniff(b"\0\0\0\x18ftypisom"),
            ImageFormat::Unknown
        );
        assert_eq!(ImageFormat::from_extension("IMG_1.JPEG"), ImageFormat::Jpeg);
    }

    #[test]
    fn summary_separates_existing_and_recovered() {
        let mk = |c| RecoveryResult::new("s", RecoveryMethod::RawCarve, ImageFormat::Jpeg, c);
        let rs = vec![
            mk(Classification::ExistingOriginal),
            mk(Classification::CarvedImage),
            mk(Classification::PartialImage),
            mk(Classification::UnavailableSourceData),
            mk(Classification::Thumbnail),
        ];
        let s = ResultSummary::from_results(&rs);
        assert_eq!(s.existing_files, 1);
        assert_eq!(s.deleted_data_recovered, 2);
        assert_eq!(s.unavailable, 1);
        assert_eq!(s.thumbnails, 1);
    }

    #[test]
    fn serde_roundtrip() {
        let r = RecoveryResult::new(
            "s",
            RecoveryMethod::RawCarve,
            ImageFormat::Png,
            Classification::CarvedImage,
        );
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains("\"carved_image\""));
        let back: RecoveryResult = serde_json::from_str(&j).unwrap();
        assert_eq!(back, r);
    }
}
