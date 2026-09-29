//! Reed–Solomon erasure coding across chunks.
//!
//! QR error correction protects the inside of a single code. It does nothing when a
//! whole code is gone — a coffee ring, a staple hole, a torn corner, a lost sheet.
//! Without parity across chunks, one destroyed QR costs the entire file.
//!
//! Data chunks are grouped into blocks of at most [`MAX_SHARDS_PER_BLOCK`] shards
//! (GF(2^8) cannot address more) and each block gets its own parity shards. A block
//! survives losing any `m` of its shards, data or parity alike.
//!
//! Parity lives in its own payload tag, so a decoder that predates this feature simply
//! ignores it and still restores an undamaged sheet.

use anyhow::{Result, bail};
use reed_solomon_erasure::galois_8::ReedSolomon;
use std::collections::BTreeMap;
use std::ops::Range;

/// GF(2^8) can address at most 256 shards in one block.
pub const MAX_SHARDS_PER_BLOCK: usize = 256;

#[derive(Debug, Clone, Copy)]
pub struct RsParams {
    /// Data shards per block, before parity is added.
    pub block_shards: usize,
    /// Parity as a fraction of the data shards in a block. 0 disables parity.
    pub parity_frac: f32,
}

impl Default for RsParams {
    fn default() -> Self {
        Self {
            block_shards: 32,
            parity_frac: 0.25,
        }
    }
}

/// How the data chunks are grouped into erasure blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockPlan {
    /// Data shards per full block (the last block may be shorter).
    pub block_shards: usize,
    pub total_chunks: usize,
    pub parity_frac: f32,
}

fn parity_for(k: usize, frac: f32) -> usize {
    let m = ((k as f32) * frac).ceil() as usize;
    m.clamp(1, MAX_SHARDS_PER_BLOCK.saturating_sub(k).max(1))
}

impl BlockPlan {
    pub fn new(total_chunks: usize, params: RsParams) -> Result<Self> {
        if total_chunks == 0 {
            bail!("no chunks to protect");
        }
        if !(params.parity_frac > 0.0) {
            bail!("parity_frac must be > 0");
        }

        // Keep data + parity inside the GF(2^8) limit.
        let k_max = (((MAX_SHARDS_PER_BLOCK as f32) / (1.0 + params.parity_frac)).floor() as usize)
            .max(1);
        let block_shards = params.block_shards.clamp(1, total_chunks.min(k_max));

        Ok(Self {
            block_shards,
            total_chunks,
            parity_frac: params.parity_frac,
        })
    }

    /// Rebuild a plan from what the sheets themselves record.
    pub fn from_recorded(total_chunks: usize, block_shards: usize) -> Result<Self> {
        if total_chunks == 0 || block_shards == 0 {
            bail!("invalid recorded block plan");
        }
        Ok(Self {
            block_shards,
            total_chunks,
            parity_frac: 0.0,
        })
    }

    pub fn block_count(&self) -> usize {
        self.total_chunks.div_ceil(self.block_shards)
    }

    pub fn data_range(&self, block: usize) -> Range<usize> {
        let start = block * self.block_shards;
        let end = (start + self.block_shards).min(self.total_chunks);
        start..end
    }

    /// Data shards in a block; the last block may hold fewer.
    pub fn data_shards(&self, block: usize) -> usize {
        self.data_range(block).len()
    }

    pub fn parity_shards(&self, block: usize) -> usize {
        parity_for(self.data_shards(block), self.parity_frac)
    }

    pub fn block_of(&self, chunk_idx: usize) -> usize {
        chunk_idx / self.block_shards
    }

    /// Real length of a chunk: every chunk is `shard_len` except the last.
    pub fn chunk_len(&self, idx: usize, shard_len: usize, compressed_len: usize) -> usize {
        if idx + 1 == self.total_chunks {
            compressed_len - (self.total_chunks - 1) * shard_len
        } else {
            shard_len
        }
    }
}

/// The parity shards computed for one block.
#[derive(Debug, Clone)]
pub struct ParityBlock {
    pub index: usize,
    pub data_shards: usize,
    pub shards: Vec<Vec<u8>>,
}

/// Compute parity for every block. Chunks shorter than `shard_len` (only ever the
/// last one) are zero-padded; the real length is restored from the manifest.
pub fn encode_parity(
    chunks: &[&[u8]],
    shard_len: usize,
    plan: &BlockPlan,
) -> Result<Vec<ParityBlock>> {
    if chunks.len() != plan.total_chunks {
        bail!(
            "plan covers {} chunks but {} were given",
            plan.total_chunks,
            chunks.len()
        );
    }

    let mut out = Vec::with_capacity(plan.block_count());
    for block in 0..plan.block_count() {
        let k = plan.data_shards(block);
        let m = plan.parity_shards(block);
        let rs = ReedSolomon::new(k, m)?;

        let mut shards: Vec<Vec<u8>> = Vec::with_capacity(k + m);
        for idx in plan.data_range(block) {
            let mut shard = vec![0u8; shard_len];
            let chunk = chunks[idx];
            if chunk.len() > shard_len {
                bail!("chunk {idx} is longer than the shard length");
            }
            shard[..chunk.len()].copy_from_slice(chunk);
            shards.push(shard);
        }
        shards.extend((0..m).map(|_| vec![0u8; shard_len]));

        rs.encode(&mut shards)?;

        out.push(ParityBlock {
            index: block,
            data_shards: k,
            shards: shards.split_off(k),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Recovery {
    /// Chunks rebuilt from parity.
    pub recovered: usize,
    /// Blocks that stayed short of their threshold and could not be rebuilt.
    pub unrecoverable_blocks: usize,
}

/// Rebuild missing data chunks from parity, in place.
///
/// Blocks are independent: one block being too damaged does not stop the others from
/// being repaired, which is what keeps a partial restore as useful as possible.
pub fn recover(
    data: &mut BTreeMap<usize, Vec<u8>>,
    parity: &BTreeMap<(usize, usize), Vec<u8>>,
    plan: &BlockPlan,
    shard_len: usize,
    compressed_len: usize,
) -> Recovery {
    let mut result = Recovery::default();

    for block in 0..plan.block_count() {
        let range = plan.data_range(block);
        let k = range.len();

        let missing: Vec<usize> = range.clone().filter(|i| !data.contains_key(i)).collect();
        if missing.is_empty() {
            continue;
        }

        // Parity shard indices present for this block, in ascending order.
        let parity_here: Vec<(usize, &Vec<u8>)> = parity
            .range((block, 0)..(block + 1, 0))
            .map(|((_, pidx), bytes)| (*pidx, bytes))
            .filter(|(_, bytes)| bytes.len() == shard_len)
            .collect();
        let m = parity_here.iter().map(|(pidx, _)| pidx + 1).max().unwrap_or(0);
        if m == 0 {
            result.unrecoverable_blocks += 1;
            continue;
        }

        let have = (k - missing.len()) + parity_here.len();
        if have < k {
            result.unrecoverable_blocks += 1;
            continue;
        }

        let mut shards: Vec<Option<Vec<u8>>> = vec![None; k + m];
        for (slot, idx) in range.clone().enumerate() {
            if let Some(bytes) = data.get(&idx) {
                if bytes.len() > shard_len {
                    continue;
                }
                let mut shard = vec![0u8; shard_len];
                shard[..bytes.len()].copy_from_slice(bytes);
                shards[slot] = Some(shard);
            }
        }
        for (pidx, bytes) in parity_here {
            shards[k + pidx] = Some(bytes.clone());
        }

        let Ok(rs) = ReedSolomon::new(k, m) else {
            result.unrecoverable_blocks += 1;
            continue;
        };
        if rs.reconstruct_data(&mut shards).is_err() {
            result.unrecoverable_blocks += 1;
            continue;
        }

        for idx in missing {
            let slot = idx - range.start;
            let Some(shard) = shards[slot].take() else {
                continue;
            };
            let len = plan.chunk_len(idx, shard_len, compressed_len);
            if len > shard.len() {
                continue;
            }
            data.insert(idx, shard[..len].to_vec());
            result.recovered += 1;
        }
    }

    result
}

// -------------------- Placement --------------------

/// One payload's place in the erasure structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardRef {
    /// A data chunk, by its global index.
    Data(usize),
    /// A parity shard, by block and index within the block.
    Parity(usize, usize),
}

impl ShardRef {
    pub fn block(&self, plan: &BlockPlan) -> usize {
        match *self {
            ShardRef::Data(idx) => plan.block_of(idx),
            ShardRef::Parity(block, _) => block,
        }
    }
}

/// Order payloads so that consecutive sheet cells hold shards of *different* blocks.
///
/// Physical damage is local: a stain, a tear, a lost sheet all take neighbouring cells.
/// Laying the blocks out sequentially would put a whole block on one sheet, where a
/// single lost page defeats any amount of parity. Round-robin spreads each block across
/// as many sheets as there are blocks.
/// Plain round-robin is not enough: the last block is usually shorter, so it runs out
/// early and the remaining blocks then crowd the later sheets. Instead each shard gets
/// a fractional position within its own block, and the shards are ordered by that. Every
/// block is then spread evenly across the whole document no matter how long it is.
pub fn interleave(plan: &BlockPlan) -> Vec<ShardRef> {
    let mut keyed: Vec<(f64, usize, ShardRef)> = Vec::new();

    for b in 0..plan.block_count() {
        let refs: Vec<ShardRef> = plan
            .data_range(b)
            .map(ShardRef::Data)
            .chain((0..plan.parity_shards(b)).map(|p| ShardRef::Parity(b, p)))
            .collect();
        let n = refs.len() as f64;
        for (i, r) in refs.into_iter().enumerate() {
            keyed.push((((i as f64) + 0.5) / n, b, r));
        }
    }

    keyed.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    keyed.into_iter().map(|(_, _, r)| r).collect()
}

/// Worst case over all sheets: if any single sheet is lost, does every block keep
/// enough shards to be rebuilt?
///
/// Surviving the loss of one sheet fundamentally needs parity worth at least one
/// sheet of data, so for small backups this is simply not reachable — better to say so
/// than to imply a guarantee that is not there.
pub fn survives_single_sheet_loss(
    order: &[ShardRef],
    data_per_page: usize,
    plan: &BlockPlan,
) -> bool {
    if data_per_page == 0 {
        return false;
    }
    order.chunks(data_per_page).all(|page| {
        let mut per_block: BTreeMap<usize, usize> = BTreeMap::new();
        for r in page {
            *per_block.entry(r.block(plan)).or_insert(0) += 1;
        }
        per_block
            .into_iter()
            .all(|(block, lost)| lost <= plan.parity_shards(block))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks_of(n: usize, len: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| (0..len).map(|j| ((i * 31 + j * 7) % 251) as u8).collect())
            .collect()
    }

    fn plan_for(n: usize, k: usize, frac: f32) -> BlockPlan {
        BlockPlan::new(
            n,
            RsParams {
                block_shards: k,
                parity_frac: frac,
            },
        )
        .unwrap()
    }

    #[test]
    fn blocks_cover_every_chunk_exactly_once() {
        let plan = plan_for(45, 32, 0.25);
        assert_eq!(plan.block_count(), 2);
        assert_eq!(plan.data_shards(0), 32);
        assert_eq!(plan.data_shards(1), 13);

        let covered: Vec<usize> = (0..plan.block_count())
            .flat_map(|b| plan.data_range(b))
            .collect();
        assert_eq!(covered, (0..45).collect::<Vec<_>>());
    }

    #[test]
    fn block_size_stays_inside_the_field_limit() {
        // A high parity fraction has to shrink the block, not overflow GF(2^8).
        let plan = plan_for(1000, 250, 1.0);
        assert!(plan.block_shards + plan.parity_shards(0) <= MAX_SHARDS_PER_BLOCK);
    }

    #[test]
    fn losing_exactly_the_parity_count_is_still_recoverable() {
        let shard_len = 16;
        let data = chunks_of(10, shard_len);
        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
        let plan = plan_for(10, 10, 0.3); // k=10, m=3
        assert_eq!(plan.parity_shards(0), 3);

        let parity_blocks = encode_parity(&refs, shard_len, &plan).unwrap();
        let mut parity = BTreeMap::new();
        for b in &parity_blocks {
            for (j, s) in b.shards.iter().enumerate() {
                parity.insert((b.index, j), s.clone());
            }
        }

        let mut available: BTreeMap<usize, Vec<u8>> = data
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.clone()))
            .collect();
        // Drop three data chunks: exactly the parity budget.
        for idx in [1, 4, 7] {
            available.remove(&idx);
        }

        let r = recover(&mut available, &parity, &plan, shard_len, 10 * shard_len);
        assert_eq!(r.recovered, 3);
        assert_eq!(r.unrecoverable_blocks, 0);
        for (i, original) in data.iter().enumerate() {
            assert_eq!(&available[&i], original, "chunk {i} differs");
        }
    }

    #[test]
    fn losing_one_more_than_the_budget_fails_cleanly() {
        let shard_len = 16;
        let data = chunks_of(10, shard_len);
        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
        let plan = plan_for(10, 10, 0.3);

        let parity_blocks = encode_parity(&refs, shard_len, &plan).unwrap();
        let mut parity = BTreeMap::new();
        for b in &parity_blocks {
            for (j, s) in b.shards.iter().enumerate() {
                parity.insert((b.index, j), s.clone());
            }
        }

        let mut available: BTreeMap<usize, Vec<u8>> = data
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.clone()))
            .collect();
        for idx in [1, 4, 7, 9] {
            available.remove(&idx);
        }

        let r = recover(&mut available, &parity, &plan, shard_len, 10 * shard_len);
        assert_eq!(r.recovered, 0);
        assert_eq!(r.unrecoverable_blocks, 1);
    }

    #[test]
    fn lost_parity_shards_do_not_block_recovery() {
        let shard_len = 16;
        let data = chunks_of(10, shard_len);
        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
        let plan = plan_for(10, 10, 0.3);

        let parity_blocks = encode_parity(&refs, shard_len, &plan).unwrap();
        let mut parity = BTreeMap::new();
        for b in &parity_blocks {
            for (j, s) in b.shards.iter().enumerate() {
                // Only the middle parity shard survives.
                if j == 1 {
                    parity.insert((b.index, j), s.clone());
                }
            }
        }

        let mut available: BTreeMap<usize, Vec<u8>> = data
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.clone()))
            .collect();
        available.remove(&5);

        let r = recover(&mut available, &parity, &plan, shard_len, 10 * shard_len);
        assert_eq!(r.recovered, 1);
        assert_eq!(available[&5], data[5]);
    }

    #[test]
    fn a_short_final_chunk_is_restored_at_its_real_length() {
        let shard_len = 16;
        let mut data = chunks_of(4, shard_len);
        data[3].truncate(5); // the tail of the compressed stream
        let compressed_len = 3 * shard_len + 5;

        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
        let plan = plan_for(4, 4, 0.5);
        let parity_blocks = encode_parity(&refs, shard_len, &plan).unwrap();
        let mut parity = BTreeMap::new();
        for b in &parity_blocks {
            for (j, s) in b.shards.iter().enumerate() {
                parity.insert((b.index, j), s.clone());
            }
        }

        let mut available: BTreeMap<usize, Vec<u8>> = data
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.clone()))
            .collect();
        available.remove(&3);

        let r = recover(&mut available, &parity, &plan, shard_len, compressed_len);
        assert_eq!(r.recovered, 1);
        assert_eq!(available[&3], data[3], "final chunk must not keep its padding");
    }

    #[test]
    fn one_damaged_block_does_not_stop_the_others() {
        let shard_len = 8;
        let data = chunks_of(20, shard_len);
        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
        let plan = plan_for(20, 10, 0.2); // two blocks, m=2 each

        let parity_blocks = encode_parity(&refs, shard_len, &plan).unwrap();
        let mut parity = BTreeMap::new();
        for b in &parity_blocks {
            for (j, s) in b.shards.iter().enumerate() {
                parity.insert((b.index, j), s.clone());
            }
        }

        let mut available: BTreeMap<usize, Vec<u8>> = data
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.clone()))
            .collect();
        // Block 0 loses more than it can bear, block 1 loses one.
        for idx in [0, 1, 2, 15] {
            available.remove(&idx);
        }

        let r = recover(&mut available, &parity, &plan, shard_len, 20 * shard_len);
        assert_eq!(r.recovered, 1);
        assert_eq!(r.unrecoverable_blocks, 1);
        assert_eq!(available[&15], data[15]);
    }

    #[test]
    fn interleaving_spreads_blocks_across_sheets() {
        let plan = plan_for(138, 32, 0.25);
        let order = interleave(&plan);
        assert_eq!(order.len(), 138 + (0..plan.block_count()).map(|b| plan.parity_shards(b)).sum::<usize>());

        // Every payload appears exactly once.
        let mut data_seen: Vec<usize> = order
            .iter()
            .filter_map(|r| match r {
                ShardRef::Data(i) => Some(*i),
                _ => None,
            })
            .collect();
        data_seen.sort_unstable();
        assert_eq!(data_seen, (0..138).collect::<Vec<_>>());

        // The first sheet must not be dominated by one block.
        let first_page = &order[..34];
        let mut per_block: BTreeMap<usize, usize> = BTreeMap::new();
        for r in first_page {
            *per_block.entry(r.block(&plan)).or_insert(0) += 1;
        }
        assert!(
            per_block.values().all(|c| *c <= 8),
            "one block dominates a sheet: {per_block:?}"
        );
    }

    #[test]
    fn sheet_loss_survival_is_reported_honestly() {
        // 138 chunks over 5 blocks: parity covers a whole lost sheet.
        let plan = plan_for(138, 32, 0.25);
        assert!(survives_single_sheet_loss(&interleave(&plan), 34, &plan));

        // 45 chunks over two sheets: losing one takes far more than 25% parity can
        // rebuild, and the report must not pretend otherwise.
        let small = plan_for(45, 32, 0.25);
        assert!(!survives_single_sheet_loss(&interleave(&small), 34, &small));
    }
}
