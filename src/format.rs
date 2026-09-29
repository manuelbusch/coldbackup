//! The on-sheet wire format.
//!
//! This is what is actually printed on paper, so it is the part of the crate that has
//! to stay readable for as long as the sheets exist. Builders and parsers live side by
//! side here so the two can never drift apart; `tests/golden.rs` pins the result
//! against real PDFs produced by earlier versions.
//!
//! Payloads are `TAG|key=value|key=value…`. Unknown keys are ignored, which is what
//! lets a minor version add fields without breaking older decoders.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use crc32fast::Hasher as Crc32;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};

use crate::layout::SheetLayout;

pub const PRODUCER: &str = "coldbackup";
pub const PRODUCER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Format version (SemVer): the same major must remain compatible.
pub const SUPPORTED_FORMAT_MAJOR: u32 = 1;
pub const FORMAT_VERSION: &str = "1.1.0";

pub const TAG_MANIFEST: &str = "CB1M";
pub const TAG_DATA: &str = "CB1D";
/// Reed-Solomon parity. A decoder that predates parity ignores this tag entirely,
/// which is what keeps 1.1 sheets readable by a 1.0 decoder.
pub const TAG_PARITY: &str = "CB1P";

// -------------------- SemVer --------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemVer {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl SemVer {
    pub fn parse(s: &str) -> Result<Self> {
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

// -------------------- Building payloads --------------------

/// Everything the manifest QR records about a backup.
#[derive(Debug, Clone, Copy)]
pub struct ManifestInfo<'a> {
    pub session_id: &'a str,
    pub sha256_hex: &'a str,
    pub len: usize,
    pub chunks: usize,
    pub file_name: &'a str,
    pub chunk_bytes: usize,
    pub quiet_modules: i32,
    pub layout: SheetLayout,
    /// Length of the compressed stream. Needed to trim the zero padding off a final
    /// chunk that was rebuilt from parity rather than read directly.
    pub compressed_len: usize,
    /// Data shards per erasure block, or `None` when the backup carries no parity.
    pub rs_block_shards: Option<usize>,
}

pub fn manifest_payload(info: &ManifestInfo<'_>) -> String {
    let l = info.layout;
    // `rsk` is only present when the backup carries parity, so a 1.0-era decoder sees
    // exactly the manifest it already understands.
    let rs = match info.rs_block_shards {
        Some(k) => format!("|rsk={k}"),
        None => String::new(),
    };
    format!(
        "{tag}|fmt={fmt}|sid={sid}|prod={prod}|pver={pver}|sha256={sha}|len={len}|clen={clen}|chunks={chunks}|name={name}|cmpr=zstd|chunk={chunk}|ecc=H|qz={qz}|page_w_mm={pw}|page_h_mm={ph}|qr_mm={qr}|margin_mm={m}|gap_mm={g}|cols={c}|rows={r}{rs}",
        rs = rs,
        clen = info.compressed_len,
        tag = TAG_MANIFEST,
        fmt = FORMAT_VERSION,
        sid = info.session_id,
        prod = PRODUCER,
        pver = PRODUCER_VERSION,
        sha = info.sha256_hex,
        len = info.len,
        chunks = info.chunks,
        name = URL_SAFE_NO_PAD.encode(info.file_name.as_bytes()),
        chunk = info.chunk_bytes,
        qz = info.quiet_modules,
        pw = trim_f32(l.page_w_mm),
        ph = trim_f32(l.page_h_mm),
        qr = trim_f32(l.qr_mm),
        m = trim_f32(l.margin_mm),
        g = trim_f32(l.gap_mm),
        c = l.cols,
        r = l.rows,
    )
}

/// Build a data payload from an already base64-encoded body.
///
/// The encoder and the adaptive sizing probe both go through here, so a size estimate
/// can never drift away from what is actually drawn.
pub fn data_payload(session_id: &str, idx: usize, total: usize, crc32: u32, b64: &str) -> String {
    format!(
        "{tag}|sid={sid}|idx={idx}|total={total}|crc32={crc:08x}|data={data}",
        tag = TAG_DATA,
        sid = session_id,
        idx = idx,
        total = total,
        crc = crc32,
        data = b64
    )
}

pub fn data_payload_for_chunk(session_id: &str, idx: usize, total: usize, chunk: &[u8]) -> String {
    let b64 = URL_SAFE_NO_PAD.encode(chunk);
    data_payload(session_id, idx, total, crc32_of(chunk), &b64)
}

/// Build a parity payload from an already base64-encoded body.
#[allow(clippy::too_many_arguments)]
pub fn parity_payload(
    session_id: &str,
    block: usize,
    pidx: usize,
    data_shards: usize,
    parity_shards: usize,
    shard_len: usize,
    crc32: u32,
    b64: &str,
) -> String {
    format!(
        "{tag}|sid={sid}|blk={blk}|pidx={pidx}|k={k}|m={m}|slen={slen}|crc32={crc:08x}|data={data}",
        tag = TAG_PARITY,
        sid = session_id,
        blk = block,
        pidx = pidx,
        k = data_shards,
        m = parity_shards,
        slen = shard_len,
        crc = crc32,
        data = b64
    )
}

pub fn parity_payload_for_shard(
    session_id: &str,
    block: usize,
    pidx: usize,
    data_shards: usize,
    parity_shards: usize,
    shard: &[u8],
) -> String {
    let b64 = URL_SAFE_NO_PAD.encode(shard);
    parity_payload(
        session_id,
        block,
        pidx,
        data_shards,
        parity_shards,
        shard.len(),
        crc32_of(shard),
        &b64,
    )
}

// -------------------- Parsed payloads --------------------

#[derive(Debug, Clone)]
pub struct Manifest {
    pub sid: String,
    pub prod: Option<String>,
    pub pver: Option<String>,
    pub sha256: Option<String>,
    pub len: Option<usize>,
    pub chunks: Option<usize>,
    pub filename: Option<String>,
    pub layout: Option<SheetLayout>,
    /// Length of the compressed stream, used to trim padding off a rebuilt final chunk.
    pub compressed_len: Option<usize>,
    /// Data shards per erasure block; `None` means the backup has no parity.
    pub rs_block_shards: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct DataChunk {
    pub sid: String,
    pub idx: usize,
    pub total: usize,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ParityShard {
    pub sid: String,
    pub block: usize,
    pub pidx: usize,
    /// Data and parity shard counts of the block, repeated on every parity QR so a
    /// block can still be rebuilt when the manifest itself is unreadable.
    pub data_shards: usize,
    pub parity_shards: usize,
    pub shard_len: usize,
    pub bytes: Vec<u8>,
}

fn parse_kv_payload(s: &str) -> Option<(&str, HashMap<String, String>)> {
    let mut it = s.split('|');
    let tag = it.next()?;
    if tag != TAG_MANIFEST && tag != TAG_DATA && tag != TAG_PARITY {
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

pub fn parse_manifest_payload(s: &str) -> Result<Manifest> {
    let (tag, kv) = parse_kv_payload(s).ok_or_else(|| anyhow!("not a kv payload"))?;
    if tag != TAG_MANIFEST {
        bail!("not a manifest payload");
    }

    let sid = kv
        .get("sid")
        .ok_or_else(|| anyhow!("manifest missing sid"))?
        .to_string();

    // coldbackup <= 0.1.0 omitted `fmt` entirely. The CB1M tag already pins the
    // generation, so treat a missing key as 1.x rather than rejecting sheets that have
    // already been printed.
    let fmt_str = kv.get("fmt").map(String::as_str).unwrap_or(FORMAT_VERSION);
    let fmt = SemVer::parse(fmt_str)?;
    if fmt.major != SUPPORTED_FORMAT_MAJOR {
        bail!(
            "unsupported format major {} (this decoder supports {}.x.y)",
            fmt.major,
            SUPPORTED_FORMAT_MAJOR
        );
    }

    let filename = kv
        .get("name")
        .and_then(|b64| URL_SAFE_NO_PAD.decode(b64).ok())
        .and_then(|b| String::from_utf8(b).ok());

    let layout = {
        let f = |k: &str| kv.get(k).and_then(|x| x.parse::<f32>().ok());
        let i = |k: &str| kv.get(k).and_then(|x| x.parse::<i32>().ok());
        match (
            f("page_w_mm"),
            f("page_h_mm"),
            f("qr_mm"),
            f("margin_mm"),
            f("gap_mm"),
            i("cols"),
            i("rows"),
        ) {
            (Some(pw), Some(ph), Some(qr), Some(m), Some(g), Some(c), Some(r)) => {
                SheetLayout::from_parts(pw, ph, qr, m, g, c, r)
            }
            _ => None,
        }
    };

    Ok(Manifest {
        sid,
        prod: kv.get("prod").cloned(),
        pver: kv.get("pver").cloned(),
        sha256: kv.get("sha256").cloned(),
        len: kv.get("len").and_then(|x| x.parse::<usize>().ok()),
        chunks: kv.get("chunks").and_then(|x| x.parse::<usize>().ok()),
        compressed_len: kv.get("clen").and_then(|x| x.parse::<usize>().ok()),
        rs_block_shards: kv
            .get("rsk")
            .and_then(|x| x.parse::<usize>().ok())
            .filter(|k| *k > 0),
        filename,
        layout,
    })
}

/// Parse a data payload and verify its CRC32.
///
/// The CRC is the reason a single misread QR does not have to cost the whole restore:
/// callers drop the payloads that fail here and keep everything else.
pub fn parse_data_payload(s: &str) -> Result<DataChunk> {
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

    let got = crc32_of(&bytes);
    if got != crc_expected {
        bail!(
            "crc32 mismatch for chunk {}: expected {:08x}, got {:08x}",
            idx,
            crc_expected,
            got
        );
    }

    Ok(DataChunk {
        sid,
        idx,
        total,
        bytes,
    })
}

/// Parse a parity payload and verify its CRC32.
pub fn parse_parity_payload(s: &str) -> Result<ParityShard> {
    let (tag, kv) = parse_kv_payload(s).ok_or_else(|| anyhow!("not a kv payload"))?;
    if tag != TAG_PARITY {
        bail!("not a parity payload");
    }

    let num = |k: &str| -> Result<usize> {
        kv.get(k)
            .ok_or_else(|| anyhow!("parity missing {k}"))?
            .parse::<usize>()
            .with_context(|| format!("{k} parse"))
    };

    let sid = kv
        .get("sid")
        .ok_or_else(|| anyhow!("parity missing sid"))?
        .to_string();
    let block = num("blk")?;
    let pidx = num("pidx")?;
    let data_shards = num("k")?;
    let parity_shards = num("m")?;
    let shard_len = num("slen")?;

    let crc_expected = kv
        .get("crc32")
        .ok_or_else(|| anyhow!("parity missing crc32"))?;
    let crc_expected = u32::from_str_radix(crc_expected, 16).context("crc32 parse")?;

    let data_b64 = kv.get("data").ok_or_else(|| anyhow!("parity missing data"))?;
    let bytes = URL_SAFE_NO_PAD.decode(data_b64).context("base64 decode")?;

    let got = crc32_of(&bytes);
    if got != crc_expected {
        bail!(
            "crc32 mismatch for parity {}/{}: expected {:08x}, got {:08x}",
            block,
            pidx,
            crc_expected,
            got
        );
    }
    if bytes.len() != shard_len {
        bail!(
            "parity {}/{}: declared shard length {} but carries {}",
            block,
            pidx,
            shard_len,
            bytes.len()
        );
    }
    if data_shards == 0 || parity_shards == 0 || pidx >= parity_shards {
        bail!("parity {}/{}: inconsistent block description", block, pidx);
    }

    Ok(ParityShard {
        sid,
        block,
        pidx,
        data_shards,
        parity_shards,
        shard_len,
        bytes,
    })
}

// -------------------- Sessions --------------------

pub fn parse_session_id(s: &str) -> Option<String> {
    let mut it = s.split('|');
    let tag = it.next()?;
    if tag != TAG_MANIFEST && tag != TAG_DATA && tag != TAG_PARITY {
        return None;
    }
    for token in it {
        if let Some(v) = token.strip_prefix("sid=") {
            return Some(v.to_string());
        }
    }
    None
}

pub fn find_sessions(decoded: &[String]) -> BTreeSet<String> {
    decoded.iter().filter_map(|s| parse_session_id(s)).collect()
}

/// Pick the session to restore. Several backups may share one PDF, and guessing
/// between them would silently restore the wrong file.
pub fn choose_session(sessions_found: &BTreeSet<String>, wanted: Option<&str>) -> Result<String> {
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
    match sessions_found.len() {
        0 => bail!("no sessions found"),
        1 => Ok(sessions_found.iter().next().unwrap().clone()),
        _ => bail!(
            "multiple sessions found: {:?}. Use --session <hex>.",
            sessions_found
        ),
    }
}

// -------------------- Small helpers --------------------

pub fn crc32_of(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finalize()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

pub fn random_session_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

pub fn trim_f32(v: f32) -> String {
    let s = format!("{:.3}", v);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> SheetLayout {
        SheetLayout::fit(210.0, 297.0, 35.0, 8.0, 2.0).unwrap()
    }

    fn info<'a>(sid: &'a str, name: &'a str, sha: &'a str) -> ManifestInfo<'a> {
        ManifestInfo {
            session_id: sid,
            sha256_hex: sha,
            len: 3000,
            chunks: 4,
            file_name: name,
            chunk_bytes: 269,
            quiet_modules: 4,
            layout: layout(),
            compressed_len: 1000,
            rs_block_shards: Some(32),
        }
    }

    #[test]
    fn manifest_roundtrips_through_its_own_parser() {
        let sha = sha256_hex(b"hello");
        let payload = manifest_payload(&info("abc123", "notes.txt", &sha));
        let m = parse_manifest_payload(&payload).unwrap();

        assert_eq!(m.sid, "abc123");
        assert_eq!(m.filename.as_deref(), Some("notes.txt"));
        assert_eq!(m.sha256.as_deref(), Some(sha.as_str()));
        assert_eq!(m.len, Some(3000));
        assert_eq!(m.chunks, Some(4));
        assert_eq!(m.prod.as_deref(), Some(PRODUCER));
        assert_eq!(m.layout, Some(layout()));
    }

    #[test]
    fn manifest_carries_the_format_version() {
        // Regression guard: the manifest was once written without `fmt=` while the
        // parser required it, which silently disabled every manifest-backed feature.
        let payload = manifest_payload(&info("abc123", "notes.txt", "00"));
        assert!(payload.contains(&format!("fmt={FORMAT_VERSION}")));
    }

    #[test]
    fn manifest_without_fmt_is_still_readable() {
        // Sheets printed by coldbackup <= 0.1.0 have no `fmt` key.
        let payload = manifest_payload(&info("abc123", "notes.txt", "00"));
        let legacy = payload.replace(&format!("|fmt={FORMAT_VERSION}"), "");
        assert!(!legacy.contains("fmt="));
        assert_eq!(parse_manifest_payload(&legacy).unwrap().sid, "abc123");
    }

    #[test]
    fn manifest_from_a_future_major_is_refused() {
        let payload = manifest_payload(&info("abc123", "notes.txt", "00"))
            .replace(&format!("fmt={FORMAT_VERSION}"), "fmt=2.0.0");
        assert!(parse_manifest_payload(&payload).is_err());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        // Minor versions must be able to add fields without breaking old decoders.
        let payload = format!("{}|future_key=42", manifest_payload(&info("abc", "n", "00")));
        assert_eq!(parse_manifest_payload(&payload).unwrap().sid, "abc");
    }

    #[test]
    fn data_chunk_roundtrips() {
        let chunk = b"some binary payload".as_slice();
        let payload = data_payload_for_chunk("deadbeef", 7, 42, chunk);
        let parsed = parse_data_payload(&payload).unwrap();

        assert_eq!(parsed.sid, "deadbeef");
        assert_eq!(parsed.idx, 7);
        assert_eq!(parsed.total, 42);
        assert_eq!(parsed.bytes, chunk);
    }

    #[test]
    fn corrupted_data_is_rejected_by_crc() {
        let payload = data_payload_for_chunk("deadbeef", 0, 1, b"payload");
        let corrupted = payload.replace("crc32=", "crc32=") + "x";
        assert!(parse_data_payload(&corrupted).is_err());

        let wrong_crc = data_payload("deadbeef", 0, 1, 0, "cGF5bG9hZA");
        assert!(parse_data_payload(&wrong_crc).is_err());
    }

    #[test]
    fn session_id_is_found_in_both_payload_kinds() {
        let m = manifest_payload(&info("cafe", "n", "00"));
        let d = data_payload_for_chunk("cafe", 0, 1, b"x");
        assert_eq!(parse_session_id(&m).as_deref(), Some("cafe"));
        assert_eq!(parse_session_id(&d).as_deref(), Some("cafe"));
        assert_eq!(parse_session_id("not a payload"), None);
    }

    #[test]
    fn ambiguous_sessions_require_an_explicit_choice() {
        let sessions: BTreeSet<String> = ["aaa".to_string(), "bbb".to_string()].into();
        assert!(choose_session(&sessions, None).is_err());
        assert_eq!(choose_session(&sessions, Some("bbb")).unwrap(), "bbb");
        assert!(choose_session(&sessions, Some("ccc")).is_err());

        let single: BTreeSet<String> = ["aaa".to_string()].into();
        assert_eq!(choose_session(&single, None).unwrap(), "aaa");
    }

    #[test]
    fn trim_f32_stays_stable_for_layout_values() {
        assert_eq!(trim_f32(35.0), "35");
        assert_eq!(trim_f32(2.5), "2.5");
        assert_eq!(trim_f32(0.3), "0.3");
    }
}
