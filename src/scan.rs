//! Reading sheets back from scans and photographs.
//!
//! The PDF path in [`crate::decode`] knows exactly where every code sits and can crop
//! cells by the millimetre. A scan offers no such promise: it is rotated a degree or
//! two, shifted, scaled slightly wrong, lit unevenly and softened by JPEG. Cropping by
//! computed geometry would slice codes in half.
//!
//! So nothing here uses page geometry to *locate* a code. Detection is left to rqrr,
//! which handles rotation and perspective on its own, and the job of this module is to
//! hand it images it can cope with: the whole sheet first, then overlapping tiles,
//! because a detector given a mosaic of thirty-five codes at once finds only a few.

use image::{GrayImage, imageops};
use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::format::{TAG_MANIFEST, parse_manifest_payload};
use crate::imaging::{Located, binarize_otsu, decode_all_qr_located, invert_gray};

/// File extensions treated as scanned sheets.
const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "tif", "tiff", "bmp", "webp", "gif"];

#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    /// Also try the inverted image, for negatives and badly exposed photographs.
    pub try_invert: bool,
    /// Upper bound on tiles actually decoded per sheet, so a huge scan cannot take
    /// unbounded time. Tiles skipped over already-read codes do not count.
    pub max_tiles: usize,
    /// Tile size as a multiple of one code's estimated width. Must stay above 1.0, and
    /// the closer to it, the better each tile isolates a single code.
    pub tile_factor: f32,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            try_invert: true,
            max_tiles: 800,
            // Barely wider than one code. Anything roomier pulls in the neighbouring
            // codes — they sit only a couple of millimetres apart — and a detector
            // handed several at once reads almost none of them.
            tile_factor: 1.15,
        }
    }
}

pub fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Every image in a directory, in name order, so numbered sheets line up.
pub fn images_in_dir(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_image_path(p))
        .collect();
    out.sort();
    Ok(out)
}

pub fn load_sheet(path: &Path) -> anyhow::Result<GrayImage> {
    let img = image::open(path)
        .map_err(|e| anyhow::anyhow!("read image '{}': {e}", path.display()))?;
    Ok(img.to_luma8())
}

/// Everything read off one sheet so far, and where on the sheet it sat.
#[derive(Default)]
struct Harvest {
    texts: BTreeSet<String>,
    found: Vec<Located>,
    probes: usize,
}

impl Harvest {
    /// Record decodes from a tile cropped at `(dx, dy)`. Returns the indices of codes
    /// whose text had not been seen before, which are the ones worth growing from.
    fn absorb(&mut self, found: Vec<Located>, dx: u32, dy: u32) -> Vec<usize> {
        let mut fresh = Vec::new();
        for located in found {
            if !self.texts.insert(located.text.clone()) {
                continue;
            }
            self.found.push(located.in_sheet_coords(dx, dy));
            fresh.push(self.found.len() - 1);
        }
        fresh
    }

    /// True if this point sits on a code that has already been read.
    fn already_read(&self, x: f32, y: f32) -> bool {
        self.found.iter().any(|f| {
            let (x0, y0, x1, y1) = f.bbox();
            x >= x0 && x <= x1 && y >= y0 && y <= y1
        })
    }
}

/// What one sheet gave up, and how well it was resolved.
pub struct SheetScan {
    pub texts: Vec<String>,
    /// Median pixels per module across the codes read here. Below roughly 4 a scan is
    /// too coarse for the rest of the codes, however hard it is searched.
    pub module_px: Option<f32>,
    /// Scan resolution in dpi, if the manifest named the sheet size.
    pub effective_dpi: Option<f32>,
}

/// Decode every coldbackup payload visible on one scanned sheet.
pub fn decode_sheet(gray: &GrayImage, opts: &ScanOptions) -> Vec<String> {
    scan_sheet(gray, opts).texts
}

/// As [`decode_sheet`], but also reporting how well the sheet was resolved.
pub fn scan_sheet(gray: &GrayImage, opts: &ScanOptions) -> SheetScan {
    let mut harvest = Harvest::default();

    // 1. Seed. The whole sheet at once, first as grayscale — rqrr does its own local
    //    thresholding, which copes with a lighting gradient far better than a global
    //    threshold would — then binarized as a second opinion. On a dense sheet this
    //    finds only a code or two, but one seed is all step 2 needs.
    harvest.absorb(decode_all_qr_located(gray.clone()), 0, 0);
    if harvest.texts.is_empty() {
        harvest.absorb(decode_all_qr_located(binarize_otsu(gray)), 0, 0);
    }

    grow(gray, opts, &mut harvest);

    // 3. Inverted, for negatives and badly exposed photographs.
    if opts.try_invert && harvest.texts.is_empty() {
        let inverted = invert_gray(gray);
        harvest.absorb(decode_all_qr_located(inverted.clone()), 0, 0);
        grow(&inverted, opts, &mut harvest);
    }

    let mut modules: Vec<f32> = harvest.found.iter().filter_map(|f| f.module_px()).collect();
    modules.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let module_px = modules.get(modules.len() / 2).copied();

    let effective_dpi = manifest_layout(&harvest.texts)
        .map(|l| (gray.width() as f32) / l.page_w_mm * 25.4);

    SheetScan {
        texts: harvest.texts.into_iter().collect(),
        module_px,
        effective_dpi,
    }
}

/// Fill in the sheet from whatever has been found, falling back to a blind sweep only
/// when there is nothing to grow from.
fn grow(gray: &GrayImage, opts: &ScanOptions, harvest: &mut Harvest) {
    let pitch = pitch_ratio(&harvest.texts).unwrap_or(DEFAULT_PITCH_RATIO);
    flood_fill(gray, opts, harvest, pitch);

    let expected = expected_per_sheet(&harvest.texts);
    if expected.is_none_or(|n| harvest.texts.len() < n) {
        sweep(gray, opts, harvest);
        // Fresh seeds may open ground the fill could not reach before.
        flood_fill(gray, opts, harvest, pitch_ratio(&harvest.texts).unwrap_or(pitch));
    }
}

/// Walk the sheet code by code.
///
/// Every decoded code reports its own four corners, so it states both how large a code
/// is here and which way the sheet is turned — a neighbour therefore sits one cell pitch
/// along the code's own axes, wherever the page happens to lie. Predicting each next
/// code and looking there costs one crop per neighbour, where sweeping the sheet blind
/// costs thousands and, on a dense sheet, runs out of budget before it is done.
fn flood_fill(gray: &GrayImage, opts: &ScanOptions, harvest: &mut Harvest, pitch_ratio: f32) {
    let mut queue: VecDeque<usize> = (0..harvest.found.len()).collect();

    while let Some(i) = queue.pop_front() {
        if harvest.probes >= opts.max_tiles {
            return;
        }

        let source = harvest.found[i].clone();
        let (cx, cy) = source.center();
        let tile = (source.size() * opts.tile_factor).max(32.0) as u32;

        for (ax, ay) in source.axes() {
            for sign in [1.0f32, -1.0] {
                let (tx, ty) = (
                    cx + ax * pitch_ratio * sign,
                    cy + ay * pitch_ratio * sign,
                );
                if tx < 0.0 || ty < 0.0 || tx >= gray.width() as f32 || ty >= gray.height() as f32 {
                    continue;
                }
                if harvest.already_read(tx, ty) {
                    continue;
                }
                if harvest.probes >= opts.max_tiles {
                    return;
                }
                harvest.probes += 1;

                let (crop, ox, oy) = crop_centered(gray, tx, ty, tile);
                for fresh in harvest.absorb(decode_all_qr_located(crop), ox, oy) {
                    queue.push_back(fresh);
                }
            }
        }
    }
}

fn crop_centered(gray: &GrayImage, cx: f32, cy: f32, tile: u32) -> (GrayImage, u32, u32) {
    let half = tile as f32 / 2.0;
    let x = (cx - half).max(0.0).min((gray.width().saturating_sub(tile)) as f32) as u32;
    let y = (cy - half).max(0.0).min((gray.height().saturating_sub(tile)) as f32) as u32;
    let w = tile.min(gray.width());
    let h = tile.min(gray.height());
    (imageops::crop_imm(gray, x, y, w, h).to_image(), x, y)
}

/// Distance between neighbouring codes, as a multiple of one code's width.
///
/// Codes are laid out on a `qr_mm + gap_mm` pitch, so this is a property of the sheet,
/// not of the scan, and it survives any rotation or scaling in between.
fn pitch_ratio(texts: &BTreeSet<String>) -> Option<f32> {
    let layout = manifest_layout(texts)?;
    let ratio = layout.cell_mm() / layout.qr_mm;
    ratio.is_finite().then_some(ratio)
}

/// Codes expected on a full sheet, so the fill knows when it is done.
fn expected_per_sheet(texts: &BTreeSet<String>) -> Option<usize> {
    Some(manifest_layout(texts)?.per_page())
}

fn manifest_layout(texts: &BTreeSet<String>) -> Option<crate::layout::SheetLayout> {
    texts
        .iter()
        .filter(|s| s.starts_with(TAG_MANIFEST))
        .find_map(|s| parse_manifest_payload(s).ok())?
        .layout
}

// -------------------- Blind fallback --------------------

/// Last resort when nothing has been decoded yet, or the fill stalled: sweep the sheet
/// with overlapping tiles. Expensive, and normally only a handful of tiles run before a
/// seed turns up and the fill takes over.
fn sweep(gray: &GrayImage, opts: &ScanOptions, harvest: &mut Harvest) {
    let expected = expected_per_sheet(&harvest.texts);
    for (tile, step) in tile_plan(gray, &harvest.texts, opts) {
        sweep_tiles(gray, tile, step, opts, harvest, expected);
        if expected.is_some_and(|n| harvest.texts.len() >= n) {
            return;
        }
    }
}

/// Tile sizes and their step, as `(tile, step)` pairs.
///
/// The step is what makes the sweep exhaustive. A code of width `q` only ever sits
/// wholly inside a tile of width `t` if some tile starts within `t - q` of it, so the
/// step must not exceed `t - q` — stepping by half the tile, the obvious choice, quietly
/// misses codes whenever `t < 2q`.
///
/// Only half that slack is actually spent, because `q` is what a code measures when it
/// is square to the page. A scan never is: at one degree of skew a code's axis-aligned
/// footprint already grows by about 1.7%, and the estimate from the manifest carries its
/// own error on top. Taking the full slack looks right and silently drops the codes that
/// straddle every tile boundary.
const STEP_SAFETY: f32 = 0.5;

/// Fallback pitch when no manifest has been read: the default 35 mm code on a 2 mm gap.
const DEFAULT_PITCH_RATIO: f32 = 37.0 / 35.0;

fn tile_plan(gray: &GrayImage, found: &BTreeSet<String>, opts: &ScanOptions) -> Vec<(u32, u32)> {
    let factor = opts.tile_factor.max(1.1);
    let step_for = |tile: f32, qr: f32| (((tile - qr) * STEP_SAFETY).max(1.0)) as u32;

    if let Some(qr_px) = estimated_code_width_px(gray, found) {
        let tile = qr_px * factor;
        if tile >= 32.0 && tile <= gray.width().max(gray.height()) as f32 {
            return vec![(tile as u32, step_for(tile, qr_px))];
        }
    }

    // Nothing decoded yet: guess across a small ladder of code sizes, assuming each
    // tile holds roughly one code.
    let short_side = gray.width().min(gray.height());
    [2u32, 3, 5, 8]
        .iter()
        .map(|d| (short_side / d) as f32)
        .filter(|t| *t >= 32.0)
        .map(|t| (t as u32, step_for(t, t / factor)))
        .collect()
}

fn estimated_code_width_px(gray: &GrayImage, found: &BTreeSet<String>) -> Option<f32> {
    let layout = manifest_layout(found)?;

    // The scan may be cropped or padded relative to the sheet, so this is deliberately
    // an approximation; tiles only need to be roughly the right size.
    let by_width = (gray.width() as f32) * layout.qr_mm / layout.page_w_mm;
    let by_height = (gray.height() as f32) * layout.qr_mm / layout.page_h_mm;
    let est = (by_width + by_height) / 2.0;
    (est.is_finite() && est >= 8.0).then_some(est)
}

fn sweep_tiles(
    gray: &GrayImage,
    tile_px: u32,
    step_px: u32,
    opts: &ScanOptions,
    harvest: &mut Harvest,
    expected: Option<usize>,
) {
    let (w, h) = (gray.width(), gray.height());
    let tile = tile_px.min(w).min(h).max(32);
    let step = step_px.clamp(1, tile);
    let half = tile as f32 / 2.0;

    for y in offsets(h, tile, step) {
        for x in offsets(w, tile, step) {
            // A tile centred on a code that has already been read can only find that
            // same code again.
            if harvest.already_read(x as f32 + half, y as f32 + half) {
                continue;
            }
            if harvest.probes >= opts.max_tiles {
                return;
            }
            harvest.probes += 1;

            let crop = imageops::crop_imm(gray, x, y, tile, tile).to_image();
            let fresh = harvest.absorb(decode_all_qr_located(crop), x, y);

            // A seed is worth far more than the next blind tile: grow from it at once.
            if !fresh.is_empty() {
                let pitch = pitch_ratio(&harvest.texts).unwrap_or(DEFAULT_PITCH_RATIO);
                flood_fill(gray, opts, harvest, pitch);
                if expected.is_some_and(|n| harvest.texts.len() >= n) {
                    return;
                }
            }
        }
    }
}

/// Tile origins along one axis, always including one flush with the far edge so no
/// strip of the sheet is left uncovered.
fn offsets(extent: u32, tile: u32, step: u32) -> Vec<u32> {
    if extent <= tile {
        return vec![0];
    }
    let last = extent - tile;
    let mut out: Vec<u32> = (0..=last).step_by(step as usize).collect();
    if out.last() != Some(&last) {
        out.push(last);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{ManifestInfo, manifest_payload};
    use crate::layout::SheetLayout;

    #[test]
    fn image_extensions_are_recognised_case_insensitively() {
        assert!(is_image_path(Path::new("sheet.PNG")));
        assert!(is_image_path(Path::new("a/b/scan-01.jpeg")));
        assert!(!is_image_path(Path::new("backup.pdf")));
        assert!(!is_image_path(Path::new("noext")));
    }

    #[test]
    fn tile_offsets_cover_the_whole_axis() {
        let offs = offsets(1000, 300, 150);
        assert_eq!(offs[0], 0);
        assert_eq!(*offs.last().unwrap(), 700, "last tile must touch the far edge");
        // Consecutive tiles overlap, so nothing falls between them.
        for pair in offs.windows(2) {
            assert!(pair[1] - pair[0] < 300);
        }
    }

    #[test]
    fn a_tile_larger_than_the_image_is_a_single_tile() {
        assert_eq!(offsets(200, 300, 150), vec![0]);
    }

    #[test]
    fn code_width_is_estimated_from_a_decoded_manifest() {
        let layout = SheetLayout::fit(210.0, 297.0, 35.0, 8.0, 2.0).unwrap();
        let payload = manifest_payload(&ManifestInfo {
            session_id: "abc",
            sha256_hex: "00",
            len: 10,
            chunks: 1,
            file_name: "f",
            chunk_bytes: 300,
            quiet_modules: 4,
            layout,
            compressed_len: 10,
            rs_block_shards: None,
        });
        let found: BTreeSet<String> = [payload].into();

        // A 300 dpi A4 scan is 2480 px wide; 35 mm of 210 mm is about 413 px.
        let gray = GrayImage::new(2480, 3508);
        let est = estimated_code_width_px(&gray, &found).unwrap();
        assert!((est - 413.0).abs() < 5.0, "estimated {est}");
    }

    #[test]
    fn without_a_manifest_a_ladder_of_tile_sizes_is_used() {
        let gray = GrayImage::new(2480, 3508);
        let plan = tile_plan(&gray, &BTreeSet::new(), &ScanOptions::default());
        assert!(plan.len() >= 3, "expected a ladder, got {plan:?}");
        assert!(plan.windows(2).all(|p| p[0].0 > p[1].0), "{plan:?}");
    }

    #[test]
    fn the_step_guarantees_every_code_lands_inside_some_tile() {
        // With a manifest the code width is known, so the step must not exceed the
        // slack between tile and code — otherwise a code can straddle every boundary.
        let layout = SheetLayout::fit(210.0, 297.0, 35.0, 8.0, 2.0).unwrap();
        let payload = manifest_payload(&ManifestInfo {
            session_id: "abc",
            sha256_hex: "00",
            len: 10,
            chunks: 1,
            file_name: "f",
            chunk_bytes: 300,
            quiet_modules: 4,
            layout,
            compressed_len: 10,
            rs_block_shards: None,
        });
        let found: BTreeSet<String> = [payload].into();
        let gray = GrayImage::new(2480, 3508);

        let qr_px = estimated_code_width_px(&gray, &found).unwrap();
        let plan = tile_plan(&gray, &found, &ScanOptions::default());
        assert_eq!(plan.len(), 1);
        let (tile, step) = plan[0];
        assert!(
            (step as f32) <= (tile as f32) - qr_px + 1.0,
            "step {step} too coarse for tile {tile} and code {qr_px}"
        );

        // A skewed code covers more ground than its nominal width. Coverage has to hold
        // for that inflated footprint, not just the ideal one.
        let skewed = qr_px * 1.02;
        let offs = offsets(gray.width(), tile, step);
        for p in (0..=(gray.width() - skewed as u32)).step_by(7) {
            assert!(
                offs.iter().any(|&o| o <= p && o + tile >= p + skewed as u32),
                "a code at {p} falls between tiles"
            );
        }
    }
}
