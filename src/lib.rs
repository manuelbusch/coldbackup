//! coldbackup — store a file as printable QR-code sheets and read it back.
//!
//! The crate is split so the parts that must stay correct forever can be tested
//! without going through a PDF:
//!
//! - [`format`]  — the payload grammar printed on the sheets. This is the contract
//!   with every sheet already sitting in a drawer, so it changes only with care.
//! - [`layout`]  — page geometry; the single source of truth for where a cell sits,
//!   shared by encoder and decoder.
//! - [`imaging`] — PDF page to pixels, pixels to QR text.
//! - [`encode`]  — the backup pipeline.
//! - [`rs`]      — Reed–Solomon parity across chunks, so a destroyed QR is survivable.
//! - [`scan`]    — reading sheets back from scans and photographs.
//! - [`decode`]  — the restore pipeline.
//! - [`verify`]  — reading a backup back to prove it is still readable.

pub mod decode;
pub mod encode;
pub mod format;
pub mod imaging;
pub mod layout;
pub mod rs;
pub mod scan;
pub mod verify;

pub use decode::{
    RestoreOptions, RestoreReport, Source, detect_source, restore, restore_from_payloads,
    render_sheets, scan_pdf, scan_sheets,
};
pub use encode::{BackupOptions, BackupReport, backup};
pub use format::{FORMAT_VERSION, PRODUCER, PRODUCER_VERSION};
pub use layout::SheetLayout;
pub use verify::{VerifyReport, verify, verify_against};
