# Golden files — do not regenerate

These PDFs stand in for sheets that are already printed and sitting in a drawer.
They are **frozen fixtures**: they exist so a future build can prove it still reads
what an older build wrote.

If `tests/golden.rs` fails, the decoder has lost the ability to read one of these
sheets. Fix the decoder. Re-creating the fixture makes the test pass while destroying
the only evidence that old sheets are still readable.

| File | Written by | What it pins |
| --- | --- | --- |
| `v0.1.0-no-fmt.pdf` | coldbackup 0.1.0, before the `fmt=` fix | Manifests without a `fmt` key, default 5x7 / 35 mm grid, 1 page |
| `v1.0.0.pdf` | coldbackup 0.1.0, format 1.0.0 | `fmt=1.0.0`, non-default 2x3 / 90 mm grid over 3 pages, UTF-8 file name (`grüße.txt`) |
| `v1.1.0-parity.pdf` | coldbackup 0.1.0, format 1.1.0 | `fmt=1.1.0`, `CB1P` parity codes, `clen`/`rsk` manifest keys, interleaved placement |

The `.expected` file next to each PDF holds the exact bytes the restore must produce.

Both are restored with **default** CLI flags on purpose: whatever the decoder needs
beyond the defaults has to come off the sheet itself, because that is all a future
user will have.

## Adding a golden file

Add one when the on-sheet format changes — never replace an existing one. Name it
after the format version that produced it and record the producing command here.

```
coldbackup backup grüße.txt   -o v1.0.0.pdf        --qr-mm 90 --chunk-bytes 300
coldbackup backup archive.bin -o v1.1.0-parity.pdf --qr-mm 90 --chunk-bytes 300
```
