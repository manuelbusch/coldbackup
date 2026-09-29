//! Damage tests for the Reed–Solomon parity.
//!
//! These go through the real pipeline: a real PDF is written, its QR codes are really
//! decoded, and only then are specific codes removed to stand in for a stain, a tear or
//! a lost sheet. What is left has to rebuild the file byte for byte.

mod common;

use coldbackup::format::{parse_data_payload, parse_parity_payload};
use coldbackup::rs::{BlockPlan, RsParams, ShardRef, interleave};
use coldbackup::{BackupOptions, BackupReport, RestoreOptions, backup, restore_from_payloads, scan_pdf};
use common::{TempDir, pseudo_random};
use std::fs;
use std::path::Path;

/// Large QRs and small chunks: few codes per sheet, so the sheets stay cheap to render
/// while still spanning several pages.
const BLOCK_SHARDS: usize = 32;
const PARITY_FRAC: f32 = 0.5;

fn options() -> BackupOptions {
    BackupOptions {
        qr_mm: 90.0,
        chunk_bytes: 300,
        parity_frac: PARITY_FRAC,
        rs_block_shards: BLOCK_SHARDS,
        ..Default::default()
    }
}

/// Write a backup and decode every code back out of the finished PDF.
fn write_and_scan(tmp: &TempDir) -> (Vec<u8>, BackupReport, Vec<String>) {
    let input = tmp.join("archive.bin");
    let pdf = tmp.join("backup.pdf");
    let original = pseudo_random(3000);
    fs::write(&input, &original).unwrap();

    let report = backup(&input, Some(&pdf), &options()).expect("backup");
    let payloads = scan_pdf(&pdf, &RestoreOptions::default()).expect("scan");
    (original, report, payloads)
}

fn shard_ref_of(payload: &str) -> Option<ShardRef> {
    if let Ok(d) = parse_data_payload(payload) {
        return Some(ShardRef::Data(d.idx));
    }
    if let Ok(p) = parse_parity_payload(payload) {
        return Some(ShardRef::Parity(p.block, p.pidx));
    }
    None
}

/// The placement the encoder used, so a test can destroy exactly one sheet's worth.
fn placement(report: &BackupReport) -> Vec<ShardRef> {
    let plan = BlockPlan::new(
        report.total_chunks,
        RsParams {
            block_shards: BLOCK_SHARDS,
            parity_frac: PARITY_FRAC,
        },
    )
    .unwrap();
    interleave(&plan)
}

fn without(payloads: &[String], destroyed: &[ShardRef]) -> Vec<String> {
    payloads
        .iter()
        .filter(|p| match shard_ref_of(p) {
            Some(r) => !destroyed.contains(&r),
            None => true, // keep the manifest
        })
        .cloned()
        .collect()
}

fn restore_into(out: &Path, payloads: &[String]) -> anyhow::Result<Vec<u8>> {
    let opts = RestoreOptions {
        out_dir: out.to_path_buf(),
        ..Default::default()
    };
    let report = restore_from_payloads(payloads, &opts)?;
    Ok(fs::read(report.out_file)?)
}

#[test]
fn parity_codes_are_actually_written_to_the_sheets() {
    let tmp = TempDir::new("parity-present");
    let (_, report, payloads) = write_and_scan(&tmp);

    assert!(report.parity_shards > 0, "backup wrote no parity");
    let parity_found = payloads
        .iter()
        .filter(|p| parse_parity_payload(p).is_ok())
        .count();
    assert_eq!(
        parity_found, report.parity_shards,
        "not every parity code came back out of the PDF"
    );
}

#[test]
fn destroying_the_full_parity_budget_still_restores() {
    let tmp = TempDir::new("parity-budget");
    let (original, report, payloads) = write_and_scan(&tmp);

    // One block here, so the budget is exactly its parity count.
    let budget = report.tolerated_losses;
    assert!(budget > 0);

    // Destroy that many *data* codes: the worst case, since parity has to stand in for
    // real content rather than for other parity.
    let destroyed: Vec<ShardRef> = (0..budget).map(ShardRef::Data).collect();
    let survivors = without(&payloads, &destroyed);

    let restored = restore_into(&tmp.join("out"), &survivors)
        .unwrap_or_else(|e| panic!("restore after losing {budget} codes failed: {e:#}"));
    assert_eq!(restored, original, "recovered bytes differ");
}

#[test]
fn destroying_one_code_beyond_the_budget_fails_loudly() {
    let tmp = TempDir::new("parity-overrun");
    let (_, report, payloads) = write_and_scan(&tmp);

    let destroyed: Vec<ShardRef> = (0..report.tolerated_losses + 1)
        .map(ShardRef::Data)
        .collect();
    let survivors = without(&payloads, &destroyed);

    let err = restore_into(&tmp.join("out"), &survivors)
        .expect_err("restore should fail past the parity budget");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("could not be decoded or repaired"),
        "unexpected error: {msg}"
    );
}

/// The scenario the interleaving exists for: an entire sheet is gone.
#[test]
fn losing_a_whole_sheet_is_recoverable() {
    let tmp = TempDir::new("parity-sheet");
    let (original, report, payloads) = write_and_scan(&tmp);

    assert!(report.total_pages > 1, "need a multi-sheet backup");
    assert!(
        report.survives_sheet_loss,
        "this configuration does not claim to survive a lost sheet"
    );

    let order = placement(&report);
    let per_page = report.layout.data_per_page();

    // Every sheet in turn, because the weakest one is what matters.
    for (page, cells) in order.chunks(per_page).enumerate() {
        let survivors = without(&payloads, cells);
        let out = tmp.join(&format!("out-{page}"));
        let restored = restore_into(&out, &survivors)
            .unwrap_or_else(|e| panic!("losing sheet {page} broke the restore: {e:#}"));
        assert_eq!(restored, original, "sheet {page}: recovered bytes differ");
    }
}

/// Parity went into its own payload tag so that a decoder written before this feature
/// keeps working. Dropping every parity code is exactly what such a decoder does, and
/// an undamaged backup must still restore.
#[test]
fn a_decoder_that_ignores_parity_still_restores_undamaged_sheets() {
    let tmp = TempDir::new("parity-forward-compat");
    let (original, report, payloads) = write_and_scan(&tmp);

    let as_seen_by_an_old_decoder: Vec<String> = payloads
        .iter()
        .filter(|p| parse_parity_payload(p).is_err())
        .cloned()
        .collect();
    assert_eq!(
        as_seen_by_an_old_decoder.len(),
        payloads.len() - report.parity_shards,
        "filtering should remove exactly the parity codes"
    );

    let restored = restore_into(&tmp.join("out"), &as_seen_by_an_old_decoder)
        .expect("a parity-unaware decoder must still restore intact sheets");
    assert_eq!(restored, original);
}

/// Without parity a single destroyed code is fatal — the state the tool was in before,
/// and what `--parity-frac 0` still opts into.
#[test]
fn without_parity_a_single_lost_code_is_fatal() {
    let tmp = TempDir::new("parity-off");
    let input = tmp.join("archive.bin");
    let pdf = tmp.join("backup.pdf");
    let original = pseudo_random(3000);
    fs::write(&input, &original).unwrap();

    let opts = BackupOptions {
        parity_frac: 0.0,
        ..options()
    };
    let report = backup(&input, Some(&pdf), &opts).expect("backup");
    assert_eq!(report.parity_shards, 0);
    assert!(!report.survives_sheet_loss);

    let payloads = scan_pdf(&pdf, &RestoreOptions::default()).expect("scan");
    assert_eq!(restore_into(&tmp.join("full"), &payloads).unwrap(), original);

    let survivors = without(&payloads, &[ShardRef::Data(0)]);
    assert!(
        restore_into(&tmp.join("out"), &survivors).is_err(),
        "without parity, losing one code must fail"
    );
}
