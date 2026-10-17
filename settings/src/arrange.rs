//! The Displays page's monitor arrangement: dragging one screen around the
//! others. Screens always end up touching another along an edge, never
//! overlapping, with edges lining up when they come close.

/// A monitor's place in the global layout, logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Screen {
    fn overlaps(&self, o: &Screen) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }
}

/// How much of an edge two neighbours must share, so the pointer can cross.
const MIN_SHARED: i32 = 100;

/// Where a `w`×`h` screen dragged to `want` (its top-left) lands: the
/// nearest spot touching one of `others` without overlapping any. Within
/// `snap` px, its edges or centre line up with the neighbour's.
pub fn place(others: &[Screen], w: i32, h: i32, want: (i32, i32), snap: i32) -> (i32, i32) {
    if others.is_empty() {
        return (0, 0);
    }
    // Slide along a neighbour's side: clamped so enough edge is shared,
    // then lined up with its start, end or centre if close.
    let slide = |want: i32, start: i32, len: i32, own: i32| {
        let shared = MIN_SHARED.min(len).min(own);
        let v = want.clamp(start - own + shared, start + len - shared);
        [start, start + len - own, start + (len - own) / 2]
            .into_iter()
            .filter(|t| (t - v).abs() <= snap)
            .min_by_key(|t| (t - v).abs())
            .unwrap_or(v)
    };
    let mut best: Option<((i32, i32), i64)> = None;
    for o in others {
        let beside_y = slide(want.1, o.y, o.h, h);
        let beside_x = slide(want.0, o.x, o.w, w);
        let candidates = [
            (o.x + o.w, beside_y),
            (o.x - w, beside_y),
            (beside_x, o.y + o.h),
            (beside_x, o.y - h),
        ];
        for (x, y) in candidates {
            let me = Screen { x, y, w, h };
            if others.iter().any(|o| me.overlaps(o)) {
                continue;
            }
            let (dx, dy) = ((x - want.0) as i64, (y - want.1) as i64);
            let distance = dx * dx + dy * dy;
            if best.is_none_or(|(_, d)| distance < d) {
                best = Some(((x, y), distance));
            }
        }
    }
    best.map(|(p, _)| p).unwrap_or_else(|| {
        // Boxed in (three or more screens): right of everything.
        let right = others.iter().map(|o| o.x + o.w).max().unwrap_or(0);
        (right, 0)
    })
}

/// Shift so the layout's top-left is (0, 0).
pub fn normalize(positions: &mut [(i32, i32)]) {
    let min_x = positions.iter().map(|p| p.0).min().unwrap_or(0);
    let min_y = positions.iter().map(|p| p.1).min().unwrap_or(0);
    for p in positions {
        p.0 -= min_x;
        p.1 -= min_y;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIG: Screen = Screen {
        x: 0,
        y: 0,
        w: 2560,
        h: 1440,
    };

    #[test]
    fn snaps_beside_and_lines_up() {
        // Dropped a little right of and below the big one's right edge: it
        // sits against it, tops lined up.
        assert_eq!(place(&[BIG], 1920, 1080, (2600, 20), 40), (2560, 0));
        // Further down it stays where it was dropped (a vertical offset).
        assert_eq!(place(&[BIG], 1920, 1080, (2600, 250), 40), (2560, 250));
        // Bottoms line up.
        assert_eq!(place(&[BIG], 1920, 1080, (2600, 350), 40), (2560, 360));
        // On the left.
        assert_eq!(place(&[BIG], 1920, 1080, (-2000, 0), 40), (-1920, 0));
        // Above.
        assert_eq!(place(&[BIG], 1920, 1080, (500, -1200), 40), (500, -1080));
    }

    #[test]
    fn keeps_an_edge_shared() {
        // Dragged far below: it can't slide off the bottom corner.
        assert_eq!(place(&[BIG], 1920, 1080, (2560, 5000), 0), (2460, 1440));
        let (x, y) = place(&[BIG], 1920, 1080, (3000, 1400), 0);
        let me = Screen {
            x,
            y,
            w: 1920,
            h: 1080,
        };
        assert!(!me.overlaps(&BIG));
        assert!(x == 2560 || y == 1440);
    }

    #[test]
    fn never_overlaps() {
        let others = [
            BIG,
            Screen {
                x: 2560,
                y: 0,
                w: 1920,
                h: 1080,
            },
        ];
        for want in [(1000, 500), (2600, 100), (-50, 1500), (4000, 2000)] {
            let (x, y) = place(&others, 1280, 1024, want, 20);
            let me = Screen {
                x,
                y,
                w: 1280,
                h: 1024,
            };
            assert!(
                others.iter().all(|o| !me.overlaps(o)),
                "{want:?} -> {x},{y}"
            );
        }
    }

    #[test]
    fn normalizes_to_origin() {
        let mut p = [(-1920, 200), (0, 0)];
        normalize(&mut p);
        assert_eq!(p, [(0, 200), (1920, 0)]);
    }
}
