# coldbackup

**Store a file as printable QR-code sheets – and restore it from the PDF, a scan or a
phone photo.**

`coldbackup` is meant for small, important files that must still be readable years from
now, independent of hard drives, cloud accounts or file systems: private keys, recovery
codes, password databases, seeds, configurations. The file is compressed, split into
chunks, protected with checksums and Reed–Solomon parity, and written as a grid of QR
codes on A4 pages into a vector PDF. Printed out, the paper goes into a drawer – a true
*cold backup*.

```
file ─► zstd ─► chunks (+CRC32) ─► Reed-Solomon parity ─► QR codes (ECC H) ─► PDF (A4)
                                                                               │
                                                                        print / store
                                                                               │
file ◄─ SHA-256 check ◄─ zstd ◄─ parity repairs gaps ◄─ read QR ◄─ PDF / scan / photo
```

## Features

- **Self-describing.** Cell 0 of every page holds a *manifest* with file name, length,
  SHA-256, chunk count and page geometry. A single sheet is enough to identify the
  backup; restoring needs no extra information. The header and footer also spell out
  the matching restore command in plain text.
- **Survives destroyed codes.** QR error correction (level H) protects the inside of a
  code. Against codes that are gone entirely – a coffee stain, a staple hole, a torn
  corner, a lost sheet – Reed–Solomon parity *across* chunks helps (default: 25 %).
  Blocks are interleaved across all pages so that local damage costs each block only a
  few shards.
- **Verifies itself.** By default the PDF is read back after writing and compared byte
  for byte against the source file. A backup that cannot be read back is never reported
  as good.
- **Reads scans and photos.** Slightly rotated, shifted, unevenly lit or JPEG-compressed
  scans are read without relying on fixed geometry: starting from one detected code, its
  neighbours are predicted; if needed, the sheet is swept with overlapping tiles.
  Inverted images (negatives) are tried as well.
- **Sized for print.** The chunk size is chosen automatically so that no QR module gets
  narrower than `--min-module-mm` (default 0.30 mm). If that cannot be met, `backup`
  aborts with an explanation instead of producing a barely readable PDF.
- **Stable, versioned format.** The payload format follows SemVer; older sheets stay
  readable. Frozen golden PDFs from earlier versions guard this in the test suite.
- **Safe to restore.** The file name from the manifest is sanitised and can never write
  outside `--out-dir`.

## Installation

Requirements: Rust (edition 2024, i.e. Rust ≥ 1.85) and at least one sans-serif font
installed on the system (used for the PDF header and footer).

```sh
cargo install --path .
# or
cargo build --release   # binary: target/release/coldbackup
```

## Quick start

```sh
# Back up: writes secret.txt.coldbackup.pdf and verifies it immediately
coldbackup backup secret.txt

# Later: check whether the backup (or a scan of it) is still fully readable
coldbackup verify secret.txt.coldbackup.pdf

# Restore – from the PDF …
coldbackup restore secret.txt.coldbackup.pdf --out-dir restored/

# … from a single scan …
coldbackup restore scan-page1.jpg --out-dir restored/

# … or from a folder of scans/photos (one file per sheet, sorted by name)
coldbackup restore scans/ --out-dir restored/
```

`restore` writes the file under its original name, plus a `<name>.sha256` next to it
(in `sha256sum` format).

Whether the input is a PDF or an image is decided by the file contents (`%PDF` header),
not the extension. Supported image formats: PNG, JPEG, TIFF, BMP, WebP, GIF.

## Commands

### `backup <FILE>`

| Option | Default | Meaning |
| --- | --- | --- |
| `-o, --output <PDF>` | `<FILE>.coldbackup.pdf` | Output PDF |
| `--orientation` | `portrait` | `portrait` or `landscape` (A4) |
| `--qr-mm` | `35` | Edge length of one QR code in mm |
| `--margin-mm` | `8` | Page margin in mm |
| `--gap-mm` | `2` | Gap between codes in mm |
| `--chunk-bytes` | `700` | Upper bound of payload bytes per code; reduced automatically if needed |
| `--min-module-mm` | `0.30` | Smallest allowed module width; `0` disables the check |
| `--zstd-level` | `10` | zstd compression level |
| `--quiet-modules` | `4` | Quiet zone around each code, in modules |
| `--parity-frac` | `0.25` | Parity as a fraction of the data; `0` = no parity |
| `--rs-block-shards` | `32` | Data chunks per Reed–Solomon block (max. 256 including parity) |
| `--no-verify` | – | Do **not** read the PDF back after writing |

The largest grid that fits is derived from page size, `--qr-mm`, `--margin-mm` and
`--gap-mm` – with the defaults, 5 × 7 codes per page, one of which is the manifest.

The output reports session ID, SHA-256, grid, page count, the actual smallest module
size and the redundancy:

- how many destroyed codes per block can be tolerated,
- whether losing **an entire sheet** is still recoverable. That requires parity worth at
  least one sheet of data; for small backups 25 % is often not enough – raise
  `--parity-frac` in that case.

### `restore <INPUT>`

`<INPUT>` is a backup PDF, a scan, or a directory of scans.

| Option | Default | Meaning |
| --- | --- | --- |
| `-o, --out-dir <DIR>` | `.` | Output directory |
| `--output-name <NAME>` | from the manifest | Override the file name |
| `--session <HEX>` | – | Pick a session when several backups are mixed together |
| `--render-dpi` | `600` | Resolution for rendering a PDF (900 dpi is tried additionally) |
| `--crop-pad-frac` | `0.10` | Crop padding per cell as a fraction of the code size |
| `--no-invert` | – | Skip the inverted-image attempt |
| `--qr-mm`, `--margin-mm`, `--gap-mm`, `--cols`, `--rows` | `35`, `8`, `2`, `5`, `7` | Geometry hints, used **only** when no manifest is readable |

Missing chunks are rebuilt from parity where possible. If a manifest is readable,
length and SHA-256 are checked; without a manifest the file is still assembled, but
explicitly reported as *not verified* and saved as `restored-<session>.bin`.

For scans, `restore` reports the effective resolution of each sheet and warns when a QR
module falls below about 4 pixels – along with a recommendation for the resolution to
rescan at.

### `verify <INPUT>`

Reads a backup (PDF, scan or scan folder) in full, rebuilds the file in memory and
checks it against the manifest – **without writing anything**. Options as for `restore`
(`--session`, `--render-dpi`, `--no-invert`, geometry hints).

The result is one of:

- `OK` – every code read, file intact.
- `OK (with damage)` – the file can be restored, but not every code was readable; the
  output states how many chunks were repaired from parity.
- `INCONCLUSIVE` – the file could be assembled, but no manifest was readable to check it
  against.

It also reports the **remaining margin**: how many more codes may be lost before the
weakest block becomes unrecoverable. It is worth scanning stored sheets now and then and
checking them with `verify`.

## Printing recommendations

- Print at **actual size** (100 %), not "fit to page".
- Prefer laser over inkjet, use good paper; store dark, dry and away from light.
- Scan right after printing and run `coldbackup verify scans/` – this checks the whole
  chain of printer, paper and scanner, not just the PDF.
- For long-term storage choose larger codes (e.g. `--qr-mm 50` or more) and, for
  multi-page backups, raise `--parity-frac` until `backup` reports that losing a whole
  sheet is recoverable.
- Scan resolution: a QR module should be at least ~4 pixels wide in the scan. With the
  default layout (35 mm codes, modules around 0.3 mm) that means **600 dpi** – at
  300 dpi many codes are lost. With large codes (e.g. `--qr-mm 90`) 300 dpi is enough.
- Keep several copies in different places. Store the tool itself (or this source code)
  alongside – the format is described below and deliberately simple.

> **Note:** `coldbackup` does not encrypt. Whoever has the sheets has the file.
> Encrypt confidential data first (e.g. with `age` or `gpg`) and back up the ciphertext.

## The on-paper format

Every QR code holds an ASCII line of the form `TAG|key=value|…`. Unknown keys are
ignored, which lets minor versions add fields without breaking older decoders. Binary
data is Base64 (URL-safe, no padding). All codes of one backup share a random session ID
(`sid`, 16 hex characters).

| Tag | Contents |
| --- | --- |
| `CB1M` | **Manifest** (cell 0 of every page): `fmt` (format version), `sid`, `prod`/`pver` (producer), `sha256` and `len` of the original file, `clen` (length of the zstd stream), `chunks`, `name` (Base64), `cmpr=zstd`, `chunk`, `ecc=H`, `qz`, page geometry (`page_w_mm`, `page_h_mm`, `qr_mm`, `margin_mm`, `gap_mm`, `cols`, `rows`) and – only with parity – `rsk` (data shards per block) |
| `CB1D` | **Data chunk**: `sid`, `idx`, `total`, `crc32`, `data` |
| `CB1P` | **Parity shard**: `sid`, `blk`, `pidx`, `k` (data shards), `m` (parity shards), `slen`, `crc32`, `data` |

Restoring by hand: concatenate all `CB1D` chunks ordered by `idx`, decompress with
zstd, compare the SHA-256 with the manifest. Parity is Reed–Solomon over GF(2⁸) per
block (chunks `blk·rsk … blk·rsk+k−1`, the last chunk zero-padded to `slen`; the real
length follows from `clen`).

Current format version: **1.1.0** (1.0 → 1.1: parity, `clen`, `rsk`). A 1.0 decoder
ignores `CB1P` and still reads undamaged 1.1 sheets.

## As a library

All logic lives in the library crate; the binary is just a thin CLI:

```rust
use coldbackup::{backup, restore, verify, BackupOptions, RestoreOptions};
use std::path::Path;

let report = backup(Path::new("secret.txt"), None, &BackupOptions::default())?;
let check = verify(&report.out_path, &RestoreOptions::default())?;
assert!(check.verified && check.intact());
```

`restore_from_payloads` restores from QR texts obtained any other way (e.g. from a
phone app).

| Module | Responsibility |
| --- | --- |
| `format` | Payload grammar – the contract with every sheet already printed |
| `layout` | Page geometry, single source of truth for encoder and decoder |
| `encode` | Backup pipeline, adaptive chunk sizing, vector QR output into the PDF |
| `rs` | Reed–Solomon blocks, interleaving across sheets, repair |
| `imaging` | PDF page → pixels, pixels → QR text (panic-safe around `rqrr`) |
| `scan` | Reading scans and photos without fixed geometry |
| `decode` | Restore pipeline, source detection, assembling the file |
| `verify` | Reading back and checking without writing |

## Tests

```sh
cargo test
```

- `tests/roundtrip.rs` – end to end through the real binary (backup → PDF → restore).
- `tests/parity.rs` – deliberately removed codes and entire sheets; the file must come
  back byte for byte.
- `tests/scan.rs` – simulated scans (rotation, offset, scaling, uneven lighting, blur,
  JPEG).
- `tests/golden.rs` – reads frozen PDFs from earlier versions in `tests/golden/`.
  **Never regenerate these files**; add new ones when the format changes. See
  [`tests/golden/README.md`](tests/golden/README.md).

The tests render real PDFs and decode real QR codes; a release build
(`cargo test --release`) is considerably faster.
