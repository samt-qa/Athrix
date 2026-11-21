//! Bounds-checked little-endian readers over untrusted byte slices.

#[inline]
pub(crate) fn array<const N: usize>(b: &[u8], off: usize) -> Option<[u8; N]> {
    let end = off.checked_add(N)?;
    b.get(off..end)?.try_into().ok()
}

#[inline]
pub(crate) fn u8_at(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}

#[inline]
pub(crate) fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    array(b, off).map(u16::from_le_bytes)
}

#[inline]
pub(crate) fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    array(b, off).map(u32::from_le_bytes)
}

#[inline]
pub(crate) fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    array(b, off).map(u64::from_le_bytes)
}

#[inline]
pub(crate) fn i64_at(b: &[u8], off: usize) -> Option<i64> {
    array(b, off).map(i64::from_le_bytes)
}

/// Decodes UTF-16LE bytes (odd trailing byte ignored); unpaired surrogates become U+FFFD.
pub(crate) fn utf16le_lossy(b: &[u8]) -> String {
    let units = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]));
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_are_bounds_checked() {
        let b = [1u8, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(u16_at(&b, 0), Some(1));
        assert_eq!(u64_at(&b, 0), Some(1));
        assert_eq!(u64_at(&b, 1), None);
        assert_eq!(u32_at(&b, usize::MAX - 1), None);
        assert_eq!(u8_at(&b, 8), None);
    }

    #[test]
    fn utf16_lossy() {
        let s: Vec<u8> = "héllo".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(utf16le_lossy(&s), "héllo");
        assert_eq!(utf16le_lossy(&[0x00, 0xD8]), "\u{FFFD}");
    }
}
