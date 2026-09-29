//! The backup pipeline: file -> zstd -> chunks -> QR sheets -> PDF.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use krilla::Document;
use krilla::color::rgb;
use krilla::geom::{PathBuilder, Point};
use krilla::num::NormalizedF32;
use krilla::page::PageSettings;
use krilla::paint::{Fill, FillRule};
use krilla::surface::Surface;
use krilla::text::{Font, TextDirection};
use qrcodegen::{QrCode, QrCodeEcc};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use system_fonts::{FontStyle, FoundFontSource, find_for_system_locale};

use crate::format::{
    ManifestInfo, PRODUCER, PRODUCER_VERSION, data_payload, data_payload_for_chunk,
    manifest_payload, parity_payload, parity_payload_for_shard, random_session_hex, sha256_hex,
    trim_f32,
};
use crate::layout::{PT_PER_MM, SheetLayout, a4_mm};
use crate::rs::{
    BlockPlan, RsParams, ShardRef, encode_parity, interleave, survives_single_sheet_loss,
};

/// The smallest chunk the adaptive sizing will fall back to. Below this the per-QR
/// overhead dominates and the sheet count explodes.
const MIN_CHUNK_BYTES: usize = 120;

/// Upper bound for the `k`/`m`/`pidx` fields when probing a parity payload's size.
/// A block never holds more shards than GF(2^8) can address.
const MAX_BLOCK_DESCRIPTION: usize = 256;

#[derive(Debug, Clone, Copy)]
pub struct BackupOptions {
    pub landscape: bool,
    pub qr_mm: f32,
    pub margin_mm: f32,
    pub gap_mm: f32,
    pub chunk_bytes: usize,
    pub min_module_mm: f32,
    pub zstd_level: i32,
    pub quiet_modules: i32,
    /// Reed-Solomon parity as a fraction of the data. 0 disables it.
    pub parity_frac: f32,
    /// Data shards per erasure block.
    pub rs_block_shards: usize,
}

impl Default for BackupOptions {
    fn default() -> Self {
        Self {
            landscape: false,
            qr_mm: 35.0,
            margin_mm: 8.0,
            gap_mm: 2.0,
            chunk_bytes: 700,
            min_module_mm: 0.30,
            zstd_level: 10,
            quiet_modules: 4,
            parity_frac: 0.25,
            rs_block_shards: 32,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackupReport {
    pub out_path: PathBuf,
    pub session_id: String,
    pub sha256_hex: String,
    pub chunk_bytes: usize,
    pub total_chunks: usize,
    pub total_pages: usize,
    pub layout: SheetLayout,
    /// Smallest QR module actually placed, in mm. This is what a printer and scanner
    /// have to resolve, so it is the number that decides whether the sheets survive.
    pub smallest_module_mm: Option<f32>,
    /// Parity shards written, and how they are grouped.
    pub parity_shards: usize,
    pub blocks: usize,
    /// How many QRs may be destroyed anywhere before the file is unrecoverable.
    /// Damage concentrated in one block is the limiting case, so this is the minimum
    /// over all blocks, not the total parity count.
    pub tolerated_losses: usize,
    /// Whether losing any single sheet outright is still recoverable.
    pub survives_sheet_loss: bool,
}

pub fn backup(input: &Path, output: Option<&Path>, opts: &BackupOptions) -> Result<BackupReport> {
    if opts.quiet_modules < 0 {
        bail!("quiet_modules must be >= 0");
    }

    let data = fs::read(input).with_context(|| format!("read input '{}'", input.display()))?;
    let file_name = input
        .file_name()
        .ok_or_else(|| anyhow!("input has no file name"))?
        .to_string_lossy()
        .to_string();

    let sha256 = sha256_hex(&data);

    // The zstd frame is self-describing; the decoder does not need the level.
    let compressed = zstd::stream::encode_all(std::io::Cursor::new(&data), opts.zstd_level)
        .context("zstd compress failed")?;

    let session_id = random_session_hex(8);

    let (page_w_mm, page_h_mm) = a4_mm(opts.landscape);
    let layout = SheetLayout::fit(
        page_w_mm,
        page_h_mm,
        opts.qr_mm,
        opts.margin_mm,
        opts.gap_mm,
    )?;
    if layout.per_page() < 2 {
        bail!("grid too small: need at least 2 cells per page (one reserved for manifest)");
    }
    let data_per_page = layout.data_per_page();

    if opts.parity_frac < 0.0 {
        bail!("parity_frac must be >= 0");
    }
    if opts.rs_block_shards == 0 {
        bail!("rs_block_shards must be >= 1");
    }
    let with_parity = opts.parity_frac > 0.0;

    // Keep the QR modules large enough to survive print + scan.
    let chunk_bytes = choose_chunk_bytes_adaptive(
        opts.chunk_bytes.max(MIN_CHUNK_BYTES),
        opts.min_module_mm,
        opts.qr_mm,
        opts.quiet_modules,
        &session_id,
        compressed.len(),
        with_parity,
    )?;

    let chunks: Vec<&[u8]> = compressed.chunks(chunk_bytes).collect();
    let total_chunks = chunks.len();
    if total_chunks == 0 {
        bail!("internal: no chunks created");
    }

    let plan = if with_parity {
        Some(BlockPlan::new(
            total_chunks,
            RsParams {
                block_shards: opts.rs_block_shards,
                parity_frac: opts.parity_frac,
            },
        )?)
    } else {
        None
    };
    let parity_blocks = match &plan {
        Some(plan) => encode_parity(&chunks, chunk_bytes, plan)?,
        None => Vec::new(),
    };

    let manifest = manifest_payload(&ManifestInfo {
        session_id: &session_id,
        sha256_hex: &sha256,
        len: data.len(),
        chunks: total_chunks,
        file_name: &file_name,
        chunk_bytes,
        quiet_modules: opts.quiet_modules,
        layout,
        compressed_len: compressed.len(),
        rs_block_shards: plan.map(|p| p.block_shards),
    });

    // The adaptive sizing above only bounds the *data* QRs. The manifest carries the
    // file name, so a long name can make it the largest payload on the page.
    if opts.min_module_mm > 0.0 {
        let mm = module_mm_for_payload(&manifest, opts.qr_mm, opts.quiet_modules)
            .context("manifest does not fit into a QR code at ECC=H (file name too long?)")?;
        if mm < opts.min_module_mm {
            bail!(
                "manifest QR module size would be {mm:.3} mm, below --min-module-mm {:.3} \
                 (file name too long?). Increase --qr-mm (currently {}), reduce --quiet-modules \
                 (currently {}), or pass --min-module-mm 0 to override.",
                opts.min_module_mm,
                opts.qr_mm,
                opts.quiet_modules
            );
        }
    }

    // Placement order. With parity, shards of one block are spread across the sheets so
    // that localised damage — a stain, a tear, a lost page — costs each block only a few
    // shards instead of wiping one out entirely.
    let order: Vec<ShardRef> = match &plan {
        Some(plan) => interleave(plan),
        None => (0..total_chunks).map(ShardRef::Data).collect(),
    };

    let payloads: Vec<String> = order
        .iter()
        .map(|r| match *r {
            ShardRef::Data(idx) => {
                data_payload_for_chunk(&session_id, idx, total_chunks, chunks[idx])
            }
            ShardRef::Parity(block, pidx) => {
                let b = &parity_blocks[block];
                parity_payload_for_shard(
                    &session_id,
                    block,
                    pidx,
                    b.data_shards,
                    b.shards.len(),
                    &b.shards[pidx],
                )
            }
        })
        .collect();

    let out_path = output
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| input.with_extension("coldbackup.pdf"));

    let page_w_pt = page_w_mm * PT_PER_MM;
    let page_h_pt = page_h_mm * PT_PER_MM;
    let settings =
        PageSettings::from_wh(page_w_pt, page_h_pt).ok_or_else(|| anyhow!("invalid page size"))?;

    let mut doc = Document::new();
    let font = load_font()?;
    let total_pages = payloads.len().div_ceil(data_per_page);

    for (page_idx_0, slice) in payloads.chunks(data_per_page).enumerate() {
        let mut page = doc.start_page_with(settings.clone());
        let mut surface = page.surface();

        draw_page_labels(
            &mut surface,
            &font,
            page_h_pt,
            &PageLabels {
                session_id: &session_id,
                sha256_hex: &sha256,
                layout,
                chunk_bytes,
                total_chunks,
                page_idx_1: page_idx_0 + 1,
                total_pages,
                cells_on_page: slice.len(),
            },
        );

        // Cell 0 is the manifest, repeated on every page so any single sheet is enough
        // to identify the backup.
        place_qr_in_cell(&mut surface, &manifest, 0, layout, opts.quiet_modules)?;

        for (k, payload) in slice.iter().enumerate() {
            place_qr_in_cell(&mut surface, payload, 1 + k, layout, opts.quiet_modules)?;
        }

        surface.finish();
        page.finish();
    }

    let pdf_bytes = doc.finish().map_err(|e| anyhow!("{e:?}"))?;
    fs::write(&out_path, pdf_bytes).with_context(|| format!("write '{}'", out_path.display()))?;

    let smallest_module_mm = payloads
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(manifest.as_str()))
        .max_by_key(|p| p.len())
        .and_then(|p| module_mm_for_payload(p, opts.qr_mm, opts.quiet_modules).ok());

    // A block is lost the moment it drops below its own threshold, so the guarantee is
    // set by the weakest block, not by the total parity count.
    let tolerated_losses = plan
        .map(|p| {
            (0..p.block_count())
                .map(|b| p.parity_shards(b))
                .min()
                .unwrap_or(0)
        })
        .unwrap_or(0);

    Ok(BackupReport {
        out_path,
        session_id,
        sha256_hex: sha256,
        chunk_bytes,
        total_chunks,
        total_pages,
        layout,
        smallest_module_mm,
        parity_shards: payloads.len() - total_chunks,
        blocks: plan.map(|p| p.block_count()).unwrap_or(0),
        tolerated_losses,
        survives_sheet_loss: plan
            .map(|p| survives_single_sheet_loss(&order, data_per_page, &p))
            .unwrap_or(false),
    })
}

// -------------------- Chunk sizing --------------------

fn choose_chunk_bytes_adaptive(
    mut chunk_bytes: usize,
    min_module_mm: f32,
    qr_mm: f32,
    quiet_modules: i32,
    session_id: &str,
    compressed_len: usize,
    with_parity: bool,
) -> Result<usize> {
    if chunk_bytes < MIN_CHUNK_BYTES {
        chunk_bytes = MIN_CHUNK_BYTES;
    }
    if min_module_mm <= 0.0 {
        return Ok(chunk_bytes);
    }

    loop {
        let total_chunks = compressed_len.max(1).div_ceil(chunk_bytes).max(1);

        // Probe with the *highest* index: it has the most digits, so it is the longest
        // payload and therefore the one with the smallest modules.
        let b64 = URL_SAFE_NO_PAD.encode(vec![0u8; chunk_bytes]);
        let mut payload = data_payload(session_id, total_chunks - 1, total_chunks, u32::MAX, &b64);

        if with_parity {
            // Parity payloads carry more key/value pairs than data payloads for the same
            // shard, so they, not the data QRs, set the smallest module size. Over-state
            // the block description to stay on the safe side.
            let probe = parity_payload(
                session_id,
                total_chunks,
                MAX_BLOCK_DESCRIPTION,
                MAX_BLOCK_DESCRIPTION,
                MAX_BLOCK_DESCRIPTION,
                chunk_bytes,
                u32::MAX,
                &b64,
            );
            if probe.len() > payload.len() {
                payload = probe;
            }
        }

        match module_mm_for_payload(&payload, qr_mm, quiet_modules) {
            Ok(mm) if mm >= min_module_mm => return Ok(chunk_bytes),
            _ if chunk_bytes > MIN_CHUNK_BYTES => {
                chunk_bytes = (((chunk_bytes as f32) * 0.9) as usize).max(MIN_CHUNK_BYTES);
            }
            // Shrinking is exhausted. Returning a layout that violates the caller's
            // minimum would produce a PDF that cannot survive print+scan, and the
            // failure would only surface years later — so refuse to write it.
            Ok(mm) => bail!(
                "cannot satisfy --min-module-mm {min_module_mm:.3}: even at the smallest chunk size \
                 ({MIN_CHUNK_BYTES} bytes) the QR module size would be {mm:.3} mm. Increase --qr-mm \
                 (currently {qr_mm}), reduce --quiet-modules (currently {quiet_modules}), or pass \
                 --min-module-mm 0 to override."
            ),
            Err(e) => bail!(
                "even a {MIN_CHUNK_BYTES}-byte chunk does not fit into a QR code at ECC=H: {e:#}"
            ),
        }
    }
}

/// Edge length of one QR module in mm for a given payload, quiet zone included.
pub fn module_mm_for_payload(payload: &str, qr_mm: f32, quiet_modules: i32) -> Result<f32> {
    let code = QrCode::encode_binary(payload.as_bytes(), QrCodeEcc::High)
        .map_err(|_| anyhow!("payload too long for ECC=H"))?;
    let total = code.size() + 2 * quiet_modules;
    if total <= 0 {
        bail!("invalid QR module total");
    }
    Ok(qr_mm / (total as f32))
}

// -------------------- Drawing --------------------

fn place_qr_in_cell(
    surface: &mut Surface<'_>,
    payload: &str,
    cell_index: usize,
    layout: SheetLayout,
    quiet_modules: i32,
) -> Result<()> {
    let (x_mm, y_mm) = layout.cell_origin_mm(cell_index);
    draw_qr_vector(
        surface,
        payload.as_bytes(),
        Point::from_xy(x_mm * PT_PER_MM, y_mm * PT_PER_MM),
        layout.qr_mm * PT_PER_MM,
        quiet_modules,
    )
}

/// Draw a QR as vector rectangles (one per horizontal run of dark modules), so the
/// result stays crisp at any print resolution.
fn draw_qr_vector(
    surface: &mut Surface<'_>,
    data: &[u8],
    top_left: Point,
    size_pt: f32,
    quiet_modules: i32,
) -> Result<()> {
    let code = QrCode::encode_binary(data, QrCodeEcc::High)
        .map_err(|_| anyhow!("data too long for QR at ECC=H"))?;

    let n = code.size();
    let total = n + 2 * quiet_modules;
    if total <= 0 {
        bail!("invalid QR size");
    }
    let module = size_pt / (total as f32);

    surface.set_fill(Some(Fill {
        paint: rgb::Color::new(0, 0, 0).into(),
        opacity: NormalizedF32::ONE,
        rule: FillRule::NonZero,
    }));

    let mut pb = PathBuilder::new();
    for y in 0..n {
        let mut x = 0;
        while x < n {
            while x < n && !code.get_module(x, y) {
                x += 1;
            }
            if x >= n {
                break;
            }
            let start = x;
            while x < n && code.get_module(x, y) {
                x += 1;
            }

            let rx = ((start + quiet_modules) as f32) * module + top_left.x;
            let ry = ((y + quiet_modules) as f32) * module + top_left.y;
            add_rect(&mut pb, rx, ry, ((x - start) as f32) * module, module);
        }
    }

    let path = pb.finish().ok_or_else(|| anyhow!("empty path"))?;
    surface.draw_path(&path);
    Ok(())
}

fn add_rect(pb: &mut PathBuilder, x: f32, y: f32, w: f32, h: f32) {
    pb.move_to(x, y);
    pb.line_to(x + w, y);
    pb.line_to(x + w, y + h);
    pb.line_to(x, y + h);
    pb.close();
}

struct PageLabels<'a> {
    session_id: &'a str,
    sha256_hex: &'a str,
    layout: SheetLayout,
    chunk_bytes: usize,
    total_chunks: usize,
    page_idx_1: usize,
    total_pages: usize,
    cells_on_page: usize,
}

/// Human-readable header and footer. These are what someone holding the sheet has to
/// go on, so they spell out the exact restore command.
fn draw_page_labels(
    surface: &mut Surface<'_>,
    font: &Font,
    page_h_pt: f32,
    l: &PageLabels<'_>,
) {
    surface.set_fill(Some(Fill {
        paint: rgb::Color::new(0, 0, 0).into(),
        opacity: NormalizedF32::ONE,
        rule: FillRule::NonZero,
    }));

    let left = 8.0_f32; // pt
    let header_y = 12.0_f32; // Y=0 is the top of the page, Y grows downwards.
    let footer1_y = page_h_pt - 16.0;
    let footer2_y = page_h_pt - 28.0;

    let sha_short = &l.sha256_hex[..l.sha256_hex.len().min(16)];

    let header = format!(
        "{} {} | sid {}",
        PRODUCER, PRODUCER_VERSION, l.session_id
    );
    // Chunks are interleaved across sheets for damage resistance, so a page no longer
    // holds a contiguous range and printing one would be misleading.
    let footer1 = format!(
        "page {}/{} | grid {}x{} | {} codes on this page | chunk_bytes {} | total_chunks {}",
        l.page_idx_1,
        l.total_pages,
        l.layout.cols,
        l.layout.rows,
        l.cells_on_page,
        l.chunk_bytes,
        l.total_chunks
    );
    let footer2 = format!(
        "sha256 {}… | restore: coldbackup restore <pdf> --cols {} --rows {} --qr-mm {} --margin-mm {} --gap-mm {}",
        sha_short,
        l.layout.cols,
        l.layout.rows,
        trim_f32(l.layout.qr_mm),
        trim_f32(l.layout.margin_mm),
        trim_f32(l.layout.gap_mm)
    );

    for (y, size, text) in [
        (header_y, 8.5, &header),
        (footer2_y, 8.0, &footer2),
        (footer1_y, 8.0, &footer1),
    ] {
        surface.draw_text(
            Point::from_xy(left, y),
            font.clone(),
            size,
            text,
            false,
            TextDirection::Auto,
        );
    }
}

fn load_font() -> Result<Font> {
    let (_locale, _region, fonts) = find_for_system_locale(FontStyle::Sans);
    let found = fonts
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no system font found via system-fonts"))?;

    let bytes: Vec<u8> = match found.source {
        FoundFontSource::Bytes(b) => b.as_ref().to_vec(),
        FoundFontSource::Path(path) => std::fs::read(&path)
            .with_context(|| format!("read system font '{}'", path.display()))?,
    };

    let arc: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(bytes);
    let data: krilla::Data = arc.into();

    // TTC/OTC collections need an index; plain TTF/OTF is always index 0.
    for idx in 0..8u32 {
        if let Some(f) = Font::new(data.clone(), idx) {
            return Ok(f);
        }
    }

    Err(anyhow!(
        "krilla: could not load system font (indices 0..7 failed)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_size_shrinks_as_payloads_grow() {
        let small = module_mm_for_payload("CB1D|sid=aa|idx=0", 35.0, 4).unwrap();
        let large = module_mm_for_payload(&"x".repeat(800), 35.0, 4).unwrap();
        assert!(small > large, "{small} should exceed {large}");
    }

    #[test]
    fn sizing_backs_off_until_the_minimum_is_met() {
        let chosen =
            choose_chunk_bytes_adaptive(700, 0.30, 35.0, 4, "0123456789abcdef", 100_000, true).unwrap();
        assert!(chosen < 700, "should have shrunk from 700, got {chosen}");

        // The chosen size must actually satisfy the constraint for the worst payload.
        let b64 = URL_SAFE_NO_PAD.encode(vec![0u8; chosen]);
        let total = 100_000usize.div_ceil(chosen);
        let payload = data_payload("0123456789abcdef", total - 1, total, u32::MAX, &b64);
        assert!(module_mm_for_payload(&payload, 35.0, 4).unwrap() >= 0.30);
    }

    #[test]
    fn unprintable_geometry_is_refused_rather_than_downgraded() {
        let err = choose_chunk_bytes_adaptive(700, 0.30, 8.0, 4, "0123456789abcdef", 10_000, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("min-module-mm"), "unexpected error: {err}");
    }

    #[test]
    fn the_minimum_can_be_disabled_explicitly() {
        let chosen =
            choose_chunk_bytes_adaptive(700, 0.0, 8.0, 4, "0123456789abcdef", 10_000, true).unwrap();
        assert_eq!(chosen, 700);
    }
}
