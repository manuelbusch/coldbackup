//! Page geometry.
//!
//! Encoder and decoder both derive cell positions from here. When the two sides
//! computed the grid independently they could drift apart silently — and a decoder
//! that crops one cell off is indistinguishable from a damaged sheet.

use anyhow::{Result, bail};

pub const PT_PER_MM: f32 = 72.0 / 25.4;

/// A4 in mm for the given orientation.
pub fn a4_mm(landscape: bool) -> (f32, f32) {
    if landscape {
        (297.0, 210.0)
    } else {
        (210.0, 297.0)
    }
}

/// A4 orientation guessed from a rendered page, used when no manifest is available.
pub fn a4_from_image_dims(width: u32, height: u32) -> (f32, f32) {
    a4_mm(width > height)
}

/// Everything needed to place or find a QR cell on a sheet.
///
/// Cell 0 always holds the manifest, cells 1.. hold data chunks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SheetLayout {
    pub page_w_mm: f32,
    pub page_h_mm: f32,
    pub qr_mm: f32,
    pub margin_mm: f32,
    pub gap_mm: f32,
    pub cols: i32,
    pub rows: i32,
}

impl SheetLayout {
    /// Derive the largest grid that fits on the page.
    pub fn fit(
        page_w_mm: f32,
        page_h_mm: f32,
        qr_mm: f32,
        margin_mm: f32,
        gap_mm: f32,
    ) -> Result<Self> {
        if !(qr_mm > 0.0) {
            bail!("qr_mm must be > 0");
        }
        if margin_mm < 0.0 || gap_mm < 0.0 {
            bail!("margin_mm and gap_mm must be >= 0");
        }

        let usable_w_mm = page_w_mm - 2.0 * margin_mm;
        let usable_h_mm = page_h_mm - 2.0 * margin_mm;
        if usable_w_mm <= 0.0 || usable_h_mm <= 0.0 {
            bail!("margins too large for page");
        }

        // The trailing gap after the last cell is not needed, hence the `+ gap_mm`.
        let cell_mm = qr_mm + gap_mm;
        let cols = ((usable_w_mm + gap_mm) / cell_mm).floor() as i32;
        let rows = ((usable_h_mm + gap_mm) / cell_mm).floor() as i32;
        if cols <= 0 || rows <= 0 {
            bail!("qr_mm/margins/gap do not fit any QR on the page");
        }

        Ok(Self {
            page_w_mm,
            page_h_mm,
            qr_mm,
            margin_mm,
            gap_mm,
            cols,
            rows,
        })
    }

    /// Rebuild a layout recorded in a manifest. Returns `None` for values that could
    /// not describe a real sheet, so a garbled manifest cannot poison the decoder.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        page_w_mm: f32,
        page_h_mm: f32,
        qr_mm: f32,
        margin_mm: f32,
        gap_mm: f32,
        cols: i32,
        rows: i32,
    ) -> Option<Self> {
        if !(page_w_mm > 0.0 && page_h_mm > 0.0 && qr_mm > 0.0) {
            return None;
        }
        if margin_mm < 0.0 || gap_mm < 0.0 || cols <= 0 || rows <= 0 {
            return None;
        }
        Some(Self {
            page_w_mm,
            page_h_mm,
            qr_mm,
            margin_mm,
            gap_mm,
            cols,
            rows,
        })
    }

    pub fn cell_mm(&self) -> f32 {
        self.qr_mm + self.gap_mm
    }

    /// Cells per sheet, including the manifest cell.
    pub fn per_page(&self) -> usize {
        (self.cols as usize) * (self.rows as usize)
    }

    /// Cells per sheet available for data (cell 0 is reserved for the manifest).
    pub fn data_per_page(&self) -> usize {
        self.per_page().saturating_sub(1)
    }

    /// Top-left corner of a cell in mm, measured from the top-left of the page.
    pub fn cell_origin_mm(&self, cell_index: usize) -> (f32, f32) {
        let cell = self.cell_mm();
        let r = (cell_index as i32) / self.cols;
        let c = (cell_index as i32) % self.cols;
        (
            self.margin_mm + (c as f32) * cell,
            self.margin_mm + (r as f32) * cell,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_a4_grid_is_5x7() {
        let (w, h) = a4_mm(false);
        let l = SheetLayout::fit(w, h, 35.0, 8.0, 2.0).unwrap();
        assert_eq!((l.cols, l.rows), (5, 7));
        assert_eq!(l.per_page(), 35);
        assert_eq!(l.data_per_page(), 34);
    }

    #[test]
    fn cells_advance_by_qr_plus_gap() {
        let l = SheetLayout::fit(210.0, 297.0, 35.0, 8.0, 2.0).unwrap();
        assert_eq!(l.cell_origin_mm(0), (8.0, 8.0));
        assert_eq!(l.cell_origin_mm(1), (8.0 + 37.0, 8.0));
        // Wraps to the next row after `cols` cells.
        assert_eq!(l.cell_origin_mm(5), (8.0, 8.0 + 37.0));
    }

    #[test]
    fn impossible_geometry_is_rejected() {
        assert!(SheetLayout::fit(210.0, 297.0, 0.0, 8.0, 2.0).is_err());
        assert!(SheetLayout::fit(210.0, 297.0, 35.0, 150.0, 2.0).is_err());
        assert!(SheetLayout::fit(210.0, 297.0, 400.0, 8.0, 2.0).is_err());
        assert!(SheetLayout::from_parts(210.0, 297.0, 35.0, 8.0, 2.0, 0, 7).is_none());
        assert!(SheetLayout::from_parts(0.0, 297.0, 35.0, 8.0, 2.0, 5, 7).is_none());
    }

    #[test]
    fn orientation_follows_page_shape() {
        assert_eq!(a4_from_image_dims(2000, 1000), (297.0, 210.0));
        assert_eq!(a4_from_image_dims(1000, 2000), (210.0, 297.0));
        // Degenerate input must not panic; portrait is the safe default.
        assert_eq!(a4_from_image_dims(0, 0), (210.0, 297.0));
    }
}
