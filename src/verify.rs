//! Proving that a backup can still be read.
//!
//! A backup tool that reports success without ever reading its own output is asking the
//! user to find out years later. Verification rebuilds the file from the codes and
//! checks it against the manifest, writing nothing — so it can be run on a fresh PDF as
//! a self-check, and on a scan of sheets that have been in a drawer since.

use anyhow::{Result, bail};
use std::path::Path;

use crate::decode::{
    RestoreOptions, Source, assemble, detect_source, gather_payloads, rebuild,
};
use crate::format::{TAG_DATA, TAG_MANIFEST, TAG_PARITY};

#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub session_id: String,
    /// SHA-256 of the file as rebuilt from the codes.
    pub sha256_hex: String,
    /// Length of the rebuilt file.
    pub len: usize,
    /// The rebuilt bytes matched the sha256 and length recorded in the manifest.
    pub verified: bool,
    pub filename: Option<String>,
    pub producer: Option<String>,

    pub total_chunks: usize,
    /// Chunks read directly off the codes.
    pub chunks_read: usize,
    /// Chunks that had to be rebuilt from parity.
    pub chunks_repaired: usize,
    /// Codes read, by kind.
    pub manifest_codes: usize,
    pub data_codes: usize,
    pub parity_codes: usize,
    /// Further codes that could be lost before some block becomes unrecoverable.
    /// `None` when the backup carries no parity.
    pub spare_codes: Option<usize>,
}

impl VerifyReport {
    /// True when the file was rebuilt without needing a single repair.
    pub fn intact(&self) -> bool {
        self.chunks_repaired == 0 && self.chunks_read == self.total_chunks
    }
}

/// Read a backup back and check it, writing nothing.
///
/// Accepts whatever [`crate::restore`] accepts: the generated PDF, a scanned sheet, or a
/// directory of scans.
pub fn verify(input: &Path, opts: &RestoreOptions) -> Result<VerifyReport> {
    let payloads = gather_payloads(input, opts)?;
    let (session, extracted) = assemble(&payloads, opts)?;

    let chunks_read = extracted.data_chunks.len() - extracted.recovered;
    let rebuilt = rebuild(&extracted)?;

    let mine = |tag: &str| {
        payloads
            .iter()
            .filter(|p| p.starts_with(tag) && p.contains(session.as_str()))
            .count()
    };
    let (manifest_codes, data_codes, parity_codes) =
        (mine(TAG_MANIFEST), mine(TAG_DATA), mine(TAG_PARITY));

    Ok(VerifyReport {
        session_id: session,
        len: rebuilt.bytes.len(),
        sha256_hex: rebuilt.sha256_hex,
        verified: rebuilt.verified,
        filename: rebuilt.filename,
        producer: rebuilt.producer,
        total_chunks: extracted.total_chunks,
        chunks_read,
        chunks_repaired: extracted.recovered,
        manifest_codes,
        data_codes,
        parity_codes,
        spare_codes: extracted.spare_codes,
    })
}

/// Verify a freshly written backup against the bytes it was made from.
///
/// This is the check that matters right after `backup`: matching the manifest only
/// proves the sheets agree with themselves, whereas matching the source proves the
/// sheets actually carry the file.
pub fn verify_against(input: &Path, expected_sha256: &str, opts: &RestoreOptions) -> Result<VerifyReport> {
    let report = verify(input, opts)?;

    if report.sha256_hex != expected_sha256 {
        bail!(
            "verification failed: '{}' rebuilds to sha256 {} but the source file is {}",
            input.display(),
            report.sha256_hex,
            expected_sha256
        );
    }
    if !report.verified {
        bail!(
            "verification failed: '{}' carries no readable manifest to check against",
            input.display()
        );
    }
    if !report.intact() {
        bail!(
            "verification failed: {} of {} chunks could not be read directly from a freshly \
             written file and needed parity to rebuild — the sheets are not trustworthy",
            report.chunks_repaired,
            report.total_chunks
        );
    }

    Ok(report)
}

/// Payload sources a verify can read, for callers that want to describe the input.
pub fn describe_source(input: &Path) -> Result<String> {
    Ok(match detect_source(input)? {
        Source::Pdf(p) => format!("PDF '{}'", p.display()),
        Source::Sheets(paths) if paths.len() == 1 => {
            format!("sheet image '{}'", paths[0].display())
        }
        Source::Sheets(paths) => format!("{} sheet image(s)", paths.len()),
    })
}
