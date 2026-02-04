use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use clap::{Parser, Subcommand};
use crc32fast::Hasher as Crc32;
use hayro::hayro_syntax::Pdf;
use image::{GrayImage, ImageBuffer, Luma, imageops};
use krilla::Document;
use krilla::color::rgb;
use krilla::geom::{PathBuilder, Point};
use krilla::num::NormalizedF32;
use krilla::page::PageSettings;
use krilla::paint::{Fill, FillRule};
use krilla::surface::Surface;
use krilla::text::Font;
use qrcodegen::{QrCode, QrCodeEcc};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use system_fonts::{FontStyle, FoundFontSource, find_for_system_locale};

// Panic control for rqrr
use std::panic::{AssertUnwindSafe, catch_unwind};

// -------------------- Identity / format --------------------

const PRODUCER: &str = "coldbackup";
const PRODUCER_VERSION: &str = env!("CARGO_PKG_VERSION");

// Format version (SemVer): same major must remain compatible
const SUPPORTED_FORMAT_MAJOR: u32 = 1;

const TAG_MANIFEST: &str = "CB1M";
const TAG_DATA: &str = "CB1D";

const PT_PER_MM: f32 = 72.0 / 25.4;

// -------------------- CLI --------------------

#[derive(Parser, Debug)]
#[command(
    name = "coldbackup",
    about = "PDF cold backup via QR codes (backup + restore)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Backup {
        input: PathBuf,

        #[arg(short, long)]
        output: Option<PathBuf>,

        #[arg(long, default_value = "portrait", value_parser = ["portrait", "landscape"])]
        orientation: String,

        #[arg(long, default_value_t = 35.0)]
        qr_mm: f32,

        #[arg(long, default_value_t = 8.0)]
        margin_mm: f32,

        #[arg(long, default_value_t = 2.0)]
        gap_mm: f32,

        #[arg(long, default_value_t = 700)]
        chunk_bytes: usize,

        #[arg(long, default_value_t = 0.30)]
        min_module_mm: f32,

        #[arg(long, default_value_t = 10)]
        zstd_level: i32,

        #[arg(long, default_value_t = 4)]
        quiet_modules: i32,
    },

    Restore {
        input_pdf: PathBuf,

        #[arg(short, long, default_value = ".")]
        out_dir: PathBuf,

        #[arg(long, default_value_t = 300)]
        render_dpi: u32,

        #[arg(long)]
        session: Option<String>,

        #[arg(long)]
        output_name: Option<String>,

        // Layout hints so restore can crop deterministically even without manifest.
        // Defaults match backup defaults and your output (5x7 grid).
        #[arg(long, default_value_t = 35.0)]
        qr_mm: f32,
        #[arg(long, default_value_t = 8.0)]
        margin_mm: f32,
        #[arg(long, default_value_t = 2.0)]
        gap_mm: f32,
        #[arg(long, default_value_t = 5)]
        cols: i32,
        #[arg(long, default_value_t = 7)]
        rows: i32,

        /// Crop padding as fraction of QR size (0.08 = 8%).
        #[arg(long, default_value_t = 0.10)]
        crop_pad_frac: f32,

        /// Try inverted binarization as well
        #[arg(long, default_value_t = true)]
        try_invert: bool,
    },
}

// -------------------- SemVer --------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemVer {
    major: u32,
    minor: u32,
    patch: u32,
}
impl SemVer {
    fn parse(s: &str) -> Result<Self> {
        let mut parts = s.split('.');
        let major = parts
            .next()
            .ok_or_else(|| anyhow!("semver missing major"))?
            .parse::<u32>()
            .context("semver major parse")?;
        let minor = parts
            .next()
            .ok_or_else(|| anyhow!("semver missing minor"))?
            .parse::<u32>()
            .context("semver minor parse")?;
        let patch = parts
            .next()
            .unwrap_or("0")
            .parse::<u32>()
            .context("semver patch parse")?;
        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

// -------------------- Entry --------------------

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Backup {
            input,
            output,
            orientation,
            qr_mm,
            margin_mm,
            gap_mm,
            chunk_bytes,
            min_module_mm,
            zstd_level,
            quiet_modules,
        } => backup(
            &input,
            output.as_deref(),
            &orientation,
            qr_mm,
            margin_mm,
            gap_mm,
            chunk_bytes,
            min_module_mm,
            zstd_level,
            quiet_modules,
        ),
        Cmd::Restore {
            input_pdf,
            out_dir,
            render_dpi,
            session,
            output_name,
            qr_mm,
            margin_mm,
            gap_mm,
            cols,
            rows,
            crop_pad_frac,
            try_invert,
        } => restore(
            &input_pdf,
            &out_dir,
            render_dpi,
            session.as_deref(),
            output_name.as_deref(),
            LayoutHint {
                qr_mm,
                margin_mm,
                gap_mm,
                cols,
                rows,
                crop_pad_frac,
                try_invert,
            },
        ),
    }
}

// -------------------- Backup --------------------

fn backup(
    input: &Path,
    output: Option<&Path>,
    orientation: &str,
    qr_mm: f32,
    margin_mm: f32,
    gap_mm: f32,
    chunk_bytes_start: usize,
    min_module_mm: f32,
    zstd_level: i32,
    quiet_modules: i32,
) -> Result<()> {
    if quiet_modules < 0 {
        bail!("quiet_modules must be >= 0");
    }
    if qr_mm <= 0.0 {
        bail!("qr_mm must be > 0");
    }

    let data = fs::read(input).with_context(|| format!("read input '{}'", input.display()))?;
    let file_name = input
        .file_name()
        .ok_or_else(|| anyhow!("input has no file name"))?
        .to_string_lossy()
        .to_string();

    let sha256_hex = sha256_hex(&data);

    // compress (zstd frame is self-describing; decoder does not need level)
    let compressed = zstd::stream::encode_all(std::io::Cursor::new(&data), zstd_level)
        .context("zstd compress failed")?;

    let session_id = random_session_hex(8);
    let filename_b64 = URL_SAFE_NO_PAD.encode(file_name.as_bytes());

    // A4 size
    let (page_w_mm, page_h_mm) = if orientation == "landscape" {
        (297.0_f32, 210.0_f32)
    } else {
        (210.0_f32, 297.0_f32)
    };

    // Layout grid
    let usable_w_mm = page_w_mm - 2.0 * margin_mm;
    let usable_h_mm = page_h_mm - 2.0 * margin_mm;
    if usable_w_mm <= 0.0 || usable_h_mm <= 0.0 {
        bail!("margins too large for page");
    }

    let cell_mm = qr_mm + gap_mm;
    let cols = ((usable_w_mm + gap_mm) / cell_mm).floor() as i32;
    let rows = ((usable_h_mm + gap_mm) / cell_mm).floor() as i32;
    if cols <= 0 || rows <= 0 {
        bail!("qr_mm/margins/gap do not fit any QR on the page");
    }

    let per_page = (cols * rows) as usize;
    if per_page < 2 {
        bail!("grid too small: need at least 2 cells per page (one reserved for manifest)");
    }
    let data_per_page = per_page - 1;

    // Choose chunk_bytes adaptively so QR modules are not too small.
    let chunk_bytes = choose_chunk_bytes_adaptive(
        chunk_bytes_start.max(120),
        min_module_mm,
        qr_mm,
        quiet_modules,
        &session_id,
        compressed.len(),
    )?;

    let chunks: Vec<&[u8]> = compressed.chunks(chunk_bytes).collect();
    let total_chunks = chunks.len();
    if total_chunks == 0 {
        bail!("internal: no chunks created");
    }

    // Manifest payload (key=value, ignore unknown keys for minor/patch evolution)
    let manifest = format!(
        "{tag}|sid={sid}|prod={prod}|pver={pver}|sha256={sha}|len={len}|chunks={chunks}|name={name}|cmpr=zstd|chunk={chunk}|ecc=H|qz={qz}|page_w_mm={pw}|page_h_mm={ph}|qr_mm={qr}|margin_mm={m}|gap_mm={g}|cols={c}|rows={r}",
        tag = TAG_MANIFEST,
        sid = session_id,
        prod = PRODUCER,
        pver = PRODUCER_VERSION,
        sha = sha256_hex,
        len = data.len(),
        chunks = total_chunks,
        name = filename_b64,
        chunk = chunk_bytes,
        qz = quiet_modules,
        pw = trim_f32(page_w_mm),
        ph = trim_f32(page_h_mm),
        qr = trim_f32(qr_mm),
        m = trim_f32(margin_mm),
        g = trim_f32(gap_mm),
        c = cols,
        r = rows,
    );

    // Data payloads
    let mut data_payloads: Vec<String> = Vec::with_capacity(total_chunks);
    for (idx, ch) in chunks.iter().enumerate() {
        let mut crc = Crc32::new();
        crc.update(ch);
        let crc32 = crc.finalize();
        let b64 = URL_SAFE_NO_PAD.encode(ch);

        data_payloads.push(format!(
            "{tag}|sid={sid}|idx={idx}|total={total}|crc32={crc:08x}|data={data}",
            tag = TAG_DATA,
            sid = session_id,
            idx = idx,
            total = total_chunks,
            crc = crc32,
            data = b64
        ));
    }

    let out_path = output
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| input.with_extension("coldbackup.pdf"));

    // PDF settings
    let page_w_pt = page_w_mm * PT_PER_MM;
    let page_h_pt = page_h_mm * PT_PER_MM;
    let settings =
        PageSettings::from_wh(page_w_pt, page_h_pt).ok_or_else(|| anyhow!("invalid page size"))?;

    let mut doc = Document::new();
    let font = load_font()?;
    let total_pages = div_ceil(total_chunks, data_per_page);

    let mut page_index_1 = 1usize;
    // Page loop: manifest in cell 0 on every page
    let mut di = 0usize;
    while di < data_payloads.len() {
        let end = (di + data_per_page).min(data_payloads.len());
        let slice = &data_payloads[di..end];

        let chunk_start = di;
        let chunk_end_inclusive = end - 1;

        let mut page = doc.start_page_with(settings.clone());
        let mut surface = page.surface();

        // Labels zeichnen (Header/Footer)
        draw_page_labels(
            &mut surface,
            &font,
            page_h_pt,
            &session_id,
            PRODUCER,
            PRODUCER_VERSION,
            &sha256_hex,
            cols,
            rows,
            qr_mm,
            margin_mm,
            gap_mm,
            chunk_bytes,
            total_chunks,
            page_index_1,
            total_pages,
            chunk_start,
            chunk_end_inclusive,
        );

        // Cell 0: manifest
        place_qr_in_cell(
            &mut surface,
            &manifest,
            0,
            cols,
            margin_mm,
            cell_mm,
            qr_mm,
            quiet_modules,
        )?;

        // Cells 1..: data
        for (k, payload) in slice.iter().enumerate() {
            let cell_index = 1 + k;
            place_qr_in_cell(
                &mut surface,
                payload,
                cell_index,
                cols,
                margin_mm,
                cell_mm,
                qr_mm,
                quiet_modules,
            )?;
        }

        surface.finish();
        page.finish();
        di = end;
        page_index_1 += 1;
    }

    let pdf_bytes = doc.finish().map_err(|e| anyhow!("{e:?}"))?;
    fs::write(&out_path, pdf_bytes).with_context(|| format!("write '{}'", out_path.display()))?;

    eprintln!(
        "OK: wrote '{}' (session={}, sha256={})\n  producer={} {}\n  chunk_bytes={} -> total_chunks={}\n  grid={}x{} ({} per page, {} data/page + manifest)\n  qr_mm={} min_module_mm={}",
        out_path.display(),
        session_id,
        sha256_hex,
        PRODUCER,
        PRODUCER_VERSION,
        chunk_bytes,
        total_chunks,
        cols,
        rows,
        per_page,
        data_per_page,
        qr_mm,
        min_module_mm,
    );

    Ok(())
}

fn place_qr_in_cell(
    surface: &mut Surface<'_>,
    payload: &str,
    cell_index: usize,
    cols: i32,
    margin_mm: f32,
    cell_mm: f32,
    qr_mm: f32,
    quiet_modules: i32,
) -> Result<()> {
    let r = (cell_index as i32) / cols;
    let c = (cell_index as i32) % cols;

    let x_mm = margin_mm + (c as f32) * cell_mm;
    let y_mm = margin_mm + (r as f32) * cell_mm;

    let x_pt = x_mm * PT_PER_MM;
    let y_pt = y_mm * PT_PER_MM;
    let size_pt = qr_mm * PT_PER_MM;

    draw_qr_vector(
        surface,
        payload.as_bytes(),
        Point::from_xy(x_pt, y_pt),
        size_pt,
        quiet_modules,
    )?;
    Ok(())
}

/// Draw QR as vector rectangles (row runs)
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
            let end = x;

            let rx = ((start + quiet_modules) as f32) * module + top_left.x;
            let ry = ((y + quiet_modules) as f32) * module + top_left.y;
            let rw = ((end - start) as f32) * module;
            let rh = module;

            add_rect(&mut pb, rx, ry, rw, rh);
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

// -------------------- Restore --------------------

#[derive(Debug, Clone, Copy)]
struct LayoutHint {
    qr_mm: f32,
    margin_mm: f32,
    gap_mm: f32,
    cols: i32,
    rows: i32,
    crop_pad_frac: f32,
    try_invert: bool,
}

#[derive(Debug, Clone)]
struct Manifest {
    sid: String,
    prod: Option<String>,
    pver: Option<String>,
    sha256: Option<String>,
    len: Option<usize>,
    chunks: Option<usize>,
    filename: Option<String>,
    layout: Option<LayoutFromManifest>,
}

#[derive(Debug, Clone)]
struct LayoutFromManifest {
    page_w_mm: f32,
    page_h_mm: f32,
    qr_mm: f32,
    margin_mm: f32,
    gap_mm: f32,
    cols: i32,
    rows: i32,
}

#[derive(Debug, Clone)]
struct Extracted {
    manifest: Option<Manifest>,
    data_chunks: BTreeMap<usize, Vec<u8>>,
    total_chunks: usize,
}

fn restore(
    input_pdf: &Path,
    out_dir: &Path,
    render_dpi_start: u32,
    wanted_session: Option<&str>,
    output_name: Option<&str>,
    hint: LayoutHint,
) -> Result<()> {
    fs::create_dir_all(out_dir)
        .with_context(|| format!("create out dir '{}'", out_dir.display()))?;

    let pdf_bytes =
        fs::read(input_pdf).with_context(|| format!("read '{}'", input_pdf.display()))?;
    let pdf = Pdf::new(Arc::new(pdf_bytes))
        .map_err(|e| anyhow!("hayro_syntax: Pdf::new failed: {e:?}"))?;

    // During restore we silence panic output (rqrr sometimes panics internally),
    // but we also catch panics so the process never aborts.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let result = restore_inner(
        &pdf,
        out_dir,
        render_dpi_start,
        wanted_session,
        output_name,
        hint,
    );

    // Restore hook no matter what
    std::panic::set_hook(prev_hook);

    result
}

fn restore_inner(
    pdf: &Pdf,
    out_dir: &Path,
    render_dpi_start: u32,
    wanted_session: Option<&str>,
    output_name: Option<&str>,
    hint: LayoutHint,
) -> Result<()> {
    // Avoid extreme DPIs; 900 is typically enough for QR mosaics.
    let mut dpis = vec![render_dpi_start, 450, 600, 900];
    dpis.sort_unstable();
    dpis.dedup();

    let mut best: Option<(String, Extracted, u32)> = None;

    for dpi in dpis {
        // Phase A: try to get a manifest quickly (full-page scan) – optional.
        // Even if it fails, Phase B will still decode via grid hint.
        let decoded_full = decode_all_qr_fullpage(pdf, dpi).unwrap_or_default();
        let maybe_manifest = first_manifest(&decoded_full);

        // Choose layout:
        // - if we got a manifest with layout -> use exact
        // - else -> use CLI hint and infer A4 orientation from render dimensions per page
        let decoded_grid =
            decode_all_qr_grid(pdf, dpi, hint, maybe_manifest.as_ref()).unwrap_or_default();

        // Merge results (dedup not strictly needed, but helps)
        let mut decoded = decoded_full;
        decoded.extend(decoded_grid);
        if decoded.is_empty() {
            eprintln!("dpi={dpi}: no {TAG_MANIFEST}/{TAG_DATA} payloads found");
            continue;
        }

        let sessions_found = find_sessions(&decoded);
        if sessions_found.is_empty() {
            eprintln!("dpi={dpi}: no sessions found");
            continue;
        }
        let chosen_session = choose_session(&sessions_found, wanted_session)?;

        let extracted = match extract_backup(&decoded, &chosen_session) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("dpi={dpi}: extraction error: {e:#}");
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

        if best.as_ref().map(|b| b.1.data_chunks.len()).unwrap_or(0) < have {
            best = Some((chosen_session.clone(), extracted.clone(), dpi));
        }

        if have == total {
            return reconstruct_and_write(out_dir, &chosen_session, extracted, output_name);
        }
    }

    if let Some((sid, extracted, dpi)) = best {
        bail!(
            "best attempt dpi={dpi} session={sid}: missing {} chunks (of {})",
            extracted
                .total_chunks
                .saturating_sub(extracted.data_chunks.len()),
            extracted.total_chunks
        );
    }

    bail!("no valid backup found in PDF");
}

/// Full-page scan (best-effort). Not reliable alone for huge mosaics, but often finds at least manifest.
fn decode_all_qr_fullpage(pdf: &Pdf, dpi: u32) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for page in pdf.pages().iter() {
        let gray = render_page_gray(page, dpi)?;
        let bin = binarize_otsu(&gray);

        if let Some(txts) = decode_many_with_rqrr_safe(bin) {
            out.extend(txts);
        }
    }
    Ok(out)
}

/// Extract first manifest payload if present.
fn first_manifest(decoded: &[String]) -> Option<Manifest> {
    for s in decoded {
        if s.starts_with(TAG_MANIFEST) {
            if let Ok(m) = parse_manifest_payload(s) {
                return Some(m);
            }
        }
    }
    None
}

/// Grid-crop decode: deterministic per-cell cropping.
/// Tries BOTH Y-mappings (top-left vs bottom-left) and picks the variant that yields more decodes per page.
fn decode_all_qr_grid(
    pdf: &Pdf,
    dpi: u32,
    hint: LayoutHint,
    manifest: Option<&Manifest>,
) -> Result<Vec<String>> {
    if hint.cols <= 0 || hint.rows <= 0 {
        bail!("invalid --cols/--rows");
    }
    let mut out = Vec::new();

    for page in pdf.pages().iter() {
        let gray = render_page_gray(page, dpi)?;
        let bin = binarize_otsu(&gray);
        let bin_inv = if hint.try_invert {
            Some(invert_gray(&bin))
        } else {
            None
        };

        // Determine page mm for mapping:
        // - if manifest has page_w_mm/page_h_mm -> use it
        // - else infer orientation from rendered image and use A4 mm
        let (page_w_mm, page_h_mm, qr_mm, margin_mm, gap_mm, cols, rows) = if let Some(m) = manifest
        {
            if let Some(l) = &m.layout {
                (
                    l.page_w_mm,
                    l.page_h_mm,
                    l.qr_mm,
                    l.margin_mm,
                    l.gap_mm,
                    l.cols,
                    l.rows,
                )
            } else {
                a4_from_image(&bin)
                    .map(|(w, h)| {
                        (
                            w,
                            h,
                            hint.qr_mm,
                            hint.margin_mm,
                            hint.gap_mm,
                            hint.cols,
                            hint.rows,
                        )
                    })
                    .unwrap()
            }
        } else {
            a4_from_image(&bin)
                .map(|(w, h)| {
                    (
                        w,
                        h,
                        hint.qr_mm,
                        hint.margin_mm,
                        hint.gap_mm,
                        hint.cols,
                        hint.rows,
                    )
                })
                .unwrap()
        };

        if cols <= 0 || rows <= 0 {
            continue;
        }
        let per_page = (cols * rows) as usize;

        let img_w = bin.width() as f32;
        let img_h = bin.height() as f32;

        let mm_to_px_x = img_w / page_w_mm;
        let mm_to_px_y = img_h / page_h_mm;

        let cell_mm = qr_mm + gap_mm;

        let qr_w_px = qr_mm * mm_to_px_x;
        let qr_h_px = qr_mm * mm_to_px_y;

        let pad_px_x = (hint.crop_pad_frac.max(0.0) * qr_w_px).max(2.0);
        let pad_px_y = (hint.crop_pad_frac.max(0.0) * qr_h_px).max(2.0);

        // Try both Y mappings; count success for each and pick best.
        let mut page_hits_a: Vec<String> = Vec::new();
        let mut page_hits_b: Vec<String> = Vec::new();

        for cell in 0..per_page {
            let r = (cell as i32) / cols;
            let c = (cell as i32) % cols;

            let x_mm = margin_mm + (c as f32) * cell_mm;
            let y_mm_from_top = margin_mm + (r as f32) * cell_mm;

            let x_px = x_mm * mm_to_px_x;

            // A: interpret y_mm_from_top as "distance from top" (image origin top-left)
            let y_px_top_a = y_mm_from_top * mm_to_px_y;

            // B: interpret y_mm_from_top as used in PDF bottom-left (flip to top)
            let y_px_top_b = (page_h_mm - (y_mm_from_top + qr_mm)) * mm_to_px_y;

            // Try A
            if let Some(text) = decode_cell(
                &bin,
                bin_inv.as_ref(),
                x_px,
                y_px_top_a,
                qr_w_px,
                qr_h_px,
                pad_px_x,
                pad_px_y,
            ) {
                page_hits_a.push(text);
            }

            // Try B
            if let Some(text) = decode_cell(
                &bin,
                bin_inv.as_ref(),
                x_px,
                y_px_top_b,
                qr_w_px,
                qr_h_px,
                pad_px_x,
                pad_px_y,
            ) {
                page_hits_b.push(text);
            }
        }

        // Pick the better mapping for this page
        if page_hits_b.len() > page_hits_a.len() {
            out.extend(page_hits_b);
        } else {
            out.extend(page_hits_a);
        }
    }

    Ok(out)
}

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
    let img_w = bin.width() as f32;
    let img_h = bin.height() as f32;

    let (x0, y0, w, h) = clamp_crop(
        x_px - pad_px_x,
        y_px_top - pad_px_y,
        x_px + qr_w_px + pad_px_x,
        y_px_top + qr_h_px + pad_px_y,
        img_w,
        img_h,
    );

    if w < 24 || h < 24 {
        return None;
    }

    let crop = imageops::crop_imm(bin, x0, y0, w, h).to_image();

    // Skip super-uniform crops (prevents rqrr edgecases and saves time)
    if is_low_contrast(&crop) {
        return None;
    }

    if let Some(text) = decode_single_qr_safe(crop) {
        return Some(text);
    }
    if let Some(inv) = bin_inv {
        let crop2 = imageops::crop_imm(inv, x0, y0, w, h).to_image();
        if is_low_contrast(&crop2) {
            return None;
        }
        if let Some(text) = decode_single_qr_safe(crop2) {
            return Some(text);
        }
    }
    None
}

/// rqrr sometimes panics internally on certain inputs; we must never crash restore.
fn decode_single_qr_safe(gray: GrayImage) -> Option<String> {
    let res = catch_unwind(AssertUnwindSafe(|| {
        let mut prep = rqrr::PreparedImage::prepare(gray);
        for g in prep.detect_grids() {
            if let Ok((_meta, text)) = g.decode() {
                return Some(text);
            }
        }
        None
    }));
    match res {
        Ok(v) => v,
        Err(_) => None,
    }
}

/// Fullpage: decode many; also panic-safe.
fn decode_many_with_rqrr_safe(gray: GrayImage) -> Option<Vec<String>> {
    let res = catch_unwind(AssertUnwindSafe(|| {
        let mut out = Vec::new();
        let mut prep = rqrr::PreparedImage::prepare(gray);
        for g in prep.detect_grids() {
            if let Ok((_meta, text)) = g.decode() {
                out.push(text);
            }
        }
        out
    }));
    match res {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn a4_from_image(img: &GrayImage) -> Option<(f32, f32)> {
    let w = img.width() as f32;
    let h = img.height() as f32;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    if w > h {
        Some((297.0, 210.0))
    } else {
        Some((210.0, 297.0))
    }
}

fn clamp_crop(x0: f32, y0: f32, x1: f32, y1: f32, img_w: f32, img_h: f32) -> (u32, u32, u32, u32) {
    let x0 = x0.max(0.0).min(img_w);
    let y0 = y0.max(0.0).min(img_h);
    let x1 = x1.max(0.0).min(img_w);
    let y1 = y1.max(0.0).min(img_h);

    let w = (x1 - x0).max(0.0) as u32;
    let h = (y1 - y0).max(0.0) as u32;

    (x0 as u32, y0 as u32, w, h)
}

fn is_low_contrast(img: &GrayImage) -> bool {
    let mut minv = 255u8;
    let mut maxv = 0u8;
    // sample a subset for speed
    let step_x = (img.width() / 32).max(1);
    let step_y = (img.height() / 32).max(1);
    for y in (0..img.height()).step_by(step_y as usize) {
        for x in (0..img.width()).step_by(step_x as usize) {
            let v = img.get_pixel(x, y)[0];
            if v < minv {
                minv = v;
            }
            if v > maxv {
                maxv = v;
            }
            if maxv.saturating_sub(minv) > 40 {
                return false;
            }
        }
    }
    true
}

fn render_page_gray(page: &hayro::hayro_syntax::page::Page, dpi: u32) -> Result<GrayImage> {
    let scale = (dpi as f32) / 72.0;
    let settings = hayro::RenderSettings {
        x_scale: scale,
        y_scale: scale,
        bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        ..Default::default()
    };
    let interp = hayro::hayro_interpret::InterpreterSettings::default();
    let pixmap = hayro::render(page, &interp, &settings);
    Ok(pixmap_to_gray(&pixmap))
}

fn pixmap_to_gray(pixmap: &hayro::vello_cpu::Pixmap) -> GrayImage {
    let w = pixmap.width() as u32;
    let h = pixmap.height() as u32;
    let rgba = pixmap.data_as_u8_slice();

    let mut out = GrayImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let r = rgba[i] as u32;
            let g = rgba[i + 1] as u32;
            let b = rgba[i + 2] as u32;
            let y8 = ((r * 2126 + g * 7152 + b * 722) / 10000) as u8;
            out.put_pixel(x, y, Luma([y8]));
        }
    }
    out
}

/// Otsu threshold binarization
fn binarize_otsu(gray: &GrayImage) -> GrayImage {
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
    for (x, y, p) in gray.enumerate_pixels() {
        let v = if p[0] <= threshold { 0u8 } else { 255u8 };
        out.put_pixel(x, y, Luma([v]));
    }
    out
}

fn invert_gray(img: &GrayImage) -> GrayImage {
    let mut out = img.clone();
    for p in out.pixels_mut() {
        p[0] = 255u8.wrapping_sub(p[0]);
    }
    out
}

fn find_sessions(decoded: &[String]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for s in decoded {
        if let Some(sid) = parse_session_id(s) {
            out.insert(sid);
        }
    }
    out
}

fn choose_session(sessions_found: &BTreeSet<String>, wanted: Option<&str>) -> Result<String> {
    if let Some(w) = wanted {
        if sessions_found.contains(w) {
            return Ok(w.to_string());
        }
        bail!(
            "session '{}' not found. sessions in PDF: {:?}",
            w,
            sessions_found
        );
    }
    if sessions_found.len() == 1 {
        Ok(sessions_found.iter().next().unwrap().clone())
    } else {
        bail!(
            "multiple sessions found: {:?}. Use --session <hex>.",
            sessions_found
        );
    }
}

fn parse_session_id(s: &str) -> Option<String> {
    let mut it = s.split('|');
    let tag = it.next()?;
    if tag != TAG_MANIFEST && tag != TAG_DATA {
        return None;
    }
    for token in it {
        if let Some(v) = token.strip_prefix("sid=") {
            return Some(v.to_string());
        }
    }
    None
}

fn parse_kv_payload(s: &str) -> Option<(&str, HashMap<String, String>)> {
    let mut it = s.split('|');
    let tag = it.next()?;
    if tag != TAG_MANIFEST && tag != TAG_DATA {
        return None;
    }
    let mut map = HashMap::new();
    for token in it {
        if let Some((k, v)) = token.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    Some((tag, map))
}

fn parse_manifest_payload(s: &str) -> Result<Manifest> {
    let (tag, kv) = parse_kv_payload(s).ok_or_else(|| anyhow!("not a kv payload"))?;
    if tag != TAG_MANIFEST {
        bail!("not a manifest payload");
    }

    let sid = kv
        .get("sid")
        .ok_or_else(|| anyhow!("manifest missing sid"))?
        .to_string();
    let fmt_str = kv
        .get("fmt")
        .ok_or_else(|| anyhow!("manifest missing fmt"))?;
    let fmt = SemVer::parse(fmt_str)?;
    if fmt.major != SUPPORTED_FORMAT_MAJOR {
        bail!(
            "unsupported format major {} (this decoder supports {}.x.y)",
            fmt.major,
            SUPPORTED_FORMAT_MAJOR
        );
    }

    let prod = kv.get("prod").cloned();
    let pver = kv.get("pver").cloned();
    let sha256 = kv.get("sha256").cloned();
    let len = kv.get("len").and_then(|x| x.parse::<usize>().ok());
    let chunks = kv.get("chunks").and_then(|x| x.parse::<usize>().ok());
    let filename = if let Some(name_b64) = kv.get("name") {
        URL_SAFE_NO_PAD
            .decode(name_b64)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
    } else {
        None
    };

    let layout = {
        let page_w_mm = kv.get("page_w_mm").and_then(|x| x.parse::<f32>().ok());
        let page_h_mm = kv.get("page_h_mm").and_then(|x| x.parse::<f32>().ok());
        let qr_mm = kv.get("qr_mm").and_then(|x| x.parse::<f32>().ok());
        let margin_mm = kv.get("margin_mm").and_then(|x| x.parse::<f32>().ok());
        let gap_mm = kv.get("gap_mm").and_then(|x| x.parse::<f32>().ok());
        let cols = kv.get("cols").and_then(|x| x.parse::<i32>().ok());
        let rows = kv.get("rows").and_then(|x| x.parse::<i32>().ok());
        match (page_w_mm, page_h_mm, qr_mm, margin_mm, gap_mm, cols, rows) {
            (Some(pw), Some(ph), Some(qr), Some(m), Some(g), Some(c), Some(r)) => {
                Some(LayoutFromManifest {
                    page_w_mm: pw,
                    page_h_mm: ph,
                    qr_mm: qr,
                    margin_mm: m,
                    gap_mm: g,
                    cols: c,
                    rows: r,
                })
            }
            _ => None,
        }
    };

    Ok(Manifest {
        sid,
        prod,
        pver,
        sha256,
        len,
        chunks,
        filename,
        layout,
    })
}

fn parse_data_payload(s: &str) -> Result<(String, usize, usize, Vec<u8>)> {
    let (tag, kv) = parse_kv_payload(s).ok_or_else(|| anyhow!("not a kv payload"))?;
    if tag != TAG_DATA {
        bail!("not a data payload");
    }

    let sid = kv
        .get("sid")
        .ok_or_else(|| anyhow!("data missing sid"))?
        .to_string();
    let idx = kv
        .get("idx")
        .ok_or_else(|| anyhow!("data missing idx"))?
        .parse::<usize>()
        .context("idx parse")?;
    let total = kv
        .get("total")
        .ok_or_else(|| anyhow!("data missing total"))?
        .parse::<usize>()
        .context("total parse")?;

    let crc_expected = kv
        .get("crc32")
        .ok_or_else(|| anyhow!("data missing crc32"))?;
    let crc_expected = u32::from_str_radix(crc_expected, 16).context("crc32 parse")?;

    let data_b64 = kv.get("data").ok_or_else(|| anyhow!("data missing data"))?;
    let bytes = URL_SAFE_NO_PAD.decode(data_b64).context("base64 decode")?;

    let mut crc = Crc32::new();
    crc.update(&bytes);
    let got = crc.finalize();
    if got != crc_expected {
        bail!(
            "crc32 mismatch for chunk {}: expected {:08x}, got {:08x}",
            idx,
            crc_expected,
            got
        );
    }

    Ok((sid, idx, total, bytes))
}

fn extract_backup(decoded: &[String], chosen_session: &str) -> Result<Extracted> {
    let mut manifest: Option<Manifest> = None;
    let mut chunks: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    let mut total_expected: Option<usize> = None;

    for s in decoded {
        if !s.contains(chosen_session) {
            continue;
        }

        if s.starts_with(TAG_MANIFEST) {
            if let Ok(m) = parse_manifest_payload(s) {
                if m.sid == chosen_session {
                    total_expected = m.chunks.or(total_expected);
                    // prefer one with layout (if present)
                    match (&manifest, &m.layout) {
                        (Some(prev), Some(_)) if prev.layout.is_none() => manifest = Some(m),
                        (None, _) => manifest = Some(m),
                        _ => {}
                    }
                }
            }
        } else if s.starts_with(TAG_DATA) {
            let (sid, idx, total, bytes) = parse_data_payload(s)?;
            if sid != chosen_session {
                continue;
            }

            if total_expected.is_none() {
                total_expected = Some(total);
            } else if total_expected != Some(total) {
                bail!("conflicting total values for session {}", chosen_session);
            }

            if let Some(existing) = chunks.get(&idx) {
                if existing.as_slice() != bytes.as_slice() {
                    bail!("conflicting data for idx {}", idx);
                }
            } else {
                chunks.insert(idx, bytes);
            }
        }
    }

    let total_chunks = total_expected
        .ok_or_else(|| anyhow!("no data chunks found for session {}", chosen_session))?;
    Ok(Extracted {
        manifest,
        data_chunks: chunks,
        total_chunks,
    })
}

fn reconstruct_and_write(
    out_dir: &Path,
    session: &str,
    extracted: Extracted,
    output_name: Option<&str>,
) -> Result<()> {
    let missing = extracted
        .total_chunks
        .saturating_sub(extracted.data_chunks.len());
    if missing != 0 {
        bail!("missing {} chunks (of {})", missing, extracted.total_chunks);
    }

    let mut compressed: Vec<u8> = Vec::new();
    for i in 0..extracted.total_chunks {
        compressed.extend_from_slice(extracted.data_chunks.get(&i).unwrap());
    }

    let restored = zstd::stream::decode_all(std::io::Cursor::new(&compressed))
        .context("zstd decompress failed")?;
    let restored_sha = sha256_hex(&restored);

    let filename = if let Some(name) = output_name {
        name.to_string()
    } else if let Some(m) = &extracted.manifest {
        m.filename
            .clone()
            .unwrap_or_else(|| format!("restored-{}.bin", session))
    } else {
        format!("restored-{}.bin", session)
    };
    let out_file = out_dir.join(sanitize_filename(&filename));
    fs::write(&out_file, &restored).with_context(|| format!("write '{}'", out_file.display()))?;

    let sha_path = out_dir.join(format!(
        "{}.sha256",
        out_file.file_name().unwrap().to_string_lossy()
    ));
    fs::write(
        &sha_path,
        format!(
            "{}  {}\n",
            restored_sha,
            out_file.file_name().unwrap().to_string_lossy()
        ),
    )
    .with_context(|| format!("write '{}'", sha_path.display()))?;

    if let Some(m) = &extracted.manifest {
        if let Some(exp_len) = m.len {
            if restored.len() != exp_len {
                bail!(
                    "length mismatch: expected {}, got {}",
                    exp_len,
                    restored.len()
                );
            }
        }
        if let Some(exp_sha) = &m.sha256 {
            if &restored_sha != exp_sha {
                bail!(
                    "sha256 mismatch: expected {}, got {}",
                    exp_sha,
                    restored_sha
                );
            }
        }
        eprintln!("OK: restored '{}' (verified)", out_file.display());
    } else {
        eprintln!(
            "OK: restored '{}' (no manifest decoded; sha256={})",
            out_file.display(),
            restored_sha
        );
    }

    Ok(())
}

// -------------------- Misc helpers --------------------

fn choose_chunk_bytes_adaptive(
    mut chunk_bytes: usize,
    min_module_mm: f32,
    qr_mm: f32,
    quiet_modules: i32,
    session_id: &str,
    compressed_len: usize,
) -> Result<usize> {
    if min_module_mm <= 0.0 {
        return Ok(chunk_bytes);
    }
    let min_chunk = 120usize;
    if chunk_bytes < min_chunk {
        chunk_bytes = min_chunk;
    }

    loop {
        let total_chunks = div_ceil(compressed_len.max(1), chunk_bytes).max(1);

        let fake = vec![0u8; chunk_bytes];
        let b64 = URL_SAFE_NO_PAD.encode(&fake);
        let payload = format!(
            "{tag}|sid={sid}|idx=0|total={total}|crc32=00000000|data={data}",
            tag = TAG_DATA,
            sid = session_id,
            total = total_chunks,
            data = b64
        );

        match module_mm_for_payload(&payload, qr_mm, quiet_modules) {
            Ok(mm) if mm >= min_module_mm => return Ok(chunk_bytes),
            _ => {
                if chunk_bytes <= min_chunk {
                    return Ok(chunk_bytes);
                }
                chunk_bytes = ((chunk_bytes as f32) * 0.9) as usize;
                if chunk_bytes < min_chunk {
                    chunk_bytes = min_chunk;
                }
            }
        }
    }
}

fn module_mm_for_payload(payload: &str, qr_mm: f32, quiet_modules: i32) -> Result<f32> {
    let code = QrCode::encode_binary(payload.as_bytes(), QrCodeEcc::High)
        .map_err(|_| anyhow!("payload too long for ECC=H"))?;
    let n = code.size();
    let total = n + 2 * quiet_modules;
    if total <= 0 {
        bail!("invalid QR module total");
    }
    Ok(qr_mm / (total as f32))
}

fn div_ceil(n: usize, d: usize) -> usize {
    (n + d - 1) / d
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn random_session_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

fn sanitize_filename(name: &str) -> String {
    name.replace(['/', '\\', '\0'], "_")
}

fn trim_f32(v: f32) -> String {
    let s = format!("{:.3}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
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

    // krilla::Data aus Arc<dyn AsRef<[u8]> + Send + Sync> bauen
    let arc: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(bytes);
    let data: krilla::Data = arc.into();

    // TTC/OTC: mehrere Indizes probieren, TTF/OTF meist index 0
    for idx in 0..8u32 {
        if let Some(f) = Font::new(data.clone(), idx) {
            return Ok(f);
        }
    }

    Err(anyhow!(
        "krilla: could not load system font (indices 0..7 failed)"
    ))
}

fn draw_page_labels(
    surface: &mut krilla::surface::Surface<'_>,
    font: &krilla::text::Font,
    page_h_pt: f32,
    sid: &str,
    producer: &str,
    producer_ver: &str,
    sha256_hex: &str,
    cols: i32,
    rows: i32,
    qr_mm: f32,
    margin_mm: f32,
    gap_mm: f32,
    chunk_bytes: usize,
    total_chunks: usize,
    page_idx_1: usize,
    total_pages: usize,
    chunk_start: usize,
    chunk_end_inclusive: usize,
) {
    use krilla::color::rgb;
    use krilla::geom::Point;
    use krilla::num::NormalizedF32;
    use krilla::paint::{Fill, FillRule};
    use krilla::text::TextDirection;

    surface.set_fill(Some(Fill {
        paint: rgb::Color::new(0, 0, 0).into(),
        opacity: NormalizedF32::ONE,
        rule: FillRule::NonZero,
    }));

    let left = 8.0_f32; // pt

    // ✅ Y=0 ist oben, Y steigt nach unten (hier so behandelt)
    let header_y = 12.0_f32; // pt (oben)
    let footer1_y = page_h_pt - 16.0; // pt (unten, Zeile 1)
    let footer2_y = page_h_pt - 28.0;
    let footer3_y = page_h_pt - 40.0;

    let sha_short = if sha256_hex.len() > 16 {
        &sha256_hex[..16]
    } else {
        sha256_hex
    };

    let header = format!("{} {} | sid {}", producer, producer_ver, sid);

    let footer1 = format!(
        "page {}/{} | grid {}x{} | chunks {}–{} | chunk_bytes {} | total_chunks {}",
        page_idx_1,
        total_pages,
        cols,
        rows,
        chunk_start,
        chunk_end_inclusive,
        chunk_bytes,
        total_chunks
    );

    let footer2 = format!(
        "sha256 {}… | restore: coldbackup restore <pdf> --cols {} --rows {} --qr-mm {} --margin-mm {} --gap-mm {}",
        sha_short,
        cols,
        rows,
        trim_f32(qr_mm),
        trim_f32(margin_mm),
        trim_f32(gap_mm)
    );

    // Header (oben)
    surface.draw_text(
        Point::from_xy(left, header_y),
        font.clone(),
        8.5,
        &header,
        false,
        TextDirection::Auto,
    );

    //let footer3 = "source: https://example.com/git/coldbackup".to_string();
    //// Footer (unten)
    //surface.draw_text(
    //    Point::from_xy(left, footer3_y),
    //    font.clone(),
    //    8.0,
    //    &footer3,
    //    false,
    //    TextDirection::Auto,
    //);
    surface.draw_text(
        Point::from_xy(left, footer2_y),
        font.clone(),
        8.0,
        &footer2,
        false,
        TextDirection::Auto,
    );
    surface.draw_text(
        Point::from_xy(left, footer1_y),
        font.clone(),
        8.0,
        &footer1,
        false,
        TextDirection::Auto,
    );
}
