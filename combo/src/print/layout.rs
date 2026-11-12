//! Page layout in physical units. Preview projects the finished printer rectangles.

use windows::Win32::Foundation::RECT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SizeMode {
    Original,
    Fit,
    Stretch,
    Custom,
}

impl SizeMode {
    pub const ALL: [Self; 4] = [Self::Original, Self::Fit, Self::Stretch, Self::Custom];
}

#[derive(Clone, Copy)]
pub(super) struct Options {
    /// Distances from the physical edges of the paper, in 1/100 mm.
    pub margins: RECT,
    pub copies: u32,
    pub mode: SizeMode,
    pub size_mm: (f64, f64),
    pub crop: bool,
    pub auto_rotate: bool,
    pub alignment: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Geometry {
    pub physical: (i32, i32),
    pub offset: (i32, i32),
    pub printable: (i32, i32),
    pub dpi: (i32, i32),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Placement {
    pub image: RECT,
    pub clip: RECT,
    pub rotate: bool,
}

pub(super) fn parse_mm(text: &str, min: f64, max: f64) -> Option<f64> {
    let value = text.trim().replace(',', ".").parse::<f64>().ok()?;
    (value.is_finite() && (min..=max).contains(&value)).then_some(value)
}

pub(super) fn intersect(a: RECT, b: RECT) -> RECT {
    RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    }
}

pub(super) fn nonempty(rect: RECT) -> bool {
    rect.right > rect.left && rect.bottom > rect.top
}

impl Geometry {
    pub fn valid(self) -> bool {
        self.physical.0 > 0
            && self.physical.1 > 0
            && self.printable.0 > 0
            && self.printable.1 > 0
            && self.dpi.0 > 0
            && self.dpi.1 > 0
            && self.offset.0 >= 0
            && self.offset.1 >= 0
    }

    pub fn paper_mm(self) -> (f64, f64) {
        (
            self.physical.0 as f64 * 25.4 / self.dpi.0 as f64,
            self.physical.1 as f64 * 25.4 / self.dpi.1 as f64,
        )
    }

    pub fn area(self, margins: RECT) -> Option<RECT> {
        if !self.valid() {
            return None;
        }
        let px = |value: i32, dpi: i32| value.max(0) as i64 * dpi as i64 / 2540;
        let x = |v: i64| v.clamp(0, self.printable.0 as i64) as i32;
        let y = |v: i64| v.clamp(0, self.printable.1 as i64) as i32;
        let area = RECT {
            left: x(px(margins.left, self.dpi.0) - self.offset.0 as i64),
            top: y(px(margins.top, self.dpi.1) - self.offset.1 as i64),
            right: x(self.physical.0 as i64 - px(margins.right, self.dpi.0) - self.offset.0 as i64),
            bottom: y(self.physical.1 as i64
                - px(margins.bottom, self.dpi.1)
                - self.offset.1 as i64),
        };
        nonempty(area).then_some(area)
    }

    pub fn layout(self, image: (u32, u32), opt: Options) -> Option<Placement> {
        if image.0 == 0 || image.1 == 0 || opt.alignment > 8 {
            return None;
        }
        let area = self.area(opt.margins)?;
        let aw = (area.right - area.left) as f64 * 25.4 / self.dpi.0 as f64;
        let ah = (area.bottom - area.top) as f64 * 25.4 / self.dpi.1 as f64;
        let custom = opt.mode == SizeMode::Custom;
        let (fw, fh) = if custom {
            let (w, h) = opt.size_mm;
            if !w.is_finite()
                || !h.is_finite()
                || !(1.0..=1000.0).contains(&w)
                || !(1.0..=1000.0).contains(&h)
            {
                return None;
            }
            (w, h)
        } else {
            (aw, ah)
        };
        let (iw, ih) = (image.0 as f64, image.1 as f64);
        let scale = |w: f64, h: f64| (fw / w).min(fh / h);
        let rotate = opt.auto_rotate && scale(ih, iw) > scale(iw, ih) + 1e-9;
        let (iw, ih) = if rotate { (ih, iw) } else { (iw, ih) };
        let (w, h) = match opt.mode {
            SizeMode::Original => (iw * 25.4 / self.dpi.0 as f64, ih * 25.4 / self.dpi.1 as f64),
            SizeMode::Stretch => (aw, ah),
            _ => {
                let (sx, sy) = (fw / iw, fh / ih);
                let s = if opt.crop { sx.max(sy) } else { sx.min(sy) };
                (iw * s, ih * s)
            }
        };
        let align_x = (opt.alignment % 3) as f64 / 2.0;
        let align_y = (opt.alignment / 3) as f64 / 2.0;
        let (x, y) = if custom {
            (
                (aw - fw) * align_x + (fw - w) / 2.0,
                (ah - fh) * align_y + (fh - h) / 2.0,
            )
        } else {
            ((aw - w) * align_x, (ah - h) * align_y)
        };
        // Float intermediates also keep extreme image aspect ratios out of integer arithmetic.
        // Reject coordinates outside GDI's signed range instead of saturating or wrapping.
        let rect = |x: f64, y: f64, w: f64, h: f64| -> Option<RECT> {
            let cvt = |mm: f64, dpi: i32, origin: i32| {
                let v = mm * dpi as f64 / 25.4 + origin as f64;
                (v.is_finite() && (i32::MIN as f64 + 1.0..=i32::MAX as f64 - 1.0).contains(&v))
                    .then(|| v.round() as i32)
            };
            let result = RECT {
                left: cvt(x, self.dpi.0, area.left)?,
                top: cvt(y, self.dpi.1, area.top)?,
                right: cvt(x + w, self.dpi.0, area.left)?,
                bottom: cvt(y + h, self.dpi.1, area.top)?,
            };
            // StretchDIBits also takes signed widths, not only signed coordinates.
            (nonempty(result)
                && result.right as i64 - result.left as i64 <= i32::MAX as i64
                && result.bottom as i64 - result.top as i64 <= i32::MAX as i64)
                .then_some(result)
        };
        let image = rect(x, y, w, h)?;
        let clip = if custom {
            intersect(
                area,
                rect((aw - fw) * align_x, (ah - fh) * align_y, fw, fh)?,
            )
        } else {
            area
        };
        nonempty(intersect(image, clip)).then_some(Placement {
            image,
            clip,
            rotate,
        })
    }

    pub fn project(self, rect: RECT, paper: RECT) -> RECT {
        let x = |v: i32| {
            (paper.left as f64
                + (v as i64 + self.offset.0 as i64) as f64
                    * (paper.right as i64 - paper.left as i64) as f64
                    / self.physical.0 as f64)
                .round() as i32
        };
        let y = |v: i32| {
            (paper.top as f64
                + (v as i64 + self.offset.1 as i64) as f64
                    * (paper.bottom as i64 - paper.top as i64) as f64
                    / self.physical.1 as f64)
                .round() as i32
        };
        RECT {
            left: x(rect.left),
            top: y(rect.top),
            right: x(rect.right),
            bottom: y(rect.bottom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn paper(dpi: (i32, i32)) -> Geometry {
        Geometry {
            physical: (
                (210.0 / 25.4 * dpi.0 as f64) as i32,
                (297.0 / 25.4 * dpi.1 as f64) as i32,
            ),
            printable: (
                (210.0 / 25.4 * dpi.0 as f64) as i32,
                (297.0 / 25.4 * dpi.1 as f64) as i32,
            ),
            offset: (0, 0),
            dpi,
        }
    }
    fn options() -> Options {
        Options {
            margins: RECT {
                left: 1000,
                top: 1000,
                right: 1000,
                bottom: 1000,
            },
            copies: 1,
            mode: SizeMode::Fit,
            size_mm: (100.0, 150.0),
            crop: false,
            auto_rotate: false,
            alignment: 4,
        }
    }
    #[test]
    fn dimensions_are_strict_and_accept_both_decimal_separators() {
        assert_eq!(parse_mm("12,5", 0.0, 300.0), Some(12.5));
        assert_eq!(parse_mm(" 12.50 ", 0.0, 300.0), Some(12.5));
        for s in [
            "",
            "nan",
            "NaN",
            "inf",
            "-1",
            "1001",
            "999999999999",
            "1\u{a0}000",
        ] {
            assert!(parse_mm(s, 1.0, 1000.0).is_none(), "{s}");
        }
        assert_eq!(parse_mm("1000", 1.0, 1000.0), Some(1000.0));
    }
    #[test]
    fn square_stays_square_at_asymmetric_printer_dpi() {
        for dpi in [(600, 300), (300, 600), (600, 600)] {
            let p = paper(dpi).layout((2000, 2000), options()).unwrap();
            let w = (p.image.right - p.image.left) as f64 / dpi.0 as f64;
            let h = (p.image.bottom - p.image.top) as f64 / dpi.1 as f64;
            assert!((w - h).abs() < 0.01);
        }
    }
    #[test]
    fn rotation_uses_physical_frame_and_leaves_square_frames_alone() {
        let mut opt = options();
        opt.auto_rotate = true;
        assert!(!paper((600, 300)).layout((1000, 1500), opt).unwrap().rotate);
        assert!(paper((600, 300)).layout((1500, 1000), opt).unwrap().rotate);
        opt.mode = SizeMode::Custom;
        opt.size_mm = (150.0, 100.0);
        assert!(paper((600, 300)).layout((1000, 1500), opt).unwrap().rotate);
        opt.size_mm = (100.0, 100.0);
        assert!(!paper((600, 300)).layout((1000, 1500), opt).unwrap().rotate);
    }
    #[test]
    fn custom_crop_is_clipped_to_the_requested_frame() {
        let g = paper((600, 300));
        let mut opt = options();
        opt.mode = SizeMode::Custom;
        opt.crop = true;
        let p = g.layout((1500, 1000), opt).unwrap();
        assert!(((p.clip.right - p.clip.left) as f64 / g.dpi.0 as f64 * 25.4 - 100.0).abs() < 0.1);
        assert!(((p.clip.bottom - p.clip.top) as f64 / g.dpi.1 as f64 * 25.4 - 150.0).abs() < 0.1);
        assert!(p.image.left < p.clip.left && p.image.right > p.clip.right);
    }
    #[test]
    fn margins_account_for_unprintable_edges_and_empty_pages() {
        let mut g = paper((254, 254));
        g.offset = (50, 50);
        g.printable = (2000, 2870);
        let r = g.area(options().margins).unwrap();
        assert_eq!((r.left, r.top, r.right, r.bottom), (50, 50, 1950, 2820));
        let mut opt = options();
        opt.margins.left = 30000;
        assert!(g.layout((1000, 1000), opt).is_none());
    }
    #[test]
    fn every_anchor_places_the_custom_frame_against_the_correct_page_edge() {
        let g = paper((254, 254));
        let mut opt = options();
        opt.mode = SizeMode::Custom;
        opt.crop = true;
        for alignment in 0..9 {
            opt.alignment = alignment;
            let p = g.layout((1000, 1500), opt).unwrap();
            assert_eq!(p.clip.left, 100 + 900 * (alignment % 3) as i32 / 2);
            assert_eq!(p.clip.top, 100 + 1270 * (alignment / 3) as i32 / 2);
            assert_eq!(
                (p.clip.right - p.clip.left, p.clip.bottom - p.clip.top),
                (1000, 1500)
            );
        }
    }
    #[test]
    fn preview_applies_the_hardware_origin_once() {
        let mut g = paper((254, 254));
        g.offset = (50, 50);
        g.printable = (2000, 2870);
        let sheet = RECT {
            left: 0,
            top: 0,
            right: 210,
            bottom: 297,
        };
        let r = g.project(g.area(options().margins).unwrap(), sheet);
        assert_eq!((r.left, r.top, r.right, r.bottom), (10, 10, 200, 287));
    }
    #[test]
    fn preview_projects_the_exact_printer_placement_for_every_mode_and_anchor() {
        let g = paper((600, 300));
        let sheet = RECT {
            left: 20,
            top: 20,
            right: 420,
            bottom: 586,
        };
        for mode in SizeMode::ALL {
            for alignment in 0..9 {
                let mut opt = options();
                opt.mode = mode;
                opt.alignment = alignment;
                let p = g.layout((900, 600), opt).unwrap();
                let preview = g.project(p.image, sheet);
                let screen_w = (preview.right - preview.left) as f64;
                let screen_h = (preview.bottom - preview.top) as f64;
                assert!(screen_w > 0.0 && screen_h > 0.0);
                if mode == SizeMode::Fit {
                    assert!((screen_w / screen_h - 1.5).abs() < 0.02);
                }
            }
        }
    }
    #[test]
    fn excessive_sizes_and_extreme_aspect_ratios_never_overflow() {
        let g = paper((600, 300));
        let mut opt = options();
        opt.mode = SizeMode::Custom;
        opt.crop = true;
        for alignment in 0..9 {
            opt.alignment = alignment;
            opt.size_mm = (999999999999.0, 150.0);
            assert!(g.layout((2000, 2000), opt).is_none());
            opt.size_mm = (1000.0, 1000.0);
            let _ = g.layout((u32::MAX, 1), opt);
        }
    }
}
