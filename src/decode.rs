//! The restore pipeline: PDF -> rendered pages -> QR text -> chunks -> file.

use anyhow::{Context, Result, anyhow, bail};
use hayro::hayro_syntax::Pdf;
use image::{GrayImage, imageops};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::format::{
    Manifest, TAG_DATA, TAG_MANIFEST, TAG_PARITY, choose_session, find_sessions,
    parse_data_payload, parse_manifest_payload, parse_parity_payload, sha256_hex,
};
use crate::imaging::{
    binarize_otsu, clamp_crop, decode_all_qr, decode_single_qr, invert_gray, is_low_contrast,
    render_page_gray,
};
use crate::layout::{SheetLayout, a4_from_image_dims};
use crate::rs::{BlockPlan, recover};
use crate::scan::{self, ScanOptions};

/// Resolutions tried in order. A cell that is unreadable at one DPI is often readable
/// at another, and results from every pass are pooled.
///
/// Only ever applied to a coldbackup PDF: scans and photographs go through
/// [`crate::scan`], which works from the pixels it is given.
const EXTRA_DPIS: [u32; 1] = [900];

/// Default resolution for rendering our own sheets.
///
/// `--min-module-mm` keeps modules at 0.3 mm or wider, which 600 dpi renders at about
/// 7 pixels. That margin is deliberate: at 300 dpi the same sheet gives only 3.7 pixels
/// per module and a dense page can lose nearly every code while the short manifest still
/// reads — which looks exactly like corruption but is only under-sampling. Even 450 dpi
/// left a handful of codes unread in practice. Since these are our own vector sheets,
/// rendering them properly the first time is cheaper than a second pass.
pub const DEFAULT_RENDER_DPI: u32 = 600;

/// Pixels per QR module a scan needs before codes read reliably. Measured from the
/// codes themselves, so it holds whatever the sheet geometry and scanner settings are.
const MIN_COMFORTABLE_MODULE_PX: f32 = 4.0;

#[derive(Debug, Clone)]
pub struct RestoreOptions {
    pub out_dir: PathBuf,
    pub render_dpi: u32,
    pub session: Option<String>,
    pub output_name: Option<String>,
    /// Layout hints, used only when no manifest could be decoded.
    pub qr_mm: f32,
    pub margin_mm: f32,
    pub gap_mm: f32,
    pub cols: i32,
    pub rows: i32,
    /// Crop padding as a fraction of the QR size.
    pub crop_pad_frac: f32,
    pub try_invert: bool,
}

impl Default for RestoreOptions {
    fn default() -> Self {
        Self {
            out_dir: PathBuf::from("."),
            render_dpi: DEFAULT_RENDER_DPI,
            session: None,
            output_name: None,
            qr_mm: 35.0,
            margin_mm: 8.0,
            gap_mm: 2.0,
            cols: 5,
            rows: 7,
            crop_pad_frac: 0.10,
            try_invert: true,
        }
    }
}

impl RestoreOptions {
    fn fallback_layout(&self, page_w_mm: f32, page_h_mm: f32) -> Option<SheetLayout> {
        SheetLayout::from_parts(
            page_w_mm,
            page_h_mm,
            self.qr_mm,
            self.margin_mm,
            self.gap_mm,
            self.cols,
            self.rows,
        )
    }
}

#[derive(Debug, Clone)]
pub struct RestoreReport {
    pub out_file: PathBuf,
    pub sha_path: PathBuf,
    pub session_id: String,
    pub sha256_hex: String,
    /// True when the restored bytes were checked against a decoded manifest.
    pub verified: bool,
    pub producer: Option<String>,
    /// Chunks that had to be rebuilt from parity.
    pub recovered_chunks: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct Extracted {
    pub(crate) manifest: Option<Manifest>,
    pub(crate) data_chunks: BTreeMap<usize, Vec<u8>>,
    pub(crate) total_chunks: usize,
    /// Chunks rebuilt from Reed-Solomon parity rather than read off a sheet.
    pub(crate) recovered: usize,
    /// Further codes that could still be lost before some block becomes unrecoverable,
    /// measured before any repair so it reflects what is physically readable.
    pub(crate) spare_codes: Option<usize>,
}

/// Silences panic output for as long as it lives.
///
/// rqrr panics internally on some cells. Those panics are caught in `imaging`, but the
/// default hook would still print a backtrace for each one. Restoring the hook from
/// `Drop` means an unexpected panic elsewhere cannot leave the process muted.
struct PanicSilencer(Option<Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send>>);

impl PanicSilencer {
    fn new() -> Self {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        Self(Some(prev))
    }
}

impl Drop for PanicSilencer {
    fn drop(&mut self) {
        if let Some(prev) = self.0.take() {
            std::panic::set_hook(prev);
        }
    }
}

fn open_pdf(input_pdf: &Path) -> Result<Pdf> {
    let pdf_bytes =
        fs::read(input_pdf).with_context(|| format!("read '{}'", input_pdf.display()))?;
    Pdf::new(Arc::new(pdf_bytes)).map_err(|e| anyhow!("hayro_syntax: Pdf::new failed: {e:?}"))
}

/// Where the codes are being read from.
#[derive(Debug, Clone)]
pub enum Source {
    /// The generated PDF, where the exact grid geometry is known.
    Pdf(PathBuf),
    /// Scanned or photographed sheets, one image per sheet.
    Sheets(Vec<PathBuf>),
}

/// Decide what kind of input this is.
///
/// The extension is only a hint — the PDF header decides, so a scan named `.pdf` or a
/// PDF named `.png` still lands in the right pipeline.
pub fn detect_source(input: &Path) -> Result<Source> {
    if input.is_dir() {
        let images = scan::images_in_dir(input)
            .with_context(|| format!("read directory '{}'", input.display()))?;
        if images.is_empty() {
            bail!(
                "no images found in '{}' (looked for {})",
                input.display(),
                "png, jpg, jpeg, tif, tiff, bmp, webp, gif"
            );
        }
        return Ok(Source::Sheets(images));
    }

    let mut header = [0u8; 5];
    let looks_like_pdf = match fs::File::open(input) {
        Ok(mut f) => {
            use std::io::Read;
            let n = f.read(&mut header).unwrap_or(0);
            header[..n].starts_with(b"%PDF")
        }
        Err(e) => {
            return Err(anyhow!("open '{}': {e}", input.display()));
        }
    };

    if looks_like_pdf {
        Ok(Source::Pdf(input.to_path_buf()))
    } else {
        Ok(Source::Sheets(vec![input.to_path_buf()]))
    }
}

pub fn restore(input: &Path, opts: &RestoreOptions) -> Result<RestoreReport> {
    if opts.cols <= 0 || opts.rows <= 0 {
        bail!("--cols and --rows must be > 0");
    }

    fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("create out dir '{}'", opts.out_dir.display()))?;

    match detect_source(input)? {
        Source::Pdf(path) => {
            let pdf = open_pdf(&path)?;
            let _silence = PanicSilencer::new();
            restore_inner(&pdf, opts)
        }
        Source::Sheets(paths) => {
            let payloads = scan_sheets(&paths, opts)?;
            let (session, extracted) = assemble(&payloads, opts)?;
            if extracted.data_chunks.len() < extracted.total_chunks {
                return Err(missing_chunks_error(&session, &extracted));
            }
            reconstruct_and_write(opts, &session, extracted)
        }
    }
}

/// Read every payload from whatever the input is: a PDF, one sheet image, or a folder.
///
/// Unlike [`restore`] this never stops early, so it reports everything the sheets still
/// hold — which is what a verification needs to judge the margin left.
pub fn gather_payloads(input: &Path, opts: &RestoreOptions) -> Result<Vec<String>> {
    match detect_source(input)? {
        Source::Pdf(path) => scan_pdf(&path, opts),
        Source::Sheets(paths) => scan_sheets(&paths, opts),
    }
}

/// Render each sheet of a backup PDF to a grayscale image.
///
/// Useful for exporting sheets, and it is what makes a scan testable: the same pixels a
/// scanner would see, before any of a scanner's distortions.
pub fn render_sheets(input_pdf: &Path, dpi: u32) -> Result<Vec<GrayImage>> {
    let pdf = open_pdf(input_pdf)?;
    Ok(pdf
        .pages()
        .iter()
        .map(|page| render_page_gray(page, dpi))
        .collect())
}

/// Decode every payload from scanned or photographed sheets.
pub fn scan_sheets(paths: &[PathBuf], opts: &RestoreOptions) -> Result<Vec<String>> {
    let _silence = PanicSilencer::new();
    let scan_opts = ScanOptions {
        try_invert: opts.try_invert,
        ..Default::default()
    };

    let mut decoded: Vec<String> = Vec::new();
    for (n, path) in paths.iter().enumerate() {
        let gray = scan::load_sheet(path)?;
        let sheet = scan::scan_sheet(&gray, &scan_opts);
        let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();

        let resolution = match (sheet.effective_dpi, sheet.module_px) {
            (Some(dpi), Some(px)) => format!(" (~{dpi:.0} dpi, {px:.1} px per module)"),
            (None, Some(px)) => format!(" ({px:.1} px per module)"),
            (Some(dpi), None) => format!(" (~{dpi:.0} dpi)"),
            (None, None) => String::new(),
        };
        eprintln!(
            "sheet {}/{} '{}': {}x{} px{}, {} code(s)",
            n + 1,
            paths.len(),
            name,
            gray.width(),
            gray.height(),
            resolution,
            sheet.texts.len()
        );

        // Below roughly four pixels per module the remaining codes are not there to be
        // found however hard the sheet is searched — the scan simply did not record
        // them. Saying so beats letting the restore fail with a list of missing chunks.
        if let Some(px) = sheet.module_px {
            if px < MIN_COMFORTABLE_MODULE_PX {
                let advice = match sheet.effective_dpi {
                    Some(dpi) => format!(
                        "rescan at about {:.0} dpi",
                        dpi * MIN_COMFORTABLE_MODULE_PX / px
                    ),
                    None => format!(
                        "rescan at about {:.1}x this resolution",
                        MIN_COMFORTABLE_MODULE_PX / px
                    ),
                };
                eprintln!(
                    "  warning: this scan resolves a module to only {px:.1} px; \
                     {MIN_COMFORTABLE_MODULE_PX:.0} px is about the minimum — {advice}"
                );
            }
        }

        let found = sheet.texts;
        decoded.extend(found);
    }

    decoded.sort_unstable();
    decoded.dedup();
    Ok(decoded)
}

/// Decode every coldbackup payload in a PDF without assembling them.
///
/// Unlike [`restore`] this runs every resolution rather than stopping as soon as the
/// data is complete, so it returns everything the sheets still hold.
pub fn scan_pdf(input_pdf: &Path, opts: &RestoreOptions) -> Result<Vec<String>> {
    if opts.cols <= 0 || opts.rows <= 0 {
        bail!("--cols and --rows must be > 0");
    }
    let pdf = open_pdf(input_pdf)?;
    let _silence = PanicSilencer::new();

    let mut decoded: Vec<String> = Vec::new();
    for dpi in dpi_ladder(opts) {
        let before = decoded.len();
        let manifest = first_manifest(&decoded);
        decoded.extend(decode_pdf_at_dpi(&pdf, dpi, opts, manifest.as_ref()));
        decoded.sort_unstable();
        decoded.dedup();

        // Stop as soon as every code the sheets declare has been read, and otherwise as
        // soon as a resolution turns up nothing new. Without this a clean PDF pays for a
        // second and third full render just to confirm there was nothing left.
        if all_codes_accounted_for(&decoded) || (decoded.len() == before && before > 0) {
            break;
        }
    }
    Ok(decoded)
}

/// Assemble a backup from payloads decoded elsewhere.
///
/// This is the seam between reading codes and rebuilding a file: anything that can
/// produce payload strings — a PDF, a scan, a phone camera — can restore through here.
pub fn restore_from_payloads(payloads: &[String], opts: &RestoreOptions) -> Result<RestoreReport> {
    fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("create out dir '{}'", opts.out_dir.display()))?;

    let (session, extracted) = assemble(payloads, opts)?;
    if extracted.data_chunks.len() < extracted.total_chunks {
        return Err(missing_chunks_error(&session, &extracted));
    }
    reconstruct_and_write(opts, &session, extracted)
}

/// True when the payloads read so far are everything the backup says it wrote.
///
/// The manifest gives the chunk count and the block size; each parity code states how
/// many parity shards its own block has. Together that is the full expected inventory,
/// so completeness is a fact here rather than a guess.
fn all_codes_accounted_for(decoded: &[String]) -> bool {
    let sessions = find_sessions(decoded);
    if sessions.len() != 1 {
        return false;
    }
    let session = sessions.iter().next().expect("one session");

    let Some(manifest) = first_manifest(decoded) else {
        return false;
    };
    if &manifest.sid != session {
        return false;
    }
    let Some(total_chunks) = manifest.chunks else {
        return false;
    };

    let mut data: BTreeSet<usize> = BTreeSet::new();
    // block -> (shards declared, shards seen)
    let mut parity: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for payload in decoded {
        if payload.starts_with(TAG_DATA) {
            if let Ok(c) = parse_data_payload(payload) {
                if &c.sid == session {
                    data.insert(c.idx);
                }
            }
        } else if payload.starts_with(TAG_PARITY) {
            if let Ok(p) = parse_parity_payload(payload) {
                if &p.sid == session {
                    let e = parity.entry(p.block).or_insert((p.parity_shards, 0));
                    e.1 += 1;
                }
            }
        }
    }

    if data.len() != total_chunks {
        return false;
    }

    match manifest.rs_block_shards {
        // No parity was written, so the data alone is the whole inventory.
        None => true,
        Some(block_shards) => match BlockPlan::from_recorded(total_chunks, block_shards) {
            Ok(plan) => (0..plan.block_count()).all(|b| match parity.get(&b) {
                Some((declared, seen)) => seen >= declared,
                None => false,
            }),
            Err(_) => false,
        },
    }
}

fn dpi_ladder(opts: &RestoreOptions) -> Vec<u32> {
    let mut dpis = vec![opts.render_dpi];
    dpis.extend(EXTRA_DPIS);
    dpis.sort_unstable();
    dpis.dedup();
    dpis
}

pub(crate) fn assemble(decoded: &[String], opts: &RestoreOptions) -> Result<(String, Extracted)> {
    let sessions_found = find_sessions(decoded);
    if sessions_found.is_empty() {
        bail!("no sessions found");
    }
    let session = choose_session(&sessions_found, opts.session.as_deref())?;
    let extracted = extract_backup(decoded, &session)?;
    Ok((session, extracted))
}

fn missing_chunks_error(sid: &str, extracted: &Extracted) -> anyhow::Error {
    let missing: Vec<usize> = (0..extracted.total_chunks)
        .filter(|i| !extracted.data_chunks.contains_key(i))
        .collect();
    let shown = missing
        .iter()
        .take(20)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let ellipsis = if missing.len() > 20 { ", …" } else { "" };
    anyhow!(
        "session {}: {} of {} chunks could not be decoded or repaired. Missing idx: {}{}",
        sid,
        missing.len(),
        extracted.total_chunks,
        shown,
        ellipsis
    )
}

fn restore_inner(pdf: &Pdf, opts: &RestoreOptions) -> Result<RestoreReport> {
    let dpis = dpi_ladder(opts);

    // Payloads accumulate across DPI passes. A chunk that only decodes at 300 dpi and
    // one that only decodes at 600 dpi both count toward the same result — scoring each
    // pass on its own would throw away a complete union.
    let mut decoded: Vec<String> = Vec::new();
    let mut last: Option<(String, Extracted)> = None;

    for dpi in dpis {
        let manifest = first_manifest(&decoded);
        decoded.extend(decode_pdf_at_dpi(pdf, dpi, opts, manifest.as_ref()));
        decoded.sort_unstable();
        decoded.dedup();

        if decoded.is_empty() {
            eprintln!("dpi={dpi}: no {TAG_MANIFEST}/{TAG_DATA} payloads found");
            continue;
        }

        let (chosen_session, extracted) = match assemble(&decoded, opts) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("dpi={dpi}: {e:#}");
                continue;
            }
        };

        let have = extracted.data_chunks.len();
        let total = extracted.total_chunks;
        eprintln!(
            "dpi={dpi}: session={chosen_session} got {have}/{total} chunks (manifest: {})",
            if extracted.manifest.is_some() {
                "yes"
            } else {
                "no"
            }
        );

        if have == total {
            return reconstruct_and_write(opts, &chosen_session, extracted);
        }
        last = Some((chosen_session, extracted));
    }

    if let Some((sid, extracted)) = last {
        return Err(missing_chunks_error(&sid, &extracted));
    }

    bail!("no valid backup found in PDF");
}

// -------------------- Scanning --------------------

/// Decode one resolution of the whole document.
///
/// Both strategies run off a single render per page: the full-page scan finds the
/// manifest, whose layout then drives the exact per-cell crops. Rendering twice — once
/// per strategy — was costing more than all the decoding put together.
fn decode_pdf_at_dpi(
    pdf: &Pdf,
    dpi: u32,
    opts: &RestoreOptions,
    manifest: Option<&Manifest>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut layout_source: Option<Manifest> = manifest.cloned();

    for page in pdf.pages().iter() {
        let bin = binarize_otsu(&render_page_gray(page, dpi));

        // The full-page scan exists to find the manifest, and on a dense sheet that is
        // about all it ever finds. Once the layout is known it earns nothing and costs a
        // detector pass over tens of megapixels, so it only runs while we still need it.
        if layout_source.as_ref().and_then(|m| m.layout).is_none() {
            let full_page = decode_all_qr(bin.clone());
            if let Some(m) = first_manifest(&full_page) {
                layout_source = Some(m);
            }
            out.extend(full_page);
        }

        out.extend(decode_grid_on_page(&bin, opts, layout_source.as_ref()));
    }
    out
}

fn first_manifest(decoded: &[String]) -> Option<Manifest> {
    decoded
        .iter()
        .filter(|s| s.starts_with(TAG_MANIFEST))
        .find_map(|s| parse_manifest_payload(s).ok())
}

/// Grid-crop decode on one already-rendered page: deterministic per-cell cropping.
///
/// Tries both Y mappings (top-left and bottom-left origin) and keeps whichever yields
/// more decodes on the page.
fn decode_grid_on_page(
    bin: &GrayImage,
    opts: &RestoreOptions,
    manifest: Option<&Manifest>,
) -> Vec<String> {
    let bin_inv = opts.try_invert.then(|| invert_gray(bin));

    // Exact layout from the manifest when we have one; otherwise the CLI hints with
    // the orientation inferred from the rendered page.
    let layout = manifest.and_then(|m| m.layout).or_else(|| {
        let (w, h) = a4_from_image_dims(bin.width(), bin.height());
        opts.fallback_layout(w, h)
    });
    let Some(layout) = layout else {
        return Vec::new();
    };

    let img_w = bin.width() as f32;
    let img_h = bin.height() as f32;
    let mm_to_px_x = img_w / layout.page_w_mm;
    let mm_to_px_y = img_h / layout.page_h_mm;

    let qr_w_px = layout.qr_mm * mm_to_px_x;
    let qr_h_px = layout.qr_mm * mm_to_px_y;
    let pad_px_x = (opts.crop_pad_frac.max(0.0) * qr_w_px).max(2.0);
    let pad_px_y = (opts.crop_pad_frac.max(0.0) * qr_h_px).max(2.0);

    let mut hits_top: Vec<String> = Vec::new();
    let mut hits_bottom: Vec<String> = Vec::new();

    for cell in 0..layout.per_page() {
        let (x_mm, y_mm_from_top) = layout.cell_origin_mm(cell);
        let x_px = x_mm * mm_to_px_x;

        // A: origin at the top-left of the image.
        let y_px_top = y_mm_from_top * mm_to_px_y;
        // B: the same offset measured from the bottom, as PDF space would.
        let y_px_bottom = (layout.page_h_mm - (y_mm_from_top + layout.qr_mm)) * mm_to_px_y;

        for (y_px, hits) in [(y_px_top, &mut hits_top), (y_px_bottom, &mut hits_bottom)] {
            if let Some(text) = decode_cell(
                bin,
                bin_inv.as_ref(),
                x_px,
                y_px,
                qr_w_px,
                qr_h_px,
                pad_px_x,
                pad_px_y,
            ) {
                hits.push(text);
            }
        }
    }

    if hits_bottom.len() > hits_top.len() {
        hits_bottom
    } else {
        hits_top
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_cell(
    bin: &GrayImage,
    bin_inv: Option<&GrayImage>,
    x_px: f32,
    y_px_top: f32,
    qr_w_px: f32,
    qr_h_px: f32,
    pad_px_x: f32,
    pad_px_y: f32,
) -> Option<String> {
    let (x0, y0, w, h) = clamp_crop(
        x_px - pad_px_x,
        y_px_top - pad_px_y,
        x_px + qr_w_px + pad_px_x,
        y_px_top + qr_h_px + pad_px_y,
        bin.width() as f32,
        bin.height() as f32,
    );

    if w < 24 || h < 24 {
        return None;
    }

    let crop = imageops::crop_imm(bin, x0, y0, w, h).to_image();
    // Skip blank crops: they cost time and provoke rqrr edge cases.
    if is_low_contrast(&crop) {
        return None;
    }
    if let Some(text) = decode_single_qr(crop) {
        return Some(text);
    }

    let inv = bin_inv?;
    let crop_inv = imageops::crop_imm(inv, x0, y0, w, h).to_image();
    if is_low_contrast(&crop_inv) {
        return None;
    }
    decode_single_qr(crop_inv)
}

// -------------------- Assembling --------------------

fn extract_backup(decoded: &[String], chosen_session: &str) -> Result<Extracted> {
    let mut manifest: Option<Manifest> = None;
    let mut candidates: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    // BTreeMap (not HashMap) so a tie breaks deterministically, toward the larger total.
    let mut total_votes: BTreeMap<usize, usize> = BTreeMap::new();
    let mut parity: BTreeMap<(usize, usize), Vec<u8>> = BTreeMap::new();
    let mut shard_lens: Vec<usize> = Vec::new();
    let mut block_shard_counts: Vec<usize> = Vec::new();
    let mut rejected = 0usize;

    for s in decoded {
        if !s.contains(chosen_session) {
            continue;
        }

        if s.starts_with(TAG_MANIFEST) {
            match parse_manifest_payload(s) {
                Ok(m) if m.sid == chosen_session => {
                    // Prefer a manifest that carries the layout.
                    match (&manifest, &m.layout) {
                        (Some(prev), Some(_)) if prev.layout.is_none() => manifest = Some(m),
                        (None, _) => manifest = Some(m),
                        _ => {}
                    }
                }
                Ok(_) => {}
                Err(_) => rejected += 1,
            }
        } else if s.starts_with(TAG_DATA) {
            // A payload that fails to parse or fails its CRC is exactly what the CRC is
            // there to catch. Drop that one chunk and keep every other chunk on the page
            // instead of aborting the whole extraction.
            match parse_data_payload(s) {
                Ok(c) if c.sid == chosen_session => {
                    *total_votes.entry(c.total).or_insert(0) += 1;
                    candidates.push((c.idx, c.total, c.bytes));
                }
                Ok(_) => {}
                Err(_) => rejected += 1,
            }
        } else if s.starts_with(TAG_PARITY) {
            match parse_parity_payload(s) {
                Ok(p) if p.sid == chosen_session => {
                    shard_lens.push(p.shard_len);
                    block_shard_counts.push(p.data_shards);
                    parity.insert((p.block, p.pidx), p.bytes);
                }
                Ok(_) => {}
                Err(_) => rejected += 1,
            }
        }
    }

    // The manifest is authoritative; without one, trust the most common `total`.
    let total_chunks = manifest
        .as_ref()
        .and_then(|m| m.chunks)
        .or_else(|| {
            total_votes
                .iter()
                .max_by_key(|(_, votes)| **votes)
                .map(|(total, _)| *total)
        })
        .ok_or_else(|| anyhow!("no data chunks found for session {}", chosen_session))?;

    let mut data_chunks: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    for (idx, total, bytes) in candidates {
        if total != total_chunks || idx >= total_chunks {
            rejected += 1;
            continue;
        }
        match data_chunks.get(&idx) {
            Some(existing) if existing != &bytes => rejected += 1,
            Some(_) => {}
            None => {
                data_chunks.insert(idx, bytes);
            }
        }
    }

    if rejected > 0 {
        eprintln!("  note: skipped {rejected} damaged or inconsistent payload(s)");
    }

    // Measured before any repair, so it reflects what is physically still readable
    // rather than what could be reconstructed from it.
    let block_shards = manifest
        .as_ref()
        .and_then(|m| m.rs_block_shards)
        .or_else(|| block_shard_counts.iter().max().copied());
    let spare_codes = block_shards
        .and_then(|k| BlockPlan::from_recorded(total_chunks, k).ok())
        .map(|plan| spare_per_block(&data_chunks, &parity, &plan));

    let recovered = if data_chunks.len() < total_chunks && !parity.is_empty() {
        repair_from_parity(
            &mut data_chunks,
            &parity,
            manifest.as_ref(),
            total_chunks,
            &shard_lens,
            &block_shard_counts,
        )
    } else {
        0
    };

    Ok(Extracted {
        manifest,
        data_chunks,
        total_chunks,
        recovered,
        spare_codes,
    })
}

/// How many further codes could be lost before some block becomes unrecoverable.
///
/// A block needs any `k` of its shards, so its slack is what it holds beyond `k`. The
/// weakest block sets the figure, because that is the one that fails first.
fn spare_per_block(
    data: &BTreeMap<usize, Vec<u8>>,
    parity: &BTreeMap<(usize, usize), Vec<u8>>,
    plan: &BlockPlan,
) -> usize {
    (0..plan.block_count())
        .map(|b| {
            let range = plan.data_range(b);
            let k = range.len();
            let have_data = range.filter(|i| data.contains_key(i)).count();
            let have_parity = parity.range((b, 0)..(b + 1, 0)).count();
            (have_data + have_parity).saturating_sub(k)
        })
        .min()
        .unwrap_or(0)
}

/// Rebuild missing chunks from parity shards.
///
/// The block description is taken from the manifest when it decoded, and otherwise from
/// the parity QRs themselves — each one repeats its block's shard counts precisely so a
/// damaged manifest cannot also cost the repair.
fn repair_from_parity(
    data_chunks: &mut BTreeMap<usize, Vec<u8>>,
    parity: &BTreeMap<(usize, usize), Vec<u8>>,
    manifest: Option<&Manifest>,
    total_chunks: usize,
    shard_lens: &[usize],
    block_shard_counts: &[usize],
) -> usize {
    // Every parity shard of a backup has the same length; a full block is the widest
    // observed shard count.
    let Some(&shard_len) = shard_lens.iter().max() else {
        return 0;
    };
    if shard_len == 0 {
        return 0;
    }

    let block_shards = manifest
        .and_then(|m| m.rs_block_shards)
        .or_else(|| block_shard_counts.iter().max().copied());
    let Some(block_shards) = block_shards.filter(|k| *k > 0) else {
        return 0;
    };

    // The final chunk is shorter than a shard, so its padding has to be trimmed. The
    // length comes from the manifest, or from the final chunk itself when that one was
    // read directly.
    let compressed_len = manifest.and_then(|m| m.compressed_len).or_else(|| {
        data_chunks
            .get(&(total_chunks - 1))
            .map(|last| (total_chunks - 1) * shard_len + last.len())
    });
    let Some(compressed_len) = compressed_len else {
        eprintln!(
            "  note: parity is present but the compressed length is unknown \
             (manifest and final chunk both missing); cannot repair"
        );
        return 0;
    };

    let Ok(plan) = BlockPlan::from_recorded(total_chunks, block_shards) else {
        return 0;
    };

    let result = recover(data_chunks, parity, &plan, shard_len, compressed_len);
    if result.recovered > 0 {
        eprintln!(
            "  repaired {} chunk(s) from Reed-Solomon parity",
            result.recovered
        );
    }
    if result.unrecoverable_blocks > 0 {
        eprintln!(
            "  note: {} block(s) lost more shards than their parity could cover",
            result.unrecoverable_blocks
        );
    }
    result.recovered
}

/// The file as rebuilt from the codes, before anything is written to disk.
pub(crate) struct Rebuilt {
    pub bytes: Vec<u8>,
    pub sha256_hex: String,
    /// The rebuilt bytes were checked against a decoded manifest.
    pub verified: bool,
    pub producer: Option<String>,
    pub filename: Option<String>,
}

/// Rebuild the file in memory and check it against its manifest.
///
/// Nothing is written here, which is what lets `verify` prove a backup is readable
/// without producing a file anyone could mistake for a restore.
pub(crate) fn rebuild(extracted: &Extracted) -> Result<Rebuilt> {
    let missing = extracted
        .total_chunks
        .saturating_sub(extracted.data_chunks.len());
    if missing != 0 {
        bail!("missing {} chunks (of {})", missing, extracted.total_chunks);
    }

    let mut compressed: Vec<u8> = Vec::new();
    for i in 0..extracted.total_chunks {
        compressed.extend_from_slice(
            extracted
                .data_chunks
                .get(&i)
                .ok_or_else(|| anyhow!("internal: chunk {i} vanished"))?,
        );
    }

    let bytes = zstd::stream::decode_all(std::io::Cursor::new(&compressed))
        .context("zstd decompress failed")?;
    let sha256 = sha256_hex(&bytes);

    // Check before returning: bytes that fail their own manifest must never reach a
    // caller looking like a success.
    let mut verified = false;
    if let Some(m) = &extracted.manifest {
        if let Some(exp_len) = m.len {
            if bytes.len() != exp_len {
                bail!("length mismatch: expected {}, got {}", exp_len, bytes.len());
            }
        }
        if let Some(exp_sha) = &m.sha256 {
            if &sha256 != exp_sha {
                bail!("sha256 mismatch: expected {}, got {}", exp_sha, sha256);
            }
            verified = true;
        }
    }

    let producer = extracted
        .manifest
        .as_ref()
        .and_then(|m| match (&m.prod, &m.pver) {
            (Some(p), Some(v)) => Some(format!("{p} {v}")),
            (Some(p), None) => Some(p.clone()),
            _ => None,
        });

    Ok(Rebuilt {
        bytes,
        sha256_hex: sha256,
        verified,
        producer,
        filename: extracted.manifest.as_ref().and_then(|m| m.filename.clone()),
    })
}

fn reconstruct_and_write(
    opts: &RestoreOptions,
    session: &str,
    extracted: Extracted,
) -> Result<RestoreReport> {
    let rebuilt = rebuild(&extracted)?;

    let filename = opts
        .output_name
        .clone()
        .or(rebuilt.filename)
        .unwrap_or_else(|| format!("restored-{session}.bin"));
    let filename =
        sanitize_filename(&filename).unwrap_or_else(|| format!("restored-{session}.bin"));

    let out_file = opts.out_dir.join(&filename);
    fs::write(&out_file, &rebuilt.bytes)
        .with_context(|| format!("write '{}'", out_file.display()))?;

    let sha_path = opts.out_dir.join(format!("{filename}.sha256"));
    fs::write(&sha_path, format!("{}  {filename}\n", rebuilt.sha256_hex))
        .with_context(|| format!("write '{}'", sha_path.display()))?;

    Ok(RestoreReport {
        out_file,
        sha_path,
        session_id: session.to_string(),
        sha256_hex: rebuilt.sha256_hex,
        verified: rebuilt.verified,
        producer: rebuilt.producer,
        recovered_chunks: extracted.recovered,
    })
}

/// Reduce a manifest-supplied name to a plain file name.
///
/// The name comes off a sheet that anyone could have produced, so it must never be
/// able to steer the write outside `--out-dir`.
fn sanitize_filename(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    // With every separator gone the name cannot walk out of `out_dir`; only the two
    // directory names themselves are still unusable. Leading dots are kept so a
    // legitimate dotfile survives the round trip.
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        None
    } else {
        Some(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{ManifestInfo, data_payload_for_chunk, manifest_payload};

    fn layout() -> SheetLayout {
        SheetLayout::fit(210.0, 297.0, 35.0, 8.0, 2.0).unwrap()
    }

    fn manifest_for(sid: &str, chunks: usize) -> String {
        manifest_payload(&ManifestInfo {
            session_id: sid,
            sha256_hex: "00",
            len: 9,
            chunks,
            file_name: "notes.txt",
            chunk_bytes: 3,
            quiet_modules: 4,
            layout: layout(),
            compressed_len: 9,
            rs_block_shards: None,
        })
    }

    #[test]
    fn a_damaged_chunk_does_not_discard_the_others() {
        let mut decoded = vec![
            manifest_for("sid1", 3),
            data_payload_for_chunk("sid1", 0, 3, b"aaa"),
            data_payload_for_chunk("sid1", 2, 3, b"ccc"),
        ];
        // Chunk 1 arrives with a broken CRC, as a misread QR would.
        decoded.push(data_payload_for_chunk("sid1", 1, 3, b"bbb").replace("crc32=", "crc32=f"));

        let extracted = extract_backup(&decoded, "sid1").unwrap();
        assert_eq!(extracted.total_chunks, 3);
        assert_eq!(extracted.data_chunks.len(), 2);
        assert_eq!(extracted.data_chunks[&0], b"aaa");
        assert_eq!(extracted.data_chunks[&2], b"ccc");
    }

    #[test]
    fn chunks_from_separate_passes_are_pooled() {
        // Neither pass is complete on its own; together they are.
        let pass_a = vec![data_payload_for_chunk("sid1", 0, 2, b"aaa")];
        let pass_b = vec![data_payload_for_chunk("sid1", 1, 2, b"bbb")];

        assert_eq!(extract_backup(&pass_a, "sid1").unwrap().data_chunks.len(), 1);

        let pooled: Vec<String> = pass_a.into_iter().chain(pass_b).collect();
        let extracted = extract_backup(&pooled, "sid1").unwrap();
        assert_eq!(extracted.data_chunks.len(), extracted.total_chunks);
    }

    #[test]
    fn duplicate_chunks_are_harmless() {
        let one = data_payload_for_chunk("sid1", 0, 1, b"aaa");
        let decoded = vec![one.clone(), one];
        assert_eq!(extract_backup(&decoded, "sid1").unwrap().data_chunks.len(), 1);
    }

    #[test]
    fn the_manifest_decides_the_chunk_count() {
        // A stray payload claiming a different total must not move the target.
        let decoded = vec![
            manifest_for("sid1", 2),
            data_payload_for_chunk("sid1", 0, 2, b"aaa"),
            data_payload_for_chunk("sid1", 1, 2, b"bbb"),
            data_payload_for_chunk("sid1", 5, 9, b"xxx"),
        ];
        let extracted = extract_backup(&decoded, "sid1").unwrap();
        assert_eq!(extracted.total_chunks, 2);
        assert_eq!(extracted.data_chunks.len(), 2);
    }

    #[test]
    fn without_a_manifest_the_majority_total_wins() {
        let decoded = vec![
            data_payload_for_chunk("sid1", 0, 2, b"aaa"),
            data_payload_for_chunk("sid1", 1, 2, b"bbb"),
            data_payload_for_chunk("sid1", 5, 9, b"xxx"),
        ];
        let extracted = extract_backup(&decoded, "sid1").unwrap();
        assert_eq!(extracted.total_chunks, 2);
    }

    #[test]
    fn other_sessions_are_ignored() {
        let decoded = vec![
            data_payload_for_chunk("sid1", 0, 1, b"aaa"),
            data_payload_for_chunk("sid2", 0, 1, b"zzz"),
        ];
        let extracted = extract_backup(&decoded, "sid1").unwrap();
        assert_eq!(extracted.data_chunks[&0], b"aaa");
    }

    #[test]
    fn filenames_cannot_escape_the_output_directory() {
        assert_eq!(sanitize_filename("notes.txt").as_deref(), Some("notes.txt"));
        // Dotfiles must survive intact.
        assert_eq!(sanitize_filename(".gitignore").as_deref(), Some(".gitignore"));

        // No separator may remain, so `out_dir.join(..)` cannot leave the directory.
        for hostile in ["../../etc/passwd", "/abs/path", "a\\b", "x\0y"] {
            let cleaned = sanitize_filename(hostile).expect("should yield a name");
            assert!(
                !cleaned.contains(['/', '\\', '\0']),
                "separator survived in {cleaned:?}"
            );
            assert_eq!(Path::new(&cleaned).components().count(), 1);
        }

        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("."), None);
        assert_eq!(sanitize_filename("   "), None);
        assert_eq!(sanitize_filename(""), None);
    }
}
