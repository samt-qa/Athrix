//! The DCT perceptual hash of a 32×32 grey thumbnail.

use std::f64::consts::PI;
use std::sync::LazyLock;

use crate::ImageHash;

/// Side of the grey thumbnail the DCT runs on.
pub(crate) const SIDE: usize = 32;

/// Side of the block of lowest frequencies kept (8×8 = 64 bits).
const LOW: usize = 8;

/// `COS[k][n] = cos(π/32 · (n + ½) · k)`: the DCT-II basis, only the 8 lowest frequencies.
static COS: LazyLock<[[f32; SIDE]; LOW]> = LazyLock::new(|| {
    let mut table = [[0.0; SIDE]; LOW];
    for (k, row) in table.iter_mut().enumerate() {
        for (n, cell) in row.iter_mut().enumerate() {
            let angle = PI / SIDE as f64 * (n as f64 + 0.5) * k as f64;
            *cell = angle.cos() as f32;
        }
    }
    table
});

/// pHash of a 32×32 grey image (row-major, any brightness scale): the 8×8 lowest DCT
/// coefficients, each bit set when the coefficient is above the median of the 63 AC ones.
pub(crate) fn phash(pixels: &[f32; SIDE * SIDE]) -> ImageHash {
    // 1-D DCT of every row, low frequencies only: rows[r][k].
    let mut rows = [[0.0f32; LOW]; SIDE];
    for (row_px, out) in pixels.chunks_exact(SIDE).zip(rows.iter_mut()) {
        for (coef, basis) in out.iter_mut().zip(COS.iter()) {
            *coef = row_px.iter().zip(basis).map(|(p, c)| p * c).sum();
        }
    }
    // Then down every column: coefs[u * 8 + v] = Σ_r rows[r][v] · cos_u(r).
    let mut coefs = [0.0f32; LOW * LOW];
    for (u, basis) in COS.iter().enumerate() {
        for v in 0..LOW {
            coefs[u * LOW + v] = rows.iter().zip(basis).map(|(row, c)| row[v] * c).sum();
        }
    }
    // The DC term (overall brightness) is left out of the median: it would skew it.
    let mut ac: Vec<f32> = coefs[1..].to_vec();
    ac.sort_unstable_by(f32::total_cmp);
    let median = ac[ac.len() / 2];
    let bits = coefs
        .iter()
        .enumerate()
        .filter(|(_, c)| **c > median)
        .fold(0u64, |h, (i, _)| h | (1u64 << i));
    ImageHash(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(f: impl Fn(usize, usize) -> f32) -> [f32; SIDE * SIDE] {
        let mut px = [0.0; SIDE * SIDE];
        for (i, p) in px.iter_mut().enumerate() {
            *p = f(i / SIDE, i % SIDE);
        }
        px
    }

    #[test]
    fn brightness_and_contrast_do_not_change_the_hash() {
        let base = pattern(|r, c| ((r * 7 + c * 3) % 23) as f32);
        let brighter = pattern(|r, c| ((r * 7 + c * 3) % 23) as f32 * 1.5 + 40.0);
        assert_eq!(phash(&base), phash(&brighter));
    }

    #[test]
    fn different_patterns_are_far_apart() {
        let stripes = pattern(|_, c| if c < 16 { 0.0 } else { 255.0 });
        let bands = pattern(|r, _| if r < 16 { 0.0 } else { 255.0 });
        assert!(phash(&stripes).distance(phash(&bands)) > 16);
    }
}
