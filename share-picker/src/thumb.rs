//! Shrinking a captured monitor to a thumbnail, on the CPU.

/// A box-filter downscale of a little-endian ARGB/XRGB8888 image (bytes
/// B, G, R, A) to at most `max_width` wide, as opaque RGBA for tiny-skia.
/// Returns (width, height, pixels).
pub fn downscale(
    src: &[u8],
    (w, h): (usize, usize),
    stride: usize,
    max_width: usize,
) -> Option<(u32, u32, Vec<u8>)> {
    if w == 0 || h == 0 || stride < w * 4 || src.len() < stride * (h - 1) + w * 4 {
        return None;
    }
    let dw = w.min(max_width.max(1));
    let dh = (h * dw / w).max(1);
    let mut out = vec![255u8; dw * dh * 4];
    for dy in 0..dh {
        let (y0, y1) = (dy * h / dh, ((dy + 1) * h / dh).max(dy * h / dh + 1));
        for dx in 0..dw {
            let (x0, x1) = (dx * w / dw, ((dx + 1) * w / dw).max(dx * w / dw + 1));
            let (mut b, mut g, mut r) = (0u32, 0u32, 0u32);
            for y in y0..y1.min(h) {
                for px in src[y * stride + x0 * 4..y * stride + x1.min(w) * 4]
                    .as_chunks::<4>()
                    .0
                {
                    b += px[0] as u32;
                    g += px[1] as u32;
                    r += px[2] as u32;
                }
            }
            let n = ((y1.min(h) - y0) * (x1.min(w) - x0)) as u32;
            let i = (dy * dw + dx) * 4;
            out[i] = (r / n) as u8;
            out[i + 1] = (g / n) as u8;
            out[i + 2] = (b / n) as u8;
        }
    }
    Some((dw as u32, dh as u32, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_blocks_and_swaps_to_rgba() {
        // 4x2, left half pure red (BGRA), right half pure blue.
        let red = [0, 0, 255, 255];
        let blue = [255, 0, 0, 255];
        let mut src = Vec::new();
        for _ in 0..2 {
            src.extend_from_slice(&[red, red, blue, blue].concat());
        }
        let (w, h, px) = downscale(&src, (4, 2), 16, 2).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(px, vec![255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn small_images_are_kept_and_bad_input_refused() {
        let src = vec![10u8; 3 * 3 * 4];
        assert_eq!(downscale(&src, (3, 3), 12, 100).unwrap().0, 3);
        assert!(downscale(&src, (3, 3), 12, 100).is_some());
        assert!(downscale(&src[..10], (3, 3), 12, 100).is_none());
        assert!(downscale(&src, (0, 3), 12, 100).is_none());
    }
}
