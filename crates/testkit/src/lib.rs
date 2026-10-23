//! Synthetic fixtures with known ground truth (spec section 6).
//!
//! Everything here is deterministic for a given seed so tests and
//! benchmarks are reproducible.

pub mod disk;
pub mod fat;
pub mod images;

/// SplitMix64 — tiny deterministic PRNG (no external dependency churn).
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[lo, hi)`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo).max(1)
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        self.fill(&mut v);
        v
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    pr_model::evidence::sha256_bytes(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_are_decodable() {
        let j = images::jpeg(
            200,
            150,
            1,
            &images::JpegOpts {
                exif_thumbnail: true,
                progressive: true,
                ..Default::default()
            },
        );
        let img = image::load_from_memory(&j).unwrap();
        assert_eq!((img.width(), img.height()), (200, 150));
        let t = images::embedded_thumbnail(200, 150, 1);
        assert!(j.windows(t.len()).any(|w| w == t.as_slice()));
        assert!(image::load_from_memory(&images::png(30, 20, 2)).is_ok());
        assert!(image::load_from_memory(&images::webp_lossless(30, 20, 3)).is_ok());
    }

    #[test]
    fn fat_builder_layout() {
        let mut b = fat::FatBuilder::new(34, 1);
        b.mkdir("DCIM");
        let f = b.add_file("DCIM/IMG_0001.jpg", &[1, 2, 3]);
        assert_eq!(f.clusters.len(), 1);
        let img = b.finish();
        assert_eq!(&img[82..90], b"FAT32   ");
        assert_eq!(&img[510..512], &[0x55, 0xAA]);
    }
}
