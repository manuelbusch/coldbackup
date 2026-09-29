//! End-to-end tests: they drive the real binary, so they cover the actual
//! backup -> PDF -> restore path including QR rendering and decoding.

mod common;

use common::{TempDir, pseudo_random, run, run_ok};
use std::fs;

#[test]
fn roundtrip_preserves_bytes_and_original_filename() {
    let tmp = TempDir::new("roundtrip");
    let input = tmp.join("secret-notes.txt");
    let pdf = tmp.join("backup.pdf");
    let out = tmp.join("out");

    let original = pseudo_random(4096);
    fs::write(&input, &original).unwrap();

    run_ok(&[
        "backup",
        input.to_str().unwrap(),
        "-o",
        pdf.to_str().unwrap(),
    ]);
    let stderr = run_ok(&[
        "restore",
        pdf.to_str().unwrap(),
        "--out-dir",
        out.to_str().unwrap(),
    ]);

    // Regression guard: the manifest used to be written without `fmt=` while the
    // parser required it, so every restore silently fell back to `restored-<sid>.bin`
    // and skipped sha256 verification entirely.
    assert!(
        stderr.contains("manifest: yes"),
        "manifest was not decoded: {stderr}"
    );
    assert!(
        stderr.contains("verified against manifest"),
        "restore did not verify against the manifest: {stderr}"
    );

    let restored = out.join("secret-notes.txt");
    assert!(
        restored.exists(),
        "original filename not recovered, got: {:?}",
        fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect::<Vec<_>>()
    );
    assert_eq!(fs::read(&restored).unwrap(), original, "bytes differ");
}

#[test]
fn rejects_layout_that_cannot_survive_printing() {
    let tmp = TempDir::new("modulesize");
    let input = tmp.join("data.bin");
    let pdf = tmp.join("backup.pdf");
    fs::write(&input, pseudo_random(2048)).unwrap();

    // 8 mm QRs put the module size around 0.09 mm, far below the 0.30 mm default.
    // This used to silently produce a PDF that no scanner could ever read back.
    let out = run(&[
        "backup",
        input.to_str().unwrap(),
        "-o",
        pdf.to_str().unwrap(),
        "--qr-mm",
        "8",
    ]);
    assert!(
        !out.status.success(),
        "backup should refuse an unprintable module size"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("min-module-mm"),
        "error should point at --min-module-mm: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!pdf.exists(), "no PDF should be written on refusal");
}

#[test]
fn min_module_mm_zero_overrides_the_refusal() {
    let tmp = TempDir::new("override");
    let input = tmp.join("data.bin");
    let pdf = tmp.join("backup.pdf");
    fs::write(&input, pseudo_random(2048)).unwrap();

    // The sheets this produces are far too fine to read, so the self-check has to be
    // waved off as well for the override to get all the way through.
    run_ok(&[
        "backup",
        input.to_str().unwrap(),
        "-o",
        pdf.to_str().unwrap(),
        "--qr-mm",
        "8",
        "--min-module-mm",
        "0",
        "--no-verify",
    ]);
    assert!(pdf.exists());
}

/// The point of the self-check: a backup that cannot be read back must never be
/// reported as a success, however it came to be written.
#[test]
fn self_verification_rejects_a_pdf_that_cannot_be_read_back() {
    let tmp = TempDir::new("selfverify-bad");
    let input = tmp.join("data.bin");
    let pdf = tmp.join("backup.pdf");
    fs::write(&input, pseudo_random(2048)).unwrap();

    let out = run(&[
        "backup",
        input.to_str().unwrap(),
        "-o",
        pdf.to_str().unwrap(),
        "--qr-mm",
        "8",
        "--min-module-mm",
        "0",
    ]);
    assert!(
        !out.status.success(),
        "an unreadable backup must not be reported as a success"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("could NOT be read back"),
        "the failure should say the PDF is untrustworthy: {stderr}"
    );
}

/// Every ordinary backup is read back before it is called done.
#[test]
fn backup_verifies_itself_by_default() {
    let tmp = TempDir::new("selfverify-good");
    let input = tmp.join("notes.txt");
    let pdf = tmp.join("backup.pdf");
    fs::write(&input, pseudo_random(3000)).unwrap();

    let stderr = run_ok(&[
        "backup",
        input.to_str().unwrap(),
        "-o",
        pdf.to_str().unwrap(),
    ]);
    assert!(
        stderr.contains("VERIFIED: the PDF restores to the original file"),
        "backup should verify itself: {stderr}"
    );

    // And --no-verify must say plainly that it did not.
    let stderr = run_ok(&[
        "backup",
        input.to_str().unwrap(),
        "-o",
        tmp.join("unchecked.pdf").to_str().unwrap(),
        "--no-verify",
    ]);
    assert!(stderr.contains("NOT VERIFIED"), "{stderr}");
}

#[test]
fn multi_page_roundtrip() {
    let tmp = TempDir::new("multipage");
    let input = tmp.join("archive.bin");
    let pdf = tmp.join("backup.pdf");
    let out = tmp.join("out");

    // Comfortably more than one page worth of chunks.
    let original = pseudo_random(20_000);
    fs::write(&input, &original).unwrap();

    run_ok(&[
        "backup",
        input.to_str().unwrap(),
        "-o",
        pdf.to_str().unwrap(),
    ]);
    run_ok(&[
        "restore",
        pdf.to_str().unwrap(),
        "--out-dir",
        out.to_str().unwrap(),
    ]);
    assert_eq!(fs::read(out.join("archive.bin")).unwrap(), original);
}

/// Landscape uses a different grid, and the decoder has to pick that up from the
/// manifest rather than assuming portrait.
#[test]
fn landscape_roundtrip() {
    let tmp = TempDir::new("landscape");
    let input = tmp.join("wide.bin");
    let pdf = tmp.join("backup.pdf");
    let out = tmp.join("out");

    let original = pseudo_random(3000);
    fs::write(&input, &original).unwrap();

    run_ok(&[
        "backup",
        input.to_str().unwrap(),
        "-o",
        pdf.to_str().unwrap(),
        "--orientation",
        "landscape",
    ]);
    run_ok(&[
        "restore",
        pdf.to_str().unwrap(),
        "--out-dir",
        out.to_str().unwrap(),
    ]);
    assert_eq!(fs::read(out.join("wide.bin")).unwrap(), original);
}

/// A restore must not silently write a file it could not check.
#[test]
fn naming_an_unknown_session_fails_loudly() {
    let tmp = TempDir::new("unknown-session");
    let input = tmp.join("a.bin");
    let pdf = tmp.join("a.pdf");
    let out = tmp.join("out");
    fs::write(&input, pseudo_random(600)).unwrap();

    run_ok(&[
        "backup",
        input.to_str().unwrap(),
        "-o",
        pdf.to_str().unwrap(),
    ]);

    let res = run(&[
        "restore",
        pdf.to_str().unwrap(),
        "--out-dir",
        out.to_str().unwrap(),
        "--session",
        "0000000000000000",
    ]);
    assert!(!res.status.success(), "unknown session should fail");
    assert!(
        !out.join("a.bin").exists(),
        "nothing should be written for an unknown session"
    );
}
