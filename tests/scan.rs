//! Restoring from scans rather than from the generated PDF.
//!
//! A real scan is never the PDF: it is rotated a degree or two, shifted, scaled a little
//! wrong, lit unevenly, softened by the optics and chewed by JPEG. These tests build
//! exactly that out of the rendered sheets and require the file back byte for byte.

mod common;

use coldbackup::{BackupOptions, RestoreOptions, backup, render_sheets, restore};
use common::{TempDir, pseudo_random};
use image::{GrayImage, Luma, imageops};
use std::fs;
use std::path::{Path, PathBuf};

/// Large codes and few per sheet: this is what someone printing for the long term would
/// choose anyway, and it keeps the test's rendering cost sane.
fn options() -> BackupOptions {
    BackupOptions {
        qr_mm: 90.0,
        chunk_bytes: 300,
        ..Default::default()
    }
}

// -------------------- Scanner simulation --------------------

/// Rotate about the centre with bilinear sampling, on a white ground.
///
/// Nobody feeds a page into a scanner perfectly square, and a degree of skew is what
/// defeats any attempt to crop cells by computed geometry.
fn rotate(src: &GrayImage, degrees: f32) -> GrayImage {
    let (w, h) = (src.width(), src.height());
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let (sin, cos) = (-degrees.to_radians()).sin_cos();

    let mut out = GrayImage::from_pixel(w, h, Luma([255]));
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = (x as f32 - cx, y as f32 - cy);
            let sx = cos * dx - sin * dy + cx;
            let sy = sin * dx + cos * dy + cy;
            if sx < 0.0 || sy < 0.0 || sx >= (w - 1) as f32 || sy >= (h - 1) as f32 {
                continue;
            }

            let (x0, y0) = (sx.floor() as u32, sy.floor() as u32);
            let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
            let p = |xx: u32, yy: u32| src.get_pixel(xx, yy)[0] as f32;
            let top = p(x0, y0) * (1.0 - fx) + p(x0 + 1, y0) * fx;
            let bottom = p(x0, y0 + 1) * (1.0 - fx) + p(x0 + 1, y0 + 1) * fx;
            out.put_pixel(x, y, Luma([(top * (1.0 - fy) + bottom * fy) as u8]));
        }
    }
    out
}

/// Uneven illumination: bright on one side, falling off to `min_gain` on the other.
/// A global threshold cannot handle this, which is why the scan path leaves the
/// thresholding to the detector.
fn lighting_gradient(src: &GrayImage, min_gain: f32) -> GrayImage {
    let w = src.width() as f32;
    let mut out = src.clone();
    for (x, _, p) in out.enumerate_pixels_mut() {
        let gain = min_gain + (1.0 - min_gain) * (x as f32 / w);
        p[0] = (p[0] as f32 * gain).clamp(0.0, 255.0) as u8;
    }
    out
}

/// Shift the page within the frame, as a sheet laid down off-centre.
fn offset(src: &GrayImage, dx: i32, dy: i32) -> GrayImage {
    let (w, h) = (src.width(), src.height());
    let mut out = GrayImage::from_pixel(w, h, Luma([255]));
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = (x as i32 - dx, y as i32 - dy);
            if sx >= 0 && sy >= 0 && (sx as u32) < w && (sy as u32) < h {
                out.put_pixel(x, y, *src.get_pixel(sx as u32, sy as u32));
            }
        }
    }
    out
}

/// A JPEG round trip at a middling quality, for the block artefacts.
fn jpeg_cycle(src: &GrayImage, quality: u8) -> GrayImage {
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality)
        .encode(src.as_raw(), src.width(), src.height(), image::ExtendedColorType::L8)
        .expect("jpeg encode");
    image::load_from_memory(&buf).expect("jpeg decode").to_luma8()
}

/// Everything a flatbed does to a page at once.
fn simulate_flatbed(sheet: &GrayImage) -> GrayImage {
    let skewed = rotate(sheet, 1.4);
    let shifted = offset(&skewed, 23, -17);
    // Slightly wrong scale, as if scanned at a hair off the nominal DPI.
    let rescaled = imageops::resize(
        &shifted,
        (shifted.width() as f32 * 0.97) as u32,
        (shifted.height() as f32 * 0.97) as u32,
        imageops::FilterType::Triangle,
    );
    let lit = lighting_gradient(&rescaled, 0.62);
    let softened = imageops::blur(&lit, 0.8);
    jpeg_cycle(&softened, 72)
}

// -------------------- Helpers --------------------

fn write_backup(tmp: &TempDir, bytes: &[u8]) -> PathBuf {
    let input = tmp.join("archive.bin");
    let pdf = tmp.join("backup.pdf");
    fs::write(&input, bytes).unwrap();
    backup(&input, Some(&pdf), &options()).expect("backup");
    pdf
}

/// Render the sheets and run each through `degrade`, writing them out as a scan folder.
fn make_scans(pdf: &Path, dir: &Path, dpi: u32, degrade: impl Fn(&GrayImage) -> GrayImage) {
    fs::create_dir_all(dir).unwrap();
    for (i, sheet) in render_sheets(pdf, dpi).expect("render").iter().enumerate() {
        degrade(sheet)
            .save(dir.join(format!("sheet-{:02}.png", i + 1)))
            .expect("write scan");
    }
}

fn restore_from(input: &Path, out: &Path) -> anyhow::Result<Vec<u8>> {
    let opts = RestoreOptions {
        out_dir: out.to_path_buf(),
        ..Default::default()
    };
    let report = restore(input, &opts)?;
    assert!(report.verified, "a scan restore must still verify its sha256");
    Ok(fs::read(report.out_file)?)
}

// -------------------- Tests --------------------

#[test]
fn a_clean_300dpi_scan_restores() {
    let tmp = TempDir::new("scan-clean");
    let original = pseudo_random(2000);
    let pdf = write_backup(&tmp, &original);

    let scans = tmp.join("scans");
    make_scans(&pdf, &scans, 300, |s| s.clone());

    assert_eq!(restore_from(&scans, &tmp.join("out")).unwrap(), original);
}

/// The headline case: skew, offset, wrong scale, uneven light, blur and JPEG together.
#[test]
fn a_realistically_degraded_scan_restores() {
    let tmp = TempDir::new("scan-degraded");
    let original = pseudo_random(2000);
    let pdf = write_backup(&tmp, &original);

    let scans = tmp.join("scans");
    make_scans(&pdf, &scans, 400, simulate_flatbed);

    assert_eq!(restore_from(&scans, &tmp.join("out")).unwrap(), original);
}

/// Sheets come back from a scanner as separate files; a single one is a valid input too.
#[test]
fn a_single_sheet_image_is_accepted() {
    let tmp = TempDir::new("scan-single");
    // Small enough to fit on one sheet.
    let original = pseudo_random(600);
    let pdf = write_backup(&tmp, &original);

    let sheets = render_sheets(&pdf, 300).unwrap();
    assert_eq!(sheets.len(), 1, "test expects a one-sheet backup");
    let path = tmp.join("only-sheet.png");
    rotate(&sheets[0], -0.9).save(&path).unwrap();

    assert_eq!(restore_from(&path, &tmp.join("out")).unwrap(), original);
}

/// A missing sheet is the everyday accident, and parity is what makes it survivable —
/// through the scan path just as through the PDF.
#[test]
fn a_missing_sheet_is_repaired_from_parity() {
    let tmp = TempDir::new("scan-missing-sheet");
    let original = pseudo_random(2000);

    let input = tmp.join("archive.bin");
    let pdf = tmp.join("backup.pdf");
    fs::write(&input, &original).unwrap();
    let report = backup(
        &input,
        Some(&pdf),
        &BackupOptions {
            // Enough parity that one sheet of loss is covered.
            parity_frac: 0.6,
            ..options()
        },
    )
    .expect("backup");
    assert!(report.total_pages > 1);
    assert!(
        report.survives_sheet_loss,
        "this configuration does not claim to survive a lost sheet"
    );

    let scans = tmp.join("scans");
    make_scans(&pdf, &scans, 400, simulate_flatbed);

    // The dog ate sheet 2.
    fs::remove_file(scans.join("sheet-02.png")).unwrap();

    assert_eq!(restore_from(&scans, &tmp.join("out")).unwrap(), original);
}

/// The default sheet layout: 35 mm codes packed 5x7, which is far denser than the
/// large-code layout the other tests use, and the one most sheets will actually have.
/// Its 0.31 mm modules need a 600 dpi scan — at 300 dpi they are under four pixels wide.
#[test]
fn the_default_dense_layout_restores_from_a_600dpi_scan() {
    let tmp = TempDir::new("scan-dense");
    let original = pseudo_random(6000);

    let input = tmp.join("archive.bin");
    let pdf = tmp.join("backup.pdf");
    fs::write(&input, &original).unwrap();
    let report = backup(&input, Some(&pdf), &BackupOptions::default()).expect("backup");
    assert_eq!((report.layout.cols, report.layout.rows), (5, 7));

    let scans = tmp.join("scans");
    make_scans(&pdf, &scans, 600, |s| {
        // Skew and light unevenness, but no downscaling: resolution is the one thing a
        // dense sheet cannot spare.
        lighting_gradient(&rotate(s, 1.1), 0.7)
    });

    assert_eq!(restore_from(&scans, &tmp.join("out")).unwrap(), original);
}

#[test]
fn a_folder_without_images_is_reported_clearly() {
    let tmp = TempDir::new("scan-empty");
    let empty = tmp.join("empty");
    fs::create_dir_all(&empty).unwrap();

    let err = restore_from(&empty, &tmp.join("out")).expect_err("should fail");
    assert!(
        format!("{err:#}").contains("no images found"),
        "unexpected error: {err:#}"
    );
}
