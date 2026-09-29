//! Turning PDF pages into pixels, and pixels back into QR text.

use hayro::hayro_syntax::page::Page;
use image::{GrayImage, ImageBuffer};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub fn render_page_gray(page: &Page, dpi: u32) -> GrayImage {
    let scale = (dpi as f32) / 72.0;
    let settings = hayro::RenderSettings {
        x_scale: scale,
        y_scale: scale,
        bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        ..Default::default()
    };
    let interp = hayro::hayro_interpret::InterpreterSettings::default();
    let pixmap = hayro::render(page, &interp, &settings);
    pixmap_to_gray(&pixmap)
}

pub fn pixmap_to_gray(pixmap: &hayro::vello_cpu::Pixmap) -> GrayImage {
    let w = pixmap.width() as u32;
    let h = pixmap.height() as u32;
    let rgba = pixmap.data_as_u8_slice();

    let mut buf = Vec::with_capacity((w as usize) * (h as usize));
    for px in rgba.chunks_exact(4) {
        let r = px[0] as u32;
        let g = px[1] as u32;
        let b = px[2] as u32;
        buf.push(((r * 2126 + g * 7152 + b * 722) / 10000) as u8);
    }
    ImageBuffer::from_raw(w, h, buf).unwrap_or_else(|| GrayImage::new(w, h))
}

/// Otsu threshold binarization.
pub fn binarize_otsu(gray: &GrayImage) -> GrayImage {
    let mut hist = [0u32; 256];
    for p in gray.pixels() {
        hist[p[0] as usize] += 1;
    }
    let total: u32 = gray.width() * gray.height();
    if total == 0 {
        return gray.clone();
    }

    let mut sum = 0u64;
    for (i, &h) in hist.iter().enumerate() {
        sum += (i as u64) * (h as u64);
    }

    let mut sum_b = 0u64;
    let mut w_b = 0u32;
    let mut var_max = 0f64;
    let mut threshold = 128u8;

    for t in 0..256 {
        w_b += hist[t];
        if w_b == 0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0 {
            break;
        }
        sum_b += (t as u64) * (hist[t] as u64);

        let m_b = (sum_b as f64) / (w_b as f64);
        let m_f = ((sum - sum_b) as f64) / (w_f as f64);

        let var_between = (w_b as f64) * (w_f as f64) * (m_b - m_f) * (m_b - m_f);
        if var_between > var_max {
            var_max = var_between;
            threshold = t as u8;
        }
    }

    let mut out: GrayImage = ImageBuffer::new(gray.width(), gray.height());
    for (dst, src) in out.pixels_mut().zip(gray.pixels()) {
        dst[0] = if src[0] <= threshold { 0u8 } else { 255u8 };
    }
    out
}

pub fn invert_gray(img: &GrayImage) -> GrayImage {
    let mut out = img.clone();
    for p in out.pixels_mut() {
        p[0] = 255u8 - p[0];
    }
    out
}

/// Cheap rejection of blank cells, which saves time and avoids rqrr edge cases.
pub fn is_low_contrast(img: &GrayImage) -> bool {
    let mut minv = 255u8;
    let mut maxv = 0u8;
    // sample a subset for speed
    let step_x = (img.width() / 32).max(1);
    let step_y = (img.height() / 32).max(1);
    for y in (0..img.height()).step_by(step_y as usize) {
        for x in (0..img.width()).step_by(step_x as usize) {
            let v = img.get_pixel(x, y)[0];
            minv = minv.min(v);
            maxv = maxv.max(v);
            if maxv.saturating_sub(minv) > 40 {
                return false;
            }
        }
    }
    true
}

/// Clamp a floating-point crop rectangle to the image, returning `(x, y, w, h)`.
pub fn clamp_crop(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    img_w: f32,
    img_h: f32,
) -> (u32, u32, u32, u32) {
    let x0 = x0.max(0.0).min(img_w);
    let y0 = y0.max(0.0).min(img_h);
    let x1 = x1.max(0.0).min(img_w);
    let y1 = y1.max(0.0).min(img_h);

    let w = (x1 - x0).max(0.0) as u32;
    let h = (y1 - y0).max(0.0) as u32;

    (x0 as u32, y0 as u32, w, h)
}

/// Decode a single QR from an image.
///
/// rqrr panics internally on some inputs. A restore must never abort because one cell
/// out of hundreds was awkward, so every call is wrapped.
pub fn decode_single_qr(gray: GrayImage) -> Option<String> {
    catch_unwind(AssertUnwindSafe(|| {
        let mut prep = rqrr::PreparedImage::prepare(gray);
        for g in prep.detect_grids() {
            if let Ok((_meta, text)) = g.decode() {
                return Some(text);
            }
        }
        None
    }))
    .ok()
    .flatten()
}

/// A decoded code together with where it sat in the image.
#[derive(Debug, Clone)]
pub struct Located {
    pub text: String,
    /// The code's four corners, in the detector's order.
    ///
    /// These carry the code's orientation as well as its size, which is what lets a
    /// scan be walked code by code instead of searched blindly.
    pub corners: [(f32, f32); 4],
    /// Modules along one edge of this code, from its QR version.
    pub modules: usize,
}

impl Located {
    pub fn center(&self) -> (f32, f32) {
        let sx: f32 = self.corners.iter().map(|c| c.0).sum();
        let sy: f32 = self.corners.iter().map(|c| c.1).sum();
        (sx / 4.0, sy / 4.0)
    }

    /// Axis-aligned bounds as `(x0, y0, x1, y1)`.
    pub fn bbox(&self) -> (f32, f32, f32, f32) {
        let (mut x0, mut y0) = (f32::MAX, f32::MAX);
        let (mut x1, mut y1) = (f32::MIN, f32::MIN);
        for &(x, y) in &self.corners {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        (x0, y0, x1, y1)
    }

    /// The two edge vectors from the first corner: the code's own axes, whatever way
    /// round the sheet was fed into the scanner.
    pub fn axes(&self) -> [(f32, f32); 2] {
        let c = self.corners;
        [
            (c[1].0 - c[0].0, c[1].1 - c[0].1),
            (c[3].0 - c[0].0, c[3].1 - c[0].1),
        ]
    }

    /// How many pixels one module measures here.
    ///
    /// This is the number that decides whether a scan is usable at all: it is measured
    /// from the code itself, so it needs no assumption about the scanner.
    pub fn module_px(&self) -> Option<f32> {
        (self.modules > 0).then(|| self.size() / self.modules as f32)
    }

    /// Edge length in pixels.
    pub fn size(&self) -> f32 {
        let [a, b] = self.axes();
        let la = (a.0 * a.0 + a.1 * a.1).sqrt();
        let lb = (b.0 * b.0 + b.1 * b.1).sqrt();
        (la + lb) / 2.0
    }

    fn shifted(mut self, dx: f32, dy: f32) -> Self {
        for c in &mut self.corners {
            c.0 += dx;
            c.1 += dy;
        }
        self
    }

    /// Move into sheet coordinates from a tile cropped at `(dx, dy)`.
    pub fn in_sheet_coords(self, dx: u32, dy: u32) -> Self {
        self.shifted(dx as f32, dy as f32)
    }
}

/// Decode every QR rqrr can find in an image. Panic-safe, see [`decode_single_qr`].
pub fn decode_all_qr(gray: GrayImage) -> Vec<String> {
    decode_all_qr_located(gray)
        .into_iter()
        .map(|l| l.text)
        .collect()
}

/// Decode every QR and report where each one was found.
///
/// The positions let a tiled sweep skip over ground it has already read, which is the
/// difference between a scan taking seconds and taking minutes.
pub fn decode_all_qr_located(gray: GrayImage) -> Vec<Located> {
    catch_unwind(AssertUnwindSafe(|| {
        let mut out = Vec::new();
        let mut prep = rqrr::PreparedImage::prepare(gray);
        for g in prep.detect_grids() {
            if let Ok((meta, text)) = g.decode() {
                let c = g.bounds;
                out.push(Located {
                    text,
                    corners: [
                        (c[0].x as f32, c[0].y as f32),
                        (c[1].x as f32, c[1].y as f32),
                        (c[2].x as f32, c[2].y as f32),
                        (c[3].x as f32, c[3].y as f32),
                    ],
                    // A QR of version v is 17 + 4v modules on a side.
                    modules: 17 + 4 * meta.version.0,
                });
            }
        }
        out
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Luma;

    #[test]
    fn otsu_splits_a_bimodal_image() {
        let mut img = GrayImage::new(4, 2);
        for (i, p) in img.pixels_mut().enumerate() {
            p[0] = if i % 2 == 0 { 10 } else { 240 };
        }
        let bin = binarize_otsu(&img);
        let values: Vec<u8> = bin.pixels().map(|p| p[0]).collect();
        assert_eq!(values, vec![0, 255, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn inversion_is_its_own_inverse() {
        let mut img = GrayImage::new(3, 1);
        img.pixels_mut().zip([0u8, 127, 255]).for_each(|(p, v)| p[0] = v);
        assert_eq!(invert_gray(&invert_gray(&img)), img);
    }

    #[test]
    fn blank_cells_are_detected_as_low_contrast() {
        let blank = GrayImage::from_pixel(64, 64, Luma([255]));
        assert!(is_low_contrast(&blank));

        // Blocks, not a checkerboard: a 1px checkerboard aliases with the sampling
        // stride. Real QR modules span many pixels at scan resolution.
        let mut mixed = GrayImage::from_pixel(64, 64, Luma([255]));
        for (_, y, p) in mixed.enumerate_pixels_mut() {
            if y < 32 {
                p[0] = 0;
            }
        }
        assert!(!is_low_contrast(&mixed));
    }

    #[test]
    fn crops_are_clamped_into_the_image() {
        assert_eq!(clamp_crop(-10.0, -10.0, 50.0, 50.0, 100.0, 100.0), (0, 0, 50, 50));
        assert_eq!(clamp_crop(80.0, 80.0, 200.0, 200.0, 100.0, 100.0), (80, 80, 20, 20));
        // Fully outside: an empty crop, not a panic.
        assert_eq!(clamp_crop(200.0, 200.0, 300.0, 300.0, 100.0, 100.0), (100, 100, 0, 0));
    }

    #[test]
    fn decoding_noise_yields_nothing_and_does_not_panic() {
        let mut img = GrayImage::new(80, 80);
        for (i, p) in img.pixels_mut().enumerate() {
            p[0] = ((i * 37) % 256) as u8;
        }
        assert!(decode_all_qr(img.clone()).is_empty());
        assert_eq!(decode_single_qr(img), None);
    }
}
