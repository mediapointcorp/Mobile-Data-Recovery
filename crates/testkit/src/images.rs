//! Real encoded test images (decodable by standard decoders).

use crate::Rng;
use image::{ImageEncoder, RgbImage};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

#[derive(Clone, Debug)]
pub struct JpegOpts {
    pub quality: u8,
    pub progressive: bool,
    pub restart_interval: Option<u16>,
    /// Embed an EXIF APP1 segment containing a JPEG thumbnail.
    pub exif_thumbnail: bool,
    /// EXIF DateTimeOriginal (format `YYYY:MM:DD HH:MM:SS`).
    pub exif_datetime: Option<String>,
}

impl Default for JpegOpts {
    fn default() -> Self {
        JpegOpts {
            quality: 85,
            progressive: false,
            restart_interval: None,
            exif_thumbnail: false,
            exif_datetime: None,
        }
    }
}

/// Deterministic "photo-like" picture: gradients, shapes and noise, so that
/// different seeds give visually distinct images (distinct perceptual hashes).
pub fn picture(w: u32, h: u32, seed: u64) -> RgbImage {
    let mut rng = Rng::new(seed);
    let base = [
        rng.range(0, 255) as f32,
        rng.range(0, 255) as f32,
        rng.range(0, 255) as f32,
    ];
    let shapes: Vec<(f32, f32, f32, [u8; 3])> = (0..6)
        .map(|_| {
            (
                rng.range(0, 1000) as f32 / 1000.0,
                rng.range(0, 1000) as f32 / 1000.0,
                rng.range(50, 350) as f32 / 1000.0,
                [
                    rng.range(0, 255) as u8,
                    rng.range(0, 255) as u8,
                    rng.range(0, 255) as u8,
                ],
            )
        })
        .collect();
    let mut img = RgbImage::new(w, h);
    let mut noise = Rng::new(seed.wrapping_mul(31));
    for (x, y, px) in img.enumerate_pixels_mut() {
        let fx = x as f32 / w as f32;
        let fy = y as f32 / h as f32;
        let mut c = [
            base[0] * (1.0 - fx) + 255.0 * fx * 0.3,
            base[1] * (1.0 - fy) + 200.0 * fy * 0.4,
            base[2] * 0.5 + 120.0 * (fx * fy),
        ];
        for (cx, cy, r, col) in &shapes {
            let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
            if d < *r {
                for i in 0..3 {
                    c[i] = col[i] as f32;
                }
            }
        }
        let n = (noise.next_u64() % 17) as f32 - 8.0;
        *px = image::Rgb([
            (c[0] + n).clamp(0.0, 255.0) as u8,
            (c[1] + n).clamp(0.0, 255.0) as u8,
            (c[2] + n).clamp(0.0, 255.0) as u8,
        ]);
    }
    img
}

fn encode_jpeg_raw(img: &RgbImage, opts: &JpegOpts, exif: Option<Vec<u8>>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = Encoder::new(&mut out, opts.quality);
    enc.set_sampling_factor(SamplingFactor::R_4_2_0);
    enc.set_progressive(opts.progressive);
    if let Some(ri) = opts.restart_interval {
        enc.set_restart_interval(ri);
    }
    if let Some(tiff) = exif {
        enc.add_exif_metadata(&tiff).expect("exif fits");
    }
    enc.encode(
        img.as_raw(),
        img.width() as u16,
        img.height() as u16,
        ColorType::Rgb,
    )
    .expect("jpeg encode");
    out
}

/// Encode a JPEG. With `exif_thumbnail`, a 160px-wide thumbnail of the same
/// picture is embedded in EXIF IFD1 (as cameras and phones do).
pub fn jpeg(w: u32, h: u32, seed: u64, opts: &JpegOpts) -> Vec<u8> {
    let img = picture(w, h, seed);
    let thumb = opts.exif_thumbnail.then(|| {
        let tw = 160u32.min(w);
        let th = (h * tw / w).max(1);
        let small = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
        encode_jpeg_raw(
            &small,
            &JpegOpts {
                quality: 70,
                ..Default::default()
            },
            None,
        )
    });
    let exif = (thumb.is_some() || opts.exif_datetime.is_some())
        .then(|| exif_tiff(opts.exif_datetime.as_deref(), thumb.as_deref()));
    encode_jpeg_raw(&img, opts, exif)
}

/// Thumbnail bytes embedded by [`jpeg`] for the same parameters.
pub fn embedded_thumbnail(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let img = picture(w, h, seed);
    let tw = 160u32.min(w);
    let th = (h * tw / w).max(1);
    let small = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
    encode_jpeg_raw(
        &small,
        &JpegOpts {
            quality: 70,
            ..Default::default()
        },
        None,
    )
}

/// Build a little-endian TIFF structure for EXIF: IFD0 (Make, Model,
/// optional ExifIFD with DateTimeOriginal) and optional IFD1 thumbnail.
pub fn exif_tiff(datetime: Option<&str>, thumbnail: Option<&[u8]>) -> Vec<u8> {
    // Layout: header(8) | IFD0 | ExifIFD | IFD1 | data area
    let make = b"PhotoRecoveryTest\0";
    let model = b"Synthetic\0";
    let dt: Vec<u8> = datetime
        .map(|d| {
            let mut v = d.as_bytes().to_vec();
            v.push(0);
            v
        })
        .unwrap_or_default();

    let ifd0_entries = 2 + datetime.is_some() as u16;
    let ifd0_len = 2 + 12 * ifd0_entries as u32 + 4;
    let exif_ifd_len = if datetime.is_some() { 2 + 12 + 4 } else { 0 };
    let ifd1_len = if thumbnail.is_some() {
        2 + 12 * 2 + 4
    } else {
        0
    };
    let ifd0_off = 8u32;
    let exif_off = ifd0_off + ifd0_len;
    let ifd1_off = exif_off + exif_ifd_len;
    let data_off = ifd1_off + ifd1_len;
    let make_off = data_off;
    let model_off = make_off + make.len() as u32;
    let dt_off = model_off + model.len() as u32;
    let thumb_off = dt_off + dt.len() as u32;

    let mut v = b"II*\0".to_vec();
    v.extend_from_slice(&ifd0_off.to_le_bytes());
    let entry = |v: &mut Vec<u8>, tag: u16, ty: u16, count: u32, value: u32| {
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&ty.to_le_bytes());
        v.extend_from_slice(&count.to_le_bytes());
        v.extend_from_slice(&value.to_le_bytes());
    };
    // IFD0
    v.extend_from_slice(&ifd0_entries.to_le_bytes());
    entry(&mut v, 0x010F, 2, make.len() as u32, make_off);
    entry(&mut v, 0x0110, 2, model.len() as u32, model_off);
    if datetime.is_some() {
        entry(&mut v, 0x8769, 4, 1, exif_off);
    }
    v.extend_from_slice(&(if thumbnail.is_some() { ifd1_off } else { 0 }).to_le_bytes());
    // Exif IFD
    if datetime.is_some() {
        v.extend_from_slice(&1u16.to_le_bytes());
        entry(&mut v, 0x9003, 2, dt.len() as u32, dt_off);
        v.extend_from_slice(&0u32.to_le_bytes());
    }
    // IFD1
    if let Some(t) = thumbnail {
        v.extend_from_slice(&2u16.to_le_bytes());
        entry(&mut v, 0x0201, 4, 1, thumb_off);
        entry(&mut v, 0x0202, 4, 1, t.len() as u32);
        v.extend_from_slice(&0u32.to_le_bytes());
    }
    assert_eq!(v.len() as u32, data_off);
    v.extend_from_slice(make);
    v.extend_from_slice(model);
    v.extend_from_slice(&dt);
    if let Some(t) = thumbnail {
        v.extend_from_slice(t);
    }
    v
}

pub fn png(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let img = picture(w, h, seed);
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
        .expect("png encode");
    out
}

pub fn webp_lossless(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let img = picture(w, h, seed);
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
        .expect("webp encode");
    out
}

fn bmff_box(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(ty);
    v.extend_from_slice(body);
    v
}

/// A structurally valid HEIF container: `ftyp`, `meta` (hdlr, iloc,
/// iprp/ipco/ispe) and `mdat`. The mdat payload is random bytes — this exercises the carver's
/// box walking, not HEVC decoding.
pub fn heif_like(w: u32, h: u32, payload: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut ftyp = b"heic".to_vec();
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(b"mif1heic");
    let ftyp = bmff_box(b"ftyp", &ftyp);

    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(b"pict");
    hdlr.extend_from_slice(&[0u8; 13]);
    let hdlr = bmff_box(b"hdlr", &hdlr);

    let mut ispe = vec![0u8; 4];
    ispe.extend_from_slice(&w.to_be_bytes());
    ispe.extend_from_slice(&h.to_be_bytes());
    let ipco = bmff_box(b"ipco", &bmff_box(b"ispe", &ispe));
    let iprp = bmff_box(b"iprp", &ipco);

    // iloc v0, offset_size=4, length_size=4, base_offset_size=0, 1 item, 1 extent.
    let build_meta = |mdat_data_off: u32| {
        let mut iloc = vec![0u8, 0, 0, 0, 0x44, 0x00];
        iloc.extend_from_slice(&1u16.to_be_bytes());
        iloc.extend_from_slice(&1u16.to_be_bytes()); // item id
        iloc.extend_from_slice(&0u16.to_be_bytes()); // data ref
        iloc.extend_from_slice(&1u16.to_be_bytes()); // extents
        iloc.extend_from_slice(&mdat_data_off.to_be_bytes());
        iloc.extend_from_slice(&(payload as u32).to_be_bytes());
        let mut meta = vec![0u8; 4];
        meta.extend_from_slice(&hdlr);
        meta.extend_from_slice(&bmff_box(b"iloc", &iloc));
        meta.extend_from_slice(&iprp);
        bmff_box(b"meta", &meta)
    };
    let meta_len = build_meta(0).len();
    let data_off = (ftyp.len() + meta_len + 8) as u32;
    let meta = build_meta(data_off);
    let mut out = ftyp;
    out.extend_from_slice(&meta);
    out.extend_from_slice(&bmff_box(b"mdat", &rng.bytes(payload)));
    out
}
