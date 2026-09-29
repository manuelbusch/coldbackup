//! Golden-file tests: sheets produced by earlier versions must stay restorable.
//!
//! These PDFs stand in for paper already sitting in a drawer. A backup format is only
//! worth anything if a future build can still read what an old build wrote, so the
//! files in `tests/golden/` are **frozen**: when one of these tests fails, the fix
//! belongs in the decoder, not in the fixture. Regenerating a golden file silently
//! discards the only evidence that old sheets are still readable.

mod common;

use common::{TempDir, run_ok};
use std::fs;
use std::path::PathBuf;

fn golden(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

/// Restore a golden PDF with **default** flags and check it byte for byte.
///
/// Defaults are deliberate: anything the decoder needs beyond them has to come out of
/// the manifest on the sheet, which is all a future user will have.
fn assert_golden_restores(pdf: &str, expected_file: &str, expected_name: &str) {
    let tmp = TempDir::new(&format!("golden-{pdf}"));
    let out = tmp.join("out");

    let stderr = run_ok(&[
        "restore",
        golden(pdf).to_str().unwrap(),
        "--out-dir",
        out.to_str().unwrap(),
    ]);

    assert!(
        stderr.contains("manifest: yes"),
        "manifest was not decoded from {pdf}: {stderr}"
    );
    assert!(
        stderr.contains("verified against manifest"),
        "{pdf} was not verified against its manifest: {stderr}"
    );

    let restored = out.join(expected_name);
    assert!(
        restored.exists(),
        "{pdf}: expected '{expected_name}', found {:?}",
        fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        fs::read(&restored).unwrap(),
        fs::read(golden(expected_file)).unwrap(),
        "{pdf}: restored bytes differ from the frozen expectation"
    );

    // The sidecar checksum must name the file it belongs to.
    let sidecar = fs::read_to_string(out.join(format!("{expected_name}.sha256"))).unwrap();
    assert!(
        sidecar.trim_end().ends_with(expected_name),
        "unexpected sidecar contents: {sidecar}"
    );
}

/// coldbackup 0.1.0 wrote manifests without a `fmt=` key. Sheets printed then must
/// keep restoring — including the file name and the sha256 verification.
#[test]
fn sheets_from_v0_1_0_without_a_format_key_still_restore() {
    assert_golden_restores(
        "v0.1.0-no-fmt.pdf",
        "v0.1.0-no-fmt.expected",
        "sample.txt",
    );
}

/// Format 1.0.0, written with a non-default 2x3 / 90 mm grid across three pages.
///
/// Restoring it with default flags only works if the decoder takes the geometry from
/// the manifest, so this also pins the manifest-driven layout path and a UTF-8 name.
#[test]
fn format_1_0_0_sheets_restore_without_layout_hints() {
    assert_golden_restores("v1.0.0.pdf", "v1.0.0-grüße.expected", "grüße.txt");
}

/// Format 1.1.0 added Reed–Solomon parity codes alongside the data.
#[test]
fn format_1_1_0_sheets_with_parity_restore() {
    assert_golden_restores(
        "v1.1.0-parity.pdf",
        "v1.1.0-parity.expected",
        "archive.bin",
    );
}
