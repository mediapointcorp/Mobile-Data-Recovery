//! Thumbnail-to-original matching.

use crate::{hamming, parse_hash};
use pr_model::model::{Classification, RecoveryResult, Relation};

/// Maximum dHash distance accepted for a thumbnail/original match.
pub const THUMB_MAX_DISTANCE: u32 = 10;
/// Maximum aspect-ratio difference (relative) for a match.
pub const THUMB_ASPECT_TOLERANCE: f64 = 0.03;
/// Cap on pairwise comparisons to keep matching bounded.
pub const MAX_COMPARISONS: usize = 200_000_000;

fn aspect(r: &RecoveryResult) -> Option<f64> {
    match (r.width, r.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some(w as f64 / h as f64),
        _ => None,
    }
}

/// Link thumbnails to candidate originals. Returns number of links made.
///
/// Evidence considered, all recorded in the association basis:
/// * explicit embedding (already linked by the carver) — kept as is;
/// * matching EXIF DateTimeOriginal;
/// * same aspect ratio (within tolerance) and dHash distance.
///
/// A match requires a perceptual match; timestamps only strengthen it.
pub fn match_thumbnails(results: &mut [RecoveryResult]) -> usize {
    let thumbs: Vec<usize> = (0..results.len())
        .filter(|&i| results[i].classification == Classification::Thumbnail)
        .collect();
    let originals: Vec<usize> = (0..results.len())
        .filter(|&i| {
            matches!(
                results[i].classification,
                Classification::ExistingOriginal
                    | Classification::ExistingDuplicate
                    | Classification::DeletedFileRecord
                    | Classification::CarvedImage
            )
        })
        .collect();
    if thumbs.len().saturating_mul(originals.len()) > MAX_COMPARISONS {
        return 0;
    }
    let mut links = Vec::new();
    for &t in &thumbs {
        let Some(th) = results[t].perceptual_hash.as_deref().and_then(parse_hash) else {
            continue;
        };
        let Some(ta) = aspect(&results[t]) else {
            continue;
        };
        let tdate = results[t].metadata.get("exif_datetime_original").cloned();
        let mut best: Option<(u32, usize, bool)> = None;
        for &o in &originals {
            let Some(oh) = results[o].perceptual_hash.as_deref().and_then(parse_hash) else {
                continue;
            };
            let Some(oa) = aspect(&results[o]) else {
                continue;
            };
            // Thumbnails must be smaller; never treat a larger image as a thumbnail.
            if results[o].width <= results[t].width {
                continue;
            }
            if ((ta - oa) / oa).abs() > THUMB_ASPECT_TOLERANCE {
                continue;
            }
            let d = hamming(th, oh);
            if d > THUMB_MAX_DISTANCE {
                continue;
            }
            let same_date = tdate.is_some()
                && tdate == results[o].metadata.get("exif_datetime_original").cloned();
            let better = match best {
                None => true,
                Some((bd, _, bdate)) => {
                    (same_date, std::cmp::Reverse(d)) > (bdate, std::cmp::Reverse(bd))
                }
            };
            if better {
                best = Some((d, o, same_date));
            }
        }
        if let Some((d, o, same_date)) = best {
            if results[t]
                .associations
                .iter()
                .any(|a| a.relation == Relation::EmbeddedIn && a.result_id == results[o].id)
            {
                continue;
            }
            let basis = format!(
                "dHash distance {d}, aspect ratio match{}",
                if same_date {
                    ", same EXIF DateTimeOriginal"
                } else {
                    ""
                }
            );
            links.push((t, o, basis));
        }
    }
    let n = links.len();
    for (t, o, basis) in links {
        let (tid, oid) = (results[t].id.clone(), results[o].id.clone());
        results[t].associate(&oid, Relation::ThumbnailOf, basis.clone());
        results[o].associate(&tid, Relation::HasThumbnail, basis);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate_bytes;
    use pr_model::model::{ImageFormat, RecoveryMethod};
    use pr_testkit::images::{self, JpegOpts};

    fn res(data: &[u8], class: Classification) -> RecoveryResult {
        let v = validate_bytes(data, ImageFormat::Jpeg, true);
        let mut r = RecoveryResult::new("s", RecoveryMethod::RawCarve, ImageFormat::Jpeg, class);
        r.width = v.width;
        r.height = v.height;
        r.perceptual_hash = v.dhash;
        r.metadata = v.metadata;
        r
    }

    #[test]
    fn links_thumbnail_to_its_original_only() {
        let o1 = images::jpeg(640, 480, 21, &JpegOpts::default());
        let o2 = images::jpeg(640, 480, 22, &JpegOpts::default());
        let t1 = images::embedded_thumbnail(640, 480, 21);
        let mut v = vec![
            res(&o1, Classification::CarvedImage),
            res(&o2, Classification::ExistingOriginal),
            res(&t1, Classification::Thumbnail),
        ];
        assert_eq!(match_thumbnails(&mut v), 1);
        assert_eq!(v[2].associations[0].result_id, v[0].id);
        assert_eq!(v[2].associations[0].relation, Relation::ThumbnailOf);
        // The thumbnail stays a thumbnail.
        assert_eq!(v[2].classification, Classification::Thumbnail);
        assert_eq!(v[2].width, Some(160));
    }

    #[test]
    fn thumbnail_only_scenario_has_no_false_link() {
        let o = images::jpeg(640, 480, 31, &JpegOpts::default());
        let t = images::embedded_thumbnail(640, 480, 32);
        let mut v = vec![
            res(&o, Classification::CarvedImage),
            res(&t, Classification::Thumbnail),
        ];
        assert_eq!(match_thumbnails(&mut v), 0);
    }
}
