//! Raw "disk" images with a ground-truth manifest for carving tests and
//! benchmarks.

use crate::images::{self, JpegOpts};
use crate::{sha256_hex, Rng};
use pr_model::model::ImageFormat;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    /// Whole file stored contiguously; byte-identical recovery expected.
    Intact,
    /// Intact file positioned to straddle a scan-chunk boundary.
    ChunkBoundary,
    /// Intact duplicate of another placed file.
    Duplicate,
    /// File cut short; followed by unrelated data.
    Truncated,
    /// Bytes in the middle overwritten with garbage.
    Corrupted,
    /// File split into two fragments with foreign data between them.
    Fragmented,
    /// Thumbnail-only: a small preview without its original.
    ThumbnailOnly,
    /// Signature-like bytes that are not an image.
    FalsePositive,
}

impl Scenario {
    /// Whether a byte-identical full recovery is possible by carving.
    pub fn fully_recoverable(self) -> bool {
        matches!(self, Scenario::Intact | Scenario::ChunkBoundary | Scenario::Duplicate | Scenario::ThumbnailOnly)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TruthEntry {
    pub id: usize,
    pub scenario: Scenario,
    pub format: ImageFormat,
    /// Absolute offset of the first stored byte.
    pub offset: u64,
    /// Bytes stored contiguously from `offset` (first fragment).
    pub stored_len: u64,
    /// Length and hash of the original, complete file.
    pub original_len: u64,
    pub original_sha256: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Manifest {
    pub seed: u64,
    pub size: u64,
    pub entries: Vec<TruthEntry>,
}

pub struct DiskBuilder {
    pub data: Vec<u8>,
    pub manifest: Manifest,
    rng: Rng,
}

impl DiskBuilder {
    pub fn new(seed: u64) -> Self {
        DiskBuilder {
            data: Vec::new(),
            manifest: Manifest {
                seed,
                ..Default::default()
            },
            rng: Rng::new(seed),
        }
    }

    pub fn rng(&mut self) -> &mut Rng {
        &mut self.rng
    }

    pub fn pos(&self) -> u64 {
        self.data.len() as u64
    }

    /// Random filler that contains no image signatures.
    pub fn random(&mut self, n: usize) -> &mut Self {
        let mut b = self.rng.bytes(n);
        scrub_signatures(&mut b);
        self.data.extend_from_slice(&b);
        self
    }

    pub fn zeros(&mut self, n: usize) -> &mut Self {
        self.data.resize(self.data.len() + n, 0);
        self
    }

    /// Pad with random filler so that the next byte is at `offset`.
    pub fn random_to(&mut self, offset: u64) -> &mut Self {
        let n = offset.saturating_sub(self.pos()) as usize;
        self.random(n)
    }

    #[allow(clippy::too_many_arguments)]
    fn record(&mut self, scenario: Scenario, format: ImageFormat, offset: u64, stored: u64, orig: &[u8], w: u32, h: u32) -> usize {
        let id = self.manifest.entries.len();
        self.manifest.entries.push(TruthEntry {
            id,
            scenario,
            format,
            offset,
            stored_len: stored,
            original_len: orig.len() as u64,
            original_sha256: sha256_hex(orig),
            width: w,
            height: h,
        });
        id
    }

    /// Place a whole file contiguously.
    pub fn place(&mut self, scenario: Scenario, format: ImageFormat, bytes: &[u8], w: u32, h: u32) -> usize {
        let off = self.pos();
        self.data.extend_from_slice(bytes);
        self.record(scenario, format, off, bytes.len() as u64, bytes, w, h)
    }

    /// Place the first `keep` bytes only.
    pub fn place_truncated(&mut self, format: ImageFormat, bytes: &[u8], keep: usize, w: u32, h: u32) -> usize {
        let off = self.pos();
        self.data.extend_from_slice(&bytes[..keep]);
        self.record(Scenario::Truncated, format, off, keep as u64, bytes, w, h)
    }

    /// Overwrite `len` bytes at `at` (relative) with random garbage.
    pub fn place_corrupted(&mut self, format: ImageFormat, bytes: &[u8], at: usize, len: usize, w: u32, h: u32) -> usize {
        let off = self.pos();
        let mut b = bytes.to_vec();
        let mut g = self.rng.bytes(len);
        scrub_signatures(&mut g);
        b[at..at + len].copy_from_slice(&g);
        self.data.extend_from_slice(&b);
        self.record(Scenario::Corrupted, format, off, bytes.len() as u64, bytes, w, h)
    }

    /// Store `bytes[..split]`, then `gap` bytes of foreign random data, then
    /// the remainder.
    pub fn place_fragmented(&mut self, format: ImageFormat, bytes: &[u8], split: usize, gap: usize, w: u32, h: u32) -> usize {
        let off = self.pos();
        self.data.extend_from_slice(&bytes[..split]);
        self.random(gap);
        self.data.extend_from_slice(&bytes[split..]);
        self.record(Scenario::Fragmented, format, off, split as u64, bytes, w, h)
    }

    /// Bytes that start with an image signature but are not an image.
    pub fn place_false_positive(&mut self, format: ImageFormat) -> usize {
        let off = self.pos();
        let mut junk = self.rng.bytes(4096);
        scrub_signatures(&mut junk);
        let head: Vec<u8> = match format {
            ImageFormat::Jpeg => {
                // Plausible APP0 then garbage.
                let mut h = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
                h.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
                h
            }
            ImageFormat::Png => crate::images::png(4, 4, 1)[..16].to_vec(),
            ImageFormat::Webp => {
                let mut h = b"RIFF".to_vec();
                h.extend_from_slice(&3000u32.to_le_bytes());
                h.extend_from_slice(b"WEBPXXXX");
                h
            }
            _ => {
                let mut h = 24u32.to_be_bytes().to_vec();
                h.extend_from_slice(b"ftypheic\0\0\0\0mif1heic");
                h
            }
        };
        self.data.extend_from_slice(&head);
        self.data.extend_from_slice(&junk);
        let id = self.manifest.entries.len();
        self.manifest.entries.push(TruthEntry {
            id,
            scenario: Scenario::FalsePositive,
            format,
            offset: off,
            stored_len: (head.len() + junk.len()) as u64,
            original_len: 0,
            original_sha256: String::new(),
            width: 0,
            height: 0,
        });
        id
    }

    pub fn finish(mut self) -> (Vec<u8>, Manifest) {
        self.manifest.size = self.data.len() as u64;
        (self.data, self.manifest)
    }
}

/// Break byte patterns that would be mistaken for image signatures, so
/// fillers only create the false positives we place deliberately. Other
/// bytes (including 0xFF) are kept so filler behaves like real foreign data.
pub fn scrub_signatures(b: &mut [u8]) {
    for i in 0..b.len() {
        if b[i] == 0xFF && i + 1 < b.len() && b[i + 1] == 0xD8 {
            b[i + 1] = 0x00;
        }
        if i + 4 <= b.len() && (&b[i..i + 4] == b"RIFF" || &b[i..i + 4] == b"ftyp" || &b[i..i + 4] == b"\x89PNG") {
            b[i] = b'x';
        }
    }
}

/// Parameters for the standard mixed-scenario benchmark/test image.
#[derive(Clone, Debug)]
pub struct StandardDiskParams {
    pub seed: u64,
    /// Number of intact JPEGs (other scenarios scale with it).
    pub jpegs: usize,
    pub chunk_size: u64,
    pub min_dim: u32,
    pub max_dim: u32,
}

impl Default for StandardDiskParams {
    fn default() -> Self {
        StandardDiskParams {
            seed: 42,
            jpegs: 12,
            chunk_size: 1 << 20,
            min_dim: 96,
            max_dim: 640,
        }
    }
}

/// Build a disk mixing every scenario in [`Scenario`].
pub fn standard_disk(p: &StandardDiskParams) -> (Vec<u8>, Manifest) {
    let mut d = DiskBuilder::new(p.seed);
    let mut seed = p.seed * 1000;
    let mut next_seed = || {
        seed += 1;
        seed
    };
    let dims = |rng: &mut Rng| {
        let w = rng.range(p.min_dim as u64, p.max_dim as u64) as u32 & !7;
        let h = rng.range(p.min_dim as u64, p.max_dim as u64) as u32 & !7;
        (w.max(16), h.max(16))
    };
    d.random(4096);
    let mut first_intact: Option<Vec<u8>> = None;
    for i in 0..p.jpegs {
        let (w, h) = dims(d.rng());
        let opts = JpegOpts {
            quality: 70 + (i % 4) as u8 * 8,
            progressive: i % 3 == 1,
            restart_interval: (i % 4 == 2).then_some(4),
            exif_thumbnail: i % 2 == 0,
            exif_datetime: Some(format!("2024:05:{:02} 10:{:02}:00", 1 + i % 28, i % 60)),
        };
        let j = images::jpeg(w, h, next_seed(), &opts);
        if first_intact.is_none() {
            first_intact = Some(j.clone());
        }
        d.place(Scenario::Intact, ImageFormat::Jpeg, &j, w, h);
        let gap = d.rng().range(512, 64 * 1024) as usize;
        if i % 3 == 0 {
            d.zeros(gap);
        } else {
            d.random(gap);
        }
    }
    // Chunk-boundary placements: the signature itself straddles a boundary.
    for k in 0..3u64 {
        let (w, h) = dims(d.rng());
        let j = images::jpeg(w, h, next_seed(), &JpegOpts::default());
        let boundary = (d.pos() / p.chunk_size + 1) * p.chunk_size;
        d.random_to(boundary - 1 - k);
        d.place(Scenario::ChunkBoundary, ImageFormat::Jpeg, &j, w, h);
        d.random(1024);
    }
    // Duplicate.
    if let Some(j) = &first_intact {
        let e = d.manifest.entries[0].clone();
        d.place(Scenario::Duplicate, ImageFormat::Jpeg, j, e.width, e.height);
        d.random(2048);
    }
    // Truncated / corrupted / fragmented JPEGs.
    for _ in 0..(p.jpegs / 3).max(2) {
        let (w, h) = dims(d.rng());
        let j = images::jpeg(w, h, next_seed(), &JpegOpts::default());
        let keep = j.len() * 6 / 10;
        d.place_truncated(ImageFormat::Jpeg, &j, keep, w, h);
        d.zeros(8192);

        let (w, h) = dims(d.rng());
        let j = images::jpeg(w, h, next_seed(), &JpegOpts { restart_interval: Some(2), ..Default::default() });
        let at = j.len() / 2;
        d.place_corrupted(ImageFormat::Jpeg, &j, at, 2048.min(j.len() / 4), w, h);
        d.random(4096);

        let (w, h) = dims(d.rng());
        let j = images::jpeg(w, h, next_seed(), &JpegOpts::default());
        let split = j.len() / 2;
        d.place_fragmented(ImageFormat::Jpeg, &j, split, 16 * 1024, w, h);
        d.random(4096);
    }
    // Thumbnail-only: a small JPEG preview whose original is absent.
    for _ in 0..2 {
        let t = images::jpeg(160, 120, next_seed(), &JpegOpts { quality: 70, ..Default::default() });
        d.place(Scenario::ThumbnailOnly, ImageFormat::Jpeg, &t, 160, 120);
        d.random(1024);
    }
    // Other formats.
    for _ in 0..2 {
        let (w, h) = dims(d.rng());
        let png = images::png(w, h, next_seed());
        d.place(Scenario::Intact, ImageFormat::Png, &png, w, h);
        d.random(3000);
        let webp = images::webp_lossless(w / 2 + 8, h / 2 + 8, next_seed());
        d.place(Scenario::Intact, ImageFormat::Webp, &webp, w / 2 + 8, h / 2 + 8);
        d.random(3000);
    }
    let heif = images::heif_like(640, 480, 20_000, next_seed());
    d.place(Scenario::Intact, ImageFormat::Heic, &heif, 640, 480);
    d.random(3000);
    // False positives.
    for f in [ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::Webp, ImageFormat::Heic, ImageFormat::Jpeg] {
        d.place_false_positive(f);
        d.random(1024);
    }
    d.random(8192);
    d.finish()
}
