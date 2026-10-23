//! Exact and perceptual duplicate detection.

use crate::{hamming, parse_hash};
use pr_model::model::{Classification, RecoveryResult, Relation};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct DedupeStats {
    pub exact_groups: usize,
    pub exact_duplicates: usize,
    pub similar_pairs: usize,
}

/// Preference order when choosing which copy of an exact duplicate group
/// is the "primary": existing originals first, then deleted-record and
/// carved data, thumbnails last. Ties break on source path then id.
fn rank(r: &RecoveryResult) -> (u8, String, String) {
    let class_rank = match r.classification {
        Classification::ExistingOriginal => 0,
        Classification::ExistingDuplicate => 1,
        Classification::DeletedFileRecord => 2,
        Classification::CarvedImage => 3,
        Classification::UnverifiedCandidate => 4,
        Classification::PartialImage => 5,
        Classification::Thumbnail => 6,
        Classification::UnavailableSourceData => 7,
    };
    let camera = r
        .source_path
        .as_deref()
        .map(|p| if p.contains("/DCIM/") { "0" } else { "1" })
        .unwrap_or("2");
    (
        class_rank,
        format!("{camera}{}", r.source_path.clone().unwrap_or_default()),
        r.id.clone(),
    )
}

/// Mark exact duplicates and visually similar results.
///
/// * Exact duplicates get a `DuplicateOf` association to the group primary.
///   Only *existing files* are reclassified (to `ExistingDuplicate`);
///   carved/deleted data keeps its classification so provenance is not lost.
/// * Visually similar non-identical images (dHash distance <= `max_distance`,
///   which must be <= 7 for the banding to be exact) get `SimilarTo`.
pub fn dedupe(results: &mut [RecoveryResult], max_distance: u32) -> DedupeStats {
    let mut stats = DedupeStats::default();
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, r) in results.iter().enumerate() {
        if let Some(h) = &r.sha256 {
            if r.classification != Classification::UnavailableSourceData {
                groups.entry(h.clone()).or_default().push(i);
            }
        }
    }
    for idxs in groups.values().filter(|g| g.len() > 1) {
        stats.exact_groups += 1;
        let primary = *idxs.iter().min_by_key(|&&i| rank(&results[i])).unwrap();
        let pid = results[primary].id.clone();
        for &i in idxs.iter().filter(|&&i| i != primary) {
            stats.exact_duplicates += 1;
            let r = &mut results[i];
            r.associate(&pid, Relation::DuplicateOf, "identical SHA-256");
            if r.classification == Classification::ExistingOriginal {
                r.classification = Classification::ExistingDuplicate;
            }
        }
    }

    // Perceptual: 8 bands of 8 bits; any pair within distance <= 7 shares
    // at least one identical band (pigeonhole), so bucketing is exact.
    let max_distance = max_distance.min(7);
    let hashes: Vec<(usize, u64)> = results
        .iter()
        .enumerate()
        .filter(|(_, r)| r.classification != Classification::Thumbnail)
        .filter_map(|(i, r)| {
            r.perceptual_hash
                .as_deref()
                .and_then(parse_hash)
                .map(|h| (i, h))
        })
        .collect();
    let mut buckets: HashMap<(u8, u8), Vec<usize>> = HashMap::new();
    for (k, &(_, h)) in hashes.iter().enumerate() {
        for band in 0..8u8 {
            buckets
                .entry((band, (h >> (band * 8)) as u8))
                .or_default()
                .push(k);
        }
    }
    let mut pairs = std::collections::BTreeSet::new();
    for members in buckets.values() {
        for (x, &a) in members.iter().enumerate() {
            for &b in &members[x + 1..] {
                let (ia, ha) = hashes[a];
                let (ib, hb) = hashes[b];
                if results[ia].sha256.is_some() && results[ia].sha256 == results[ib].sha256 {
                    continue;
                }
                let d = hamming(ha, hb);
                if d <= max_distance {
                    pairs.insert((ia.min(ib), ia.max(ib), d));
                }
            }
        }
    }
    for (a, b, d) in pairs {
        stats.similar_pairs += 1;
        let (ida, idb) = (results[a].id.clone(), results[b].id.clone());
        results[a].associate(&idb, Relation::SimilarTo, format!("dHash distance {d}"));
        results[b].associate(&ida, Relation::SimilarTo, format!("dHash distance {d}"));
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use pr_model::model::{ImageFormat, RecoveryMethod};

    fn r(class: Classification, sha: &str, ph: &str, path: &str) -> RecoveryResult {
        let mut x = RecoveryResult::new(
            "s",
            RecoveryMethod::FilesystemLiveEntry,
            ImageFormat::Jpeg,
            class,
        );
        x.sha256 = Some(sha.into());
        x.perceptual_hash = Some(ph.into());
        x.source_path = Some(path.into());
        x
    }

    #[test]
    fn exact_and_similar() {
        let mut v = vec![
            r(
                Classification::ExistingOriginal,
                "aa",
                "00000000000000ff",
                "/sdcard/Download/x.jpg",
            ),
            r(
                Classification::ExistingOriginal,
                "aa",
                "00000000000000ff",
                "/sdcard/DCIM/Camera/x.jpg",
            ),
            r(Classification::CarvedImage, "aa", "00000000000000ff", ""),
            r(
                Classification::ExistingOriginal,
                "bb",
                "00000000000000fe",
                "/sdcard/DCIM/y.jpg",
            ),
            r(
                Classification::ExistingOriginal,
                "cc",
                "ffffffffffffffff",
                "/sdcard/DCIM/z.jpg",
            ),
        ];
        let s = dedupe(&mut v, 6);
        assert_eq!(s.exact_groups, 1);
        assert_eq!(s.exact_duplicates, 2);
        // DCIM copy is primary.
        assert_eq!(v[1].classification, Classification::ExistingOriginal);
        assert_eq!(v[0].classification, Classification::ExistingDuplicate);
        // Carved copy keeps its classification.
        assert_eq!(v[2].classification, Classification::CarvedImage);
        assert!(v[2]
            .associations
            .iter()
            .any(|a| a.relation == Relation::DuplicateOf && a.result_id == v[1].id));
        assert!(v[3]
            .associations
            .iter()
            .any(|a| a.relation == Relation::SimilarTo));
        assert!(v[4].associations.is_empty());
    }
}
