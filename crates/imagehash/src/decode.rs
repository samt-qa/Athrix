//! Reading an image file and hashing it.

use std::fs::File;
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use image::imageops::FilterType;
use image::{DynamicImage, ImageError, ImageFormat, ImageReader, Limits};

use crate::ImageHash;
use crate::exif::jpeg_thumbnail;
use crate::phash::{SIDE, phash};

/// Files larger than this are not hashed ([`HashError::TooLarge`]).
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Lower-case extensions (without the dot) of the files `similar:` reads. HEIC/HEIF and AVIF
/// are missing on purpose: no pure-Rust decoder.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "webp", "gif", "bmp", "tif", "tiff",
];

/// Whether `ext` (with or without its dot, any case) is one of [`IMAGE_EXTENSIONS`].
pub fn is_image_extension(ext: &str) -> bool {
    let ext = ext.strip_prefix('.').unwrap_or(ext);
    IMAGE_EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(ext))
}

/// JPEGs at least this big are hashed from their EXIF thumbnail when they have a usable one.
const EXIF_THUMBNAIL_MIN_BYTES: u64 = 256 * 1024;

/// Bytes read from the start of a JPEG to find its EXIF block (an APP1 segment is at most
/// 64 KiB and comes first or second).
const EXIF_PREFIX_BYTES: u64 = 160 * 1024;

/// Widest or tallest image decoded.
const MAX_DIMENSION: u32 = 24_000;

/// Most memory one decode may allocate.
const MAX_DECODE_ALLOC: u64 = 384 * 1024 * 1024;

/// Smallest side of an EXIF thumbnail worth using (they are usually 160×120).
const MIN_THUMBNAIL_SIDE: u32 = 64;

/// Largest relative difference between the aspect ratios of the thumbnail and the picture:
/// cameras letterbox some thumbnails, and black bars would change the hash.
const MAX_ASPECT_DIFFERENCE: f64 = 0.03;

/// Why an image could not be hashed.
#[derive(Debug, thiserror::Error)]
pub enum HashError {
    /// Reading the file failed.
    #[error("cannot read the image: {0}")]
    Io(#[from] io::Error),
    /// The file is bigger than [`MAX_FILE_BYTES`], or its picture bigger than the decode
    /// limits.
    #[error("the image is too large to compare")]
    TooLarge,
    /// Not an image format this crate decodes (HEIC, AVIF, PSD, not an image at all...).
    #[error("unsupported image format")]
    Unsupported,
    /// The decoder rejected the data (a truncated or corrupt file).
    #[error("cannot decode the image: {0}")]
    Decode(String),
}

/// Hashes the image file at `path` (see the crate docs). Files over [`MAX_FILE_BYTES`] are
/// rejected without being read.
pub fn hash_file(path: &Path) -> Result<ImageHash, HashError> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len > MAX_FILE_BYTES {
        return Err(HashError::TooLarge);
    }
    let mut reader = BufReader::new(file);
    if len >= EXIF_THUMBNAIL_MIN_BYTES {
        let mut prefix = Vec::new();
        (&mut reader)
            .take(EXIF_PREFIX_BYTES)
            .read_to_end(&mut prefix)?;
        if let Some(hash) = exif_thumbnail_hash(&prefix) {
            return Ok(hash);
        }
        reader.seek(SeekFrom::Start(0))?;
    }
    let image = ImageReader::new(reader).with_guessed_format()?;
    decode_and_hash(image)
}

/// Hashes an image held in memory (any size: [`MAX_FILE_BYTES`] applies to files only).
pub fn hash_bytes(bytes: &[u8]) -> Result<ImageHash, HashError> {
    if bytes.len() as u64 >= EXIF_THUMBNAIL_MIN_BYTES {
        if let Some(hash) = exif_thumbnail_hash(bytes) {
            return Ok(hash);
        }
    }
    let image = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    decode_and_hash(image)
}

/// Hashes a decoded image: 32×32 grey levels (transparent pixels count as white), then
/// the DCT pHash (see the crate docs).
pub fn hash_image(image: &DynamicImage) -> ImageHash {
    let side = SIDE as u32;
    let small = if image.width() >= side && image.height() >= side {
        // A fast area average: fine (and much quicker) for shrinking.
        image.thumbnail_exact(side, side)
    } else {
        image.resize_exact(side, side, FilterType::Triangle)
    };
    let rgba = small.to_rgba8();
    let mut grey = [0.0f32; SIDE * SIDE];
    for (out, px) in grey.iter_mut().zip(rgba.pixels()) {
        let [r, g, b, a] = px.0.map(f32::from);
        let luma = 0.299 * r + 0.587 * g + 0.114 * b;
        let alpha = a / 255.0;
        *out = luma * alpha + 255.0 * (1.0 - alpha);
    }
    phash(&grey)
}

/// Decodes with limits and hashes; a decoder panic (malformed data) is a [`HashError::Decode`].
fn decode_and_hash<R: io::BufRead + Seek>(
    mut reader: ImageReader<R>,
) -> Result<ImageHash, HashError> {
    if reader.format().is_none() {
        return Err(HashError::Unsupported);
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let decoded = catch_unwind(AssertUnwindSafe(move || reader.decode()))
        .map_err(|_| HashError::Decode("the decoder failed".to_owned()))?;
    let image = decoded.map_err(map_error)?;
    Ok(hash_image(&image))
}

fn map_error(e: ImageError) -> HashError {
    match e {
        ImageError::IoError(e) => HashError::Io(e),
        ImageError::Unsupported(_) => HashError::Unsupported,
        ImageError::Limits(_) => HashError::TooLarge,
        other => HashError::Decode(other.to_string()),
    }
}

/// The hash of the EXIF thumbnail of the JPEG starting with `prefix`, when it has one of the
/// picture's shape. `None` means "decode the whole picture instead".
fn exif_thumbnail_hash(prefix: &[u8]) -> Option<ImageHash> {
    let thumb_bytes = jpeg_thumbnail(prefix)?;
    // The picture's size, from its frame header (no pixels decoded).
    let (w, h) = ImageReader::with_format(Cursor::new(prefix), ImageFormat::Jpeg)
        .into_dimensions()
        .ok()?;
    let thumb =
        catch_unwind(|| image::load_from_memory_with_format(thumb_bytes, ImageFormat::Jpeg))
            .ok()?
            .ok()?;
    let (tw, th) = (thumb.width(), thumb.height());
    if tw.min(th) < MIN_THUMBNAIL_SIDE || w == 0 || h == 0 {
        return None;
    }
    let aspect = f64::from(w) / f64::from(h);
    let thumb_aspect = f64::from(tw) / f64::from(th);
    if ((aspect - thumb_aspect) / aspect).abs() > MAX_ASPECT_DIFFERENCE {
        return None;
    }
    Some(hash_image(&thumb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exif::with_exif_thumbnail;
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    /// A smooth test picture: a few blobs and gradients, `seed` choosing the layout.
    fn picture(w: u32, h: u32, seed: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let s = seed as f32;
            let v = ((fx * (3.0 + s) + fy * (2.0 + s * 0.7)).sin()
                + (fx * fy * (5.0 + s * 1.3)).cos()
                + ((fx - 0.3 * s.sin()).powi(2) + (fy - 0.5).powi(2)).sqrt() * 2.0)
                * 60.0
                + 128.0;
            let v = v.clamp(0.0, 255.0) as u8;
            Rgb([v, v.saturating_add(20), 255 - v])
        })
    }

    fn encode(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn jpeg(image: &DynamicImage, quality: u8) -> Vec<u8> {
        let mut out = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        image.to_rgb8().write_with_encoder(encoder).unwrap();
        out
    }

    #[test]
    fn resized_and_reencoded_copies_are_close() {
        let original = DynamicImage::ImageRgb8(picture(640, 480, 1));
        let h = hash_bytes(&encode(&original, ImageFormat::Png)).unwrap();
        let copies = [
            jpeg(&original, 90),
            jpeg(&original.resize_exact(320, 240, FilterType::Lanczos3), 60),
            encode(
                &original.resize_exact(1000, 750, FilterType::CatmullRom),
                ImageFormat::Png,
            ),
            encode(&original, ImageFormat::Bmp),
            encode(
                &original.resize_exact(200, 150, FilterType::Triangle),
                ImageFormat::Gif,
            ),
            encode(&original, ImageFormat::Tiff),
            encode(&original, ImageFormat::WebP),
        ];
        for (i, bytes) in copies.iter().enumerate() {
            let d = hash_bytes(bytes).unwrap().distance(h);
            assert!(d <= 4, "copy {i} is {d} bits away");
        }
    }

    #[test]
    fn different_pictures_are_far() {
        let a = hash_image(&DynamicImage::ImageRgb8(picture(400, 300, 1)));
        for seed in 2..6 {
            let b = hash_image(&DynamicImage::ImageRgb8(picture(400, 300, seed)));
            let d = a.distance(b);
            assert!(
                d > crate::GROUP_MAX_DISTANCE * 2,
                "seed {seed}: only {d} bits"
            );
        }
    }

    #[test]
    fn transparent_pixels_count_as_white() {
        let clear = RgbaImage::from_fn(64, 64, |x, _| {
            if x < 32 {
                Rgba([0, 0, 0, 0])
            } else {
                Rgba([0, 0, 0, 255])
            }
        });
        let white = RgbaImage::from_fn(64, 64, |x, _| {
            if x < 32 {
                Rgba([255, 255, 255, 255])
            } else {
                Rgba([0, 0, 0, 255])
            }
        });
        assert_eq!(
            hash_image(&DynamicImage::ImageRgba8(clear)),
            hash_image(&DynamicImage::ImageRgba8(white))
        );
    }

    #[test]
    fn tiny_images_are_upscaled() {
        let tiny = DynamicImage::ImageRgb8(picture(12, 9, 3));
        let big =
            DynamicImage::ImageRgb8(picture(12, 9, 3)).resize_exact(120, 90, FilterType::Nearest);
        assert!(hash_image(&tiny).distance(hash_image(&big)) <= 6);
    }

    #[test]
    fn corrupt_and_foreign_data_are_errors_not_panics() {
        let png = encode(
            &DynamicImage::ImageRgb8(picture(64, 64, 1)),
            ImageFormat::Png,
        );
        assert!(matches!(
            hash_bytes(&png[..png.len() / 2]),
            Err(HashError::Decode(_) | HashError::Io(_))
        ));
        assert!(matches!(
            hash_bytes(b"hello, world"),
            Err(HashError::Unsupported)
        ));
        assert!(matches!(hash_bytes(&[]), Err(HashError::Unsupported)));
        let jpg = jpeg(&DynamicImage::ImageRgb8(picture(64, 64, 1)), 80);
        let mut garbled = jpg.clone();
        for b in garbled.iter_mut().skip(200).step_by(7) {
            *b = b.wrapping_mul(31).wrapping_add(7);
        }
        // Garbled entropy data may still decode (to noise); it must just not panic.
        let _ = hash_bytes(&garbled);
        assert!(hash_bytes(&jpg[..40]).is_err());
    }

    #[test]
    fn files_are_hashed_and_bad_files_reported() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("a.png");
        let image = DynamicImage::ImageRgb8(picture(300, 200, 2));
        std::fs::write(&good, encode(&image, ImageFormat::Png)).unwrap();
        assert_eq!(hash_file(&good).unwrap(), hash_image(&image));
        let bad = dir.path().join("b.jpg");
        std::fs::write(&bad, b"\xFF\xD8\xFF\xE0 broken").unwrap();
        assert!(hash_file(&bad).is_err());
        assert!(matches!(
            hash_file(&dir.path().join("missing.png")),
            Err(HashError::Io(_))
        ));
    }

    #[test]
    fn big_jpegs_use_a_matching_exif_thumbnail() {
        let original = DynamicImage::ImageRgb8(picture(1200, 900, 4));
        let main = jpeg(&original, 98);
        let thumb = jpeg(&original.resize_exact(160, 120, FilterType::Triangle), 85);
        let mut with_thumb = with_exif_thumbnail(&main, &thumb);
        // Pad with comment segments (after the EXIF block) to pass the size threshold.
        let comment = [&[0xFF, 0xFE, 0xFF, 0xF0][..], &[b' '; 0xFFEE][..]].concat();
        while (with_thumb.len() as u64) < EXIF_THUMBNAIL_MIN_BYTES {
            let at = with_thumb.len() - main.len() + 2;
            with_thumb.splice(at..at, comment.iter().copied());
        }
        let thumb_hash = exif_thumbnail_hash(&with_thumb).expect("thumbnail used");
        assert!(thumb_hash.distance(hash_image(&original)) <= 4);
        assert_eq!(hash_bytes(&with_thumb).unwrap(), thumb_hash);

        // A letterboxed (4:3 picture, 16:9 thumbnail) one is ignored.
        let wide = jpeg(&original.resize_exact(160, 90, FilterType::Triangle), 85);
        assert_eq!(
            exif_thumbnail_hash(&with_exif_thumbnail(&main, &wide)),
            None
        );
    }

    #[test]
    fn image_extensions() {
        assert!(is_image_extension("JPG"));
        assert!(is_image_extension(".webp"));
        assert!(!is_image_extension("heic"));
        assert!(!is_image_extension("pdf"));
    }
}
