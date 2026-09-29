//! CLI front end. All the work lives in the library so it can be tested directly.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use coldbackup::{
    BackupOptions, FORMAT_VERSION, PRODUCER, PRODUCER_VERSION, RestoreOptions, VerifyReport,
    backup, restore, verify, verify_against,
};

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

        /// Reed-Solomon parity as a fraction of the data (0.25 = 25% extra codes).
        /// 0 disables parity, and with it any tolerance for a destroyed QR.
        #[arg(long, default_value_t = 0.25)]
        parity_frac: f32,

        /// Data shards per erasure block.
        #[arg(long, default_value_t = 32)]
        rs_block_shards: usize,

        /// Skip reading the finished PDF back to prove it restores. Faster, but the
        /// backup is then only assumed to be good.
        #[arg(long)]
        no_verify: bool,
    },

    Restore {
        /// A backup PDF, a scanned sheet image, or a directory of scanned sheets.
        input: PathBuf,

        #[arg(short, long, default_value = ".")]
        out_dir: PathBuf,

        #[arg(long, default_value_t = coldbackup::decode::DEFAULT_RENDER_DPI)]
        render_dpi: u32,

        #[arg(long)]
        session: Option<String>,

        #[arg(long)]
        output_name: Option<String>,

        // Layout hints so restore can crop deterministically even without manifest.
        // Defaults match backup defaults (5x7 grid).
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

        /// Skip the inverted-image pass (it only runs when a sheet is otherwise
        /// unreadable, so turning it off rarely helps).
        #[arg(long)]
        no_invert: bool,
    },

    /// Read a backup back and report whether it still restores, writing nothing.
    Verify {
        /// A backup PDF, a scanned sheet image, or a directory of scanned sheets.
        input: PathBuf,

        #[arg(long)]
        session: Option<String>,

        /// Layout hints, used only when no manifest can be decoded.
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

        #[arg(long, default_value_t = coldbackup::decode::DEFAULT_RENDER_DPI)]
        render_dpi: u32,

        #[arg(long)]
        no_invert: bool,
    },
}

/// Print what a verification found.
fn report_verification(r: &VerifyReport) {
    let codes = r.manifest_codes + r.data_codes + r.parity_codes;
    eprintln!(
        "  session={} sha256={}\n  {} byte(s) in {} chunk(s); {} code(s) read ({} manifest, {} data, {} parity)",
        r.session_id,
        r.sha256_hex,
        r.len,
        r.total_chunks,
        codes,
        r.manifest_codes,
        r.data_codes,
        r.parity_codes,
    );
    if let Some(name) = &r.filename {
        eprintln!("  file name: {name}");
    }
    if r.chunks_repaired > 0 {
        eprintln!(
            "  {} chunk(s) were unreadable and had to be rebuilt from parity",
            r.chunks_repaired
        );
    }
    match r.spare_codes {
        Some(0) => eprintln!("  NO margin left: losing any further code loses the file"),
        Some(n) => eprintln!("  margin: {n} more code(s) may be lost and still recover"),
        None => eprintln!("  no parity: losing any single code loses the file"),
    }
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
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
            parity_frac,
            rs_block_shards,
            no_verify,
        } => {
            let opts = BackupOptions {
                landscape: orientation == "landscape",
                qr_mm,
                margin_mm,
                gap_mm,
                chunk_bytes,
                min_module_mm,
                zstd_level,
                quiet_modules,
                parity_frac,
                rs_block_shards,
            };
            let r = backup(&input, output.as_deref(), &opts)?;

            eprintln!(
                "OK: wrote '{}' (session={}, sha256={})\n  producer={} {} (format {})\n  chunk_bytes={} -> total_chunks={}\n  grid={}x{} ({} per page, {} data/page + manifest, {} pages)\n  qr_mm={} min_module_mm={} smallest_module_mm={}",
                r.out_path.display(),
                r.session_id,
                r.sha256_hex,
                PRODUCER,
                PRODUCER_VERSION,
                FORMAT_VERSION,
                r.chunk_bytes,
                r.total_chunks,
                r.layout.cols,
                r.layout.rows,
                r.layout.per_page(),
                r.layout.data_per_page(),
                r.total_pages,
                qr_mm,
                min_module_mm,
                r.smallest_module_mm
                    .map(|mm| format!("{mm:.3}"))
                    .unwrap_or_else(|| "?".to_string()),
            );

            if r.parity_shards == 0 {
                eprintln!(
                    "  redundancy: NONE — a single destroyed QR code loses the whole file.\n\
                     \x20             Re-run without --parity-frac 0 to add Reed-Solomon parity."
                );
            } else {
                eprintln!(
                    "  redundancy: {} parity codes in {} block(s); any {} destroyed code(s) \
                     per block are recoverable",
                    r.parity_shards, r.blocks, r.tolerated_losses
                );
                if r.survives_sheet_loss {
                    eprintln!("  losing any single sheet is recoverable");
                } else if r.total_pages > 1 {
                    eprintln!(
                        "  losing a whole sheet is NOT recoverable: surviving that needs parity \
                         worth at least one sheet of data (raise --parity-frac)"
                    );
                }
            }

            if no_verify {
                eprintln!(
                    "  NOT VERIFIED: --no-verify was given, so the PDF was never read back"
                );
                return Ok(());
            }

            // Read the finished PDF back and rebuild the file from it. Anything short of
            // a byte-for-byte match against the source means this backup must not be
            // reported as good — that is the whole point of a cold backup.
            eprintln!("verifying '{}' ...", r.out_path.display());
            let verified = verify_against(&r.out_path, &r.sha256_hex, &RestoreOptions::default())
                .with_context(|| {
                    format!(
                        "'{}' was written but could NOT be read back. It is kept for inspection, \
                         but do not rely on it: print larger codes (--qr-mm), or raise \
                         --min-module-mm, and make a fresh backup",
                        r.out_path.display()
                    )
                })?;
            eprintln!("VERIFIED: the PDF restores to the original file.");
            report_verification(&verified);
            Ok(())
        }

        Cmd::Restore {
            input,
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
            no_invert,
        } => {
            let opts = RestoreOptions {
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
                try_invert: !no_invert,
            };
            let r = restore(&input, &opts)?;

            let verified = if r.verified {
                "sha256 + length verified against manifest"
            } else {
                "no manifest decoded — bytes NOT verified"
            };
            let produced_by = r
                .producer
                .map(|p| format!(" [written by {p}]"))
                .unwrap_or_default();
            let repaired = if r.recovered_chunks > 0 {
                format!("\n  {} chunk(s) rebuilt from Reed-Solomon parity", r.recovered_chunks)
            } else {
                String::new()
            };
            eprintln!(
                "OK: restored '{}' ({}){}\n  sha256={}{}",
                r.out_file.display(),
                verified,
                produced_by,
                r.sha256_hex,
                repaired
            );
            Ok(())
        }

        Cmd::Verify {
            input,
            session,
            qr_mm,
            margin_mm,
            gap_mm,
            cols,
            rows,
            render_dpi,
            no_invert,
        } => {
            let opts = RestoreOptions {
                render_dpi,
                session,
                qr_mm,
                margin_mm,
                gap_mm,
                cols,
                rows,
                try_invert: !no_invert,
                ..Default::default()
            };
            let r = verify(&input, &opts)?;

            if r.verified && r.intact() {
                eprintln!("OK: '{}' restores intact.", input.display());
            } else if r.verified {
                eprintln!(
                    "OK (with damage): '{}' still restores, but not every code could be read.",
                    input.display()
                );
            } else {
                eprintln!(
                    "INCONCLUSIVE: '{}' rebuilds a file, but no manifest was readable to \
                     check it against.",
                    input.display()
                );
            }
            report_verification(&r);
            Ok(())
        }
    }
}
