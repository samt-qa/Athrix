//! Finding the JPEG thumbnail embedded in a JPEG's EXIF block (IFD1), without a full EXIF
//! parser: a few bounds-checked reads of the APP1 segment.

/// The embedded thumbnail of the JPEG `bytes` (a whole JPEG file of its own), if its EXIF block
/// has one. Only the segments before the image data are looked at.
pub(crate) fn jpeg_thumbnail(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut pos = 2;
    loop {
        // Markers may be padded with extra 0xFF bytes.
        while *bytes.get(pos)? == 0xFF && *bytes.get(pos + 1)? == 0xFF {
            pos += 1;
        }
        if *bytes.get(pos)? != 0xFF {
            return None;
        }
        let marker = *bytes.get(pos + 1)?;
        // Start of scan or end of image: no EXIF before the image data.
        if marker == 0xDA || marker == 0xD9 {
            return None;
        }
        let len = usize::from(u16::from_be_bytes([
            *bytes.get(pos + 2)?,
            *bytes.get(pos + 3)?,
        ]));
        if len < 2 {
            return None;
        }
        let segment = bytes.get(pos + 4..pos + 2 + len)?;
        if marker == 0xE1 {
            if let Some(tiff) = segment.strip_prefix(b"Exif\0\0") {
                if let Some(thumb) = tiff_thumbnail(tiff) {
                    return Some(thumb);
                }
            }
        }
        pos += 2 + len;
    }
}

/// Byte order of a TIFF block.
#[derive(Clone, Copy)]
enum Order {
    Little,
    Big,
}

impl Order {
    fn u16(self, b: &[u8], at: usize) -> Option<u16> {
        let v = [*b.get(at)?, *b.get(at + 1)?];
        Some(match self {
            Order::Little => u16::from_le_bytes(v),
            Order::Big => u16::from_be_bytes(v),
        })
    }

    fn u32(self, b: &[u8], at: usize) -> Option<u32> {
        let v: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
        Some(match self {
            Order::Little => u32::from_le_bytes(v),
            Order::Big => u32::from_be_bytes(v),
        })
    }
}

/// Tag of IFD1 giving the thumbnail's offset (from the TIFF header).
const TAG_THUMB_OFFSET: u16 = 0x0201;
/// Tag of IFD1 giving the thumbnail's length in bytes.
const TAG_THUMB_LENGTH: u16 = 0x0202;

/// The thumbnail of IFD1 of the TIFF block `tiff` (the EXIF payload after `Exif\0\0`).
fn tiff_thumbnail(tiff: &[u8]) -> Option<&[u8]> {
    let order = match tiff.get(..2)? {
        b"II" => Order::Little,
        b"MM" => Order::Big,
        _ => return None,
    };
    if order.u16(tiff, 2)? != 42 {
        return None;
    }
    let ifd0 = usize::try_from(order.u32(tiff, 4)?).ok()?;
    let ifd0_count = usize::from(order.u16(tiff, ifd0)?);
    let ifd1 = usize::try_from(order.u32(tiff, ifd0 + 2 + ifd0_count * 12)?).ok()?;
    if ifd1 == 0 {
        return None;
    }
    let count = usize::from(order.u16(tiff, ifd1)?);
    let (mut offset, mut length) = (None, None);
    for i in 0..count {
        let entry = ifd1 + 2 + i * 12;
        let value = order.u32(tiff, entry + 8)?;
        match order.u16(tiff, entry)? {
            TAG_THUMB_OFFSET => offset = Some(value),
            TAG_THUMB_LENGTH => length = Some(value),
            _ => {}
        }
    }
    let start = usize::try_from(offset?).ok()?;
    let end = start.checked_add(usize::try_from(length?).ok()?)?;
    let thumb = tiff.get(start..end)?;
    thumb.starts_with(&[0xFF, 0xD8]).then_some(thumb)
}

/// Builds a JPEG whose APP1 EXIF block embeds `thumb` as its IFD1 thumbnail, followed by
/// `main` (a whole JPEG, whose SOI marker is dropped). For tests.
#[cfg(test)]
pub(crate) fn with_exif_thumbnail(main: &[u8], thumb: &[u8]) -> Vec<u8> {
    with_exif(main, None, Some(thumb))
}

/// Builds a JPEG whose APP1 EXIF block has the IFD0 orientation tag (0x0112) set to
/// `orientation` (1–8) and, when given, the IFD1 thumbnail `thumb`, followed by `main`. For
/// tests.
#[cfg(test)]
pub(crate) fn with_exif(main: &[u8], orientation: Option<u16>, thumb: Option<&[u8]>) -> Vec<u8> {
    const TAG_ORIENTATION: u16 = 0x0112;
    // Little-endian TIFF: header, IFD0 (orientation) pointing to IFD1 (offset and length).
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at 8
    let ifd0_count = u16::from(orientation.is_some());
    tiff.extend_from_slice(&ifd0_count.to_le_bytes());
    if let Some(o) = orientation {
        tiff.extend_from_slice(&TAG_ORIENTATION.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&o.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes());
    }
    let ifd1 = 8 + 2 + 12 * usize::from(ifd0_count) + 4;
    let next = if thumb.is_some() { ifd1 as u32 } else { 0 };
    tiff.extend_from_slice(&next.to_le_bytes());
    if let Some(thumb) = thumb {
        let thumb_at = ifd1 + 2 + 2 * 12 + 4;
        tiff.extend_from_slice(&2u16.to_le_bytes());
        for (tag, value) in [
            (TAG_THUMB_OFFSET, thumb_at as u32),
            (TAG_THUMB_LENGTH, thumb.len() as u32),
        ] {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&4u16.to_le_bytes()); // LONG
            tiff.extend_from_slice(&1u32.to_le_bytes());
            tiff.extend_from_slice(&value.to_le_bytes());
        }
        tiff.extend_from_slice(&0u32.to_le_bytes()); // no IFD2
        tiff.extend_from_slice(thumb);
    }

    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    let len = u16::try_from(2 + 6 + tiff.len()).expect("thumbnail too big for a test");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(&tiff);
    out.extend_from_slice(&main[2..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_ifd1_thumbnail() {
        let thumb = [0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9];
        let main = [0xFF, 0xD8, 0xFF, 0xDA, 0, 2, 0xFF, 0xD9];
        let jpeg = with_exif_thumbnail(&main, &thumb);
        assert_eq!(jpeg_thumbnail(&jpeg), Some(&thumb[..]));
    }

    #[test]
    fn garbage_and_truncated_input_give_none() {
        assert_eq!(jpeg_thumbnail(b""), None);
        assert_eq!(jpeg_thumbnail(b"not a jpeg"), None);
        let thumb = [0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9];
        let main = [0xFF, 0xD8, 0xFF, 0xDA, 0, 2, 0xFF, 0xD9];
        let jpeg = with_exif_thumbnail(&main, &thumb);
        for cut in 0..jpeg.len() {
            // Never panics; a cut inside the EXIF block finds nothing.
            let _ = jpeg_thumbnail(&jpeg[..cut]);
        }
        let mut broken = jpeg.clone();
        broken[10] = b'X'; // not "II"/"MM"
        assert_eq!(jpeg_thumbnail(&broken), None);
    }
}
