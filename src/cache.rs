//! A write-back block cache over any [`BlockDevice`].
//!
//! # Why this exists
//!
//! Issue #4: unpacking a 55 MB file through `fio-ext4` onto a volume over
//! NVMe/TCP cost ~14.3 million device operations — ~1065x write amplification,
//! with a read paired to nearly every write. The device was not the limiter
//! (0.08 ms per operation); the operation *count* was. Every few KiB of
//! appended payload re-read and re-wrote the same block bitmap, inode-table
//! block, extent node, group descriptor and superblock, with the device acting
//! as the only metadata cache. Transport-side batching could not help, because
//! interleaved far-offset metadata operations prevent contiguous runs from
//! forming at all.
//!
//! [`CachedDevice`] is the fix at this seam. It wraps any [`BlockDevice`] with
//! a bounded write-back cache at block granularity:
//!
//! - reads are served from cache; misses fetch contiguous runs in one
//!   underlying read each,
//! - writes are absorbed into cache as dirty blocks and cost no device I/O
//!   until eviction or [`flush`](BlockDevice::flush),
//! - eviction happens in batches of the least-recently-used blocks, and dirty
//!   victims that are contiguous on the device are written back coalesced into
//!   single writes,
//! - [`flush`](BlockDevice::flush) writes back every dirty block the same way,
//!   then flushes the underlying device.
//!
//! The hot metadata blocks a writer hammers stay resident and settle to one
//! read on first touch and one write per sync point, while streamed file data
//! passes through the LRU and reaches the device as large sequential writes.
//!
//! # Durability
//!
//! This is a *write-back* cache: between sync points the device does not have
//! the dirty blocks, and dropping the wrapper without flushing loses them.
//! That is the intended contract — the consumer this was measured against
//! discards and rebuilds any volume whose build died mid-stream and calls
//! `flush()` before sealing. Do not put this in front of a device whose
//! consumer expects every completed `write_at` to be on stable storage.
//!
//! # Concurrency
//!
//! The cache is a single structure behind one async lock, so operations through
//! the wrapper are serialised. That is the right trade for the workload it
//! exists for — one writer streaming into one volume. The parallel formatter
//! does not need this wrapper: formatting writes each block once, so there is
//! nothing for a cache to absorb.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;

use crate::device::{check_bounds, BlockDevice};
use crate::error::Result;

/// Default cache-block granularity: 4 KiB, the common filesystem block size.
const DEFAULT_BLOCK_SIZE: u32 = 4096;

/// Default capacity: 32 MiB of cached blocks.
const DEFAULT_CAPACITY_BYTES: u64 = 32 << 20;

/// One cached block.
struct Entry {
    /// Block contents. Full `block_size` bytes, except the device's final
    /// block, which is as long as the device allows.
    data: Vec<u8>,
    /// True if the cache holds bytes the device does not.
    dirty: bool,
    /// Recency stamp: the cache-wide tick at last touch. Larger is younger.
    tick: u64,
}

struct Cache {
    map: HashMap<u64, Entry>,
    tick: u64,
}

impl Cache {
    fn touch(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }
}

/// Counters describing what the cache has done — enough to measure the
/// amplification it removes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// `read_at` calls issued to the underlying device.
    pub inner_reads: u64,
    /// `write_at` calls issued to the underlying device.
    pub inner_writes: u64,
    /// Blocks served from cache.
    pub hits: u64,
    /// Blocks fetched from the underlying device.
    pub misses: u64,
    /// Blocks evicted to make room.
    pub evictions: u64,
}

#[derive(Default)]
struct AtomicStats {
    inner_reads: AtomicU64,
    inner_writes: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

/// A write-back block cache wrapped around a [`BlockDevice`].
///
/// See the [module documentation](self) for what it is for and the durability
/// contract. Construct with [`CachedDevice::new`], tune with
/// [`with_block_size`](CachedDevice::with_block_size) and
/// [`with_capacity`](CachedDevice::with_capacity), and call
/// [`flush`](BlockDevice::flush) at every point the bytes must be on the
/// device.
pub struct CachedDevice<D> {
    inner: D,
    block_size: u64,
    max_blocks: usize,
    state: Mutex<Cache>,
    stats: AtomicStats,
}

impl<D: BlockDevice> CachedDevice<D> {
    /// Wrap `inner` with the default geometry: 4 KiB cache blocks, 32 MiB
    /// capacity.
    pub fn new(inner: D) -> Self {
        Self::with_geometry(inner, DEFAULT_BLOCK_SIZE, DEFAULT_CAPACITY_BYTES)
    }

    /// Set the cache-block granularity. Must be a power of two and at least
    /// 512; match it to the filesystem's block size.
    pub fn with_block_size(self, block_size: u32) -> Self {
        Self::with_geometry(self.inner, block_size, self.max_blocks as u64 * self.block_size)
    }

    /// Set the cache capacity in bytes. At least 8 blocks are always kept.
    pub fn with_capacity(self, capacity_bytes: u64) -> Self {
        Self::with_geometry(self.inner, self.block_size as u32, capacity_bytes)
    }

    fn with_geometry(inner: D, block_size: u32, capacity_bytes: u64) -> Self {
        assert!(
            block_size.is_power_of_two() && block_size >= 512,
            "cache block size must be a power of two and at least 512, got {block_size}"
        );
        let max_blocks = ((capacity_bytes / u64::from(block_size)) as usize).max(8);
        Self {
            inner,
            block_size: u64::from(block_size),
            max_blocks,
            state: Mutex::new(Cache {
                map: HashMap::new(),
                tick: 0,
            }),
            stats: AtomicStats::default(),
        }
    }

    /// The counters so far.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            inner_reads: self.stats.inner_reads.load(Ordering::Relaxed),
            inner_writes: self.stats.inner_writes.load(Ordering::Relaxed),
            hits: self.stats.hits.load(Ordering::Relaxed),
            misses: self.stats.misses.load(Ordering::Relaxed),
            evictions: self.stats.evictions.load(Ordering::Relaxed),
        }
    }

    /// Flush every dirty block and return the underlying device.
    pub async fn into_inner(self) -> Result<D> {
        BlockDevice::flush(&self).await?;
        Ok(self.inner)
    }

    /// Byte length of block `blk` — `block_size`, except a short final block
    /// on a device whose size is not a multiple of it.
    fn block_len(&self, blk: u64) -> u64 {
        let start = blk * self.block_size;
        self.block_size.min(self.inner.size() - start)
    }

    async fn read_run_from_inner(&self, first_blk: u64, blocks: u64) -> Result<Vec<u8>> {
        let start = first_blk * self.block_size;
        let len = (0..blocks).map(|i| self.block_len(first_blk + i)).sum::<u64>();
        let mut data = vec![0u8; len as usize];
        self.inner.read_at(start, &mut data).await?;
        self.stats.inner_reads.fetch_add(1, Ordering::Relaxed);
        self.stats.misses.fetch_add(blocks, Ordering::Relaxed);
        Ok(data)
    }

    /// Write back the given blocks, coalescing device-contiguous dirty entries
    /// into single writes. Blocks must be sorted. Clean blocks are skipped.
    /// `remove` controls whether written-back (and clean) blocks leave the
    /// cache or stay resident marked clean.
    async fn write_back(&self, cache: &mut Cache, blocks: &[u64], remove: bool) -> Result<()> {
        let mut run_start = 0u64;
        let mut run = Vec::new();
        for &blk in blocks {
            let dirty = cache.map.get(&blk).map(|e| e.dirty).unwrap_or(false);
            if dirty {
                if run.is_empty() {
                    run_start = blk * self.block_size;
                }
                if remove {
                    let entry = cache.map.remove(&blk).expect("checked above");
                    run.extend_from_slice(&entry.data);
                } else {
                    let entry = cache.map.get_mut(&blk).expect("checked above");
                    entry.dirty = false;
                    run.extend_from_slice(&entry.data);
                }
                // A short block is the device's last; nothing can follow it, so
                // the run stays byte-contiguous while block numbers are.
                let next_joins = blocks.contains(&(blk + 1))
                    && cache.map.get(&(blk + 1)).map(|e| e.dirty).unwrap_or(false);
                if !next_joins {
                    self.inner.write_at(run_start, &run).await?;
                    self.stats.inner_writes.fetch_add(1, Ordering::Relaxed);
                    run.clear();
                }
            } else if remove {
                cache.map.remove(&blk);
            }
        }
        debug_assert!(run.is_empty());
        Ok(())
    }

    /// Make room for one more entry, evicting a batch of the least recently
    /// used blocks if the cache is full.
    async fn make_room(&self, cache: &mut Cache) -> Result<()> {
        if cache.map.len() < self.max_blocks {
            return Ok(());
        }
        // Evict an eighth at a time: amortises the recency scan and gives
        // dirty neighbours from one write stream the chance to leave together
        // as a single coalesced device write.
        let batch = (self.max_blocks / 8).max(1);
        let mut by_age: Vec<(u64, u64)> =
            cache.map.iter().map(|(&blk, e)| (e.tick, blk)).collect();
        by_age.sort_unstable();
        let mut victims: Vec<u64> = by_age[..batch].iter().map(|&(_, blk)| blk).collect();
        victims.sort_unstable();
        self.write_back(cache, &victims, true).await?;
        self.stats
            .evictions
            .fetch_add(victims.len() as u64, Ordering::Relaxed);
        Ok(())
    }
}

#[async_trait::async_trait]
impl<D: BlockDevice> BlockDevice for CachedDevice<D> {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn logical_sector_size(&self) -> u32 {
        self.inner.logical_sector_size()
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_bounds(offset, buf.len() as u64, self.inner.size())?;
        if buf.is_empty() {
            return Ok(());
        }
        let mut cache = self.state.lock().await;
        let end = offset + buf.len() as u64;
        let last = (end - 1) / self.block_size;
        let mut blk = offset / self.block_size;
        while blk <= last {
            if cache.map.contains_key(&blk) {
                let tick = cache.touch();
                let entry = cache.map.get_mut(&blk).expect("checked above");
                entry.tick = tick;
                let bstart = blk * self.block_size;
                let s = offset.max(bstart);
                let e = end.min(bstart + entry.data.len() as u64);
                buf[(s - offset) as usize..(e - offset) as usize]
                    .copy_from_slice(&entry.data[(s - bstart) as usize..(e - bstart) as usize]);
                self.stats.hits.fetch_add(1, Ordering::Relaxed);
                blk += 1;
                continue;
            }
            // A miss: extend it over every consecutive missing block so the
            // underlying device sees one read for the whole run.
            let run_first = blk;
            while blk <= last && !cache.map.contains_key(&blk) {
                blk += 1;
            }
            let run_blocks = blk - run_first;
            let data = self.read_run_from_inner(run_first, run_blocks).await?;
            let run_start = run_first * self.block_size;
            // Serve the caller from the run buffer first — insertion below may
            // evict, and nothing the caller needs may depend on what survives.
            let s = offset.max(run_start);
            let e = end.min(run_start + data.len() as u64);
            buf[(s - offset) as usize..(e - offset) as usize]
                .copy_from_slice(&data[(s - run_start) as usize..(e - run_start) as usize]);
            let mut consumed = 0usize;
            for b in run_first..run_first + run_blocks {
                let blen = self.block_len(b) as usize;
                self.make_room(&mut cache).await?;
                let tick = cache.touch();
                cache.map.insert(
                    b,
                    Entry {
                        data: data[consumed..consumed + blen].to_vec(),
                        dirty: false,
                        tick,
                    },
                );
                consumed += blen;
            }
        }
        Ok(())
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        check_bounds(offset, buf.len() as u64, self.inner.size())?;
        if buf.is_empty() {
            return Ok(());
        }
        let mut cache = self.state.lock().await;
        let end = offset + buf.len() as u64;
        let last = (end - 1) / self.block_size;
        for blk in offset / self.block_size..=last {
            let bstart = blk * self.block_size;
            let blen = self.block_len(blk);
            let s = offset.max(bstart);
            let e = end.min(bstart + blen);
            let covers_whole_block = s == bstart && e == bstart + blen;
            if !cache.map.contains_key(&blk) {
                self.make_room(&mut cache).await?;
                let data = if covers_whole_block {
                    // Fully overwritten: the device's bytes do not matter.
                    vec![0u8; blen as usize]
                } else {
                    // Partially overwritten: the rest of the block must come
                    // from the device — the one read-modify-write left, paid
                    // once per block instead of once per append.
                    self.read_run_from_inner(blk, 1).await?
                };
                let tick = cache.touch();
                cache.map.insert(
                    blk,
                    Entry {
                        data,
                        dirty: false,
                        tick,
                    },
                );
            } else {
                self.stats.hits.fetch_add(1, Ordering::Relaxed);
            }
            let tick = cache.touch();
            let entry = cache.map.get_mut(&blk).expect("inserted above");
            entry.data[(s - bstart) as usize..(e - bstart) as usize]
                .copy_from_slice(&buf[(s - offset) as usize..(e - offset) as usize]);
            entry.dirty = true;
            entry.tick = tick;
        }
        Ok(())
    }

    async fn flush(&self) -> Result<()> {
        let mut cache = self.state.lock().await;
        let mut dirty: Vec<u64> = cache
            .map
            .iter()
            .filter(|(_, e)| e.dirty)
            .map(|(&blk, _)| blk)
            .collect();
        dirty.sort_unstable();
        self.write_back(&mut cache, &dirty, false).await?;
        self.inner.flush().await
    }

    async fn write_zeroes(&self, offset: u64, len: u64) -> Result<()> {
        check_bounds(offset, len, self.inner.size())?;
        if len == 0 {
            return Ok(());
        }
        let mut cache = self.state.lock().await;
        let end = offset + len;
        let last = (end - 1) / self.block_size;
        for blk in offset / self.block_size..=last {
            let bstart = blk * self.block_size;
            let Some(entry) = cache.map.get_mut(&blk) else {
                continue;
            };
            let s = offset.max(bstart);
            let e = end.min(bstart + entry.data.len() as u64);
            if s == bstart && e == bstart + entry.data.len() as u64 {
                // Fully zeroed: the device's zeroes below are the truth now,
                // whatever the cache held.
                cache.map.remove(&blk);
            } else {
                // Partially zeroed: zero the cached copy so a later write-back
                // of this block does not resurrect the old bytes.
                entry.data[(s - bstart) as usize..(e - bstart) as usize].fill(0);
            }
        }
        // Pass the primitive through — on a thin device this is deallocation,
        // which buffering as dirty data blocks would forfeit.
        self.inner.write_zeroes(offset, len).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;
    use std::sync::Arc;

    /// The churn from issue #4 in miniature: each append writes a bit of file
    /// data, then re-reads and re-writes the same handful of metadata blocks.
    /// Uncached, that is six device operations per append; cached, the
    /// metadata settles into the cache and the data coalesces.
    #[tokio::test]
    async fn append_churn_collapses() {
        let inner = Arc::new(MemDevice::new(8 << 20));
        let dev = CachedDevice::new(inner.clone());

        let meta_blocks = [0u64, 4096, 8192, 12288]; // "superblock", "bitmap", ...
        let data_start = 1 << 20;
        let chunk = vec![0xabu8; 1024];
        let appends = 1000u64;
        for i in 0..appends {
            dev.write_at(data_start + i * 1024, &chunk).await.unwrap();
            for &m in &meta_blocks {
                let mut b = [0u8; 64];
                dev.read_at(m, &mut b).await.unwrap();
                dev.write_at(m, &[i as u8; 64]).await.unwrap();
            }
        }
        dev.flush().await.unwrap();

        let s = dev.stats();
        // Uncached this workload is 5 * appends writes and 4 * appends reads.
        // Cached, the metadata blocks cost at most one read each, and the
        // whole megabyte of appends plus metadata flushes back in a handful
        // of coalesced writes.
        assert!(
            s.inner_reads <= meta_blocks.len() as u64,
            "expected at most one device read per metadata block, got {s:?}"
        );
        assert!(
            s.inner_writes < 20,
            "expected coalesced write-back, got {s:?}"
        );
    }

    /// Interleaved writes, reads and zeroes against a mirror, with a cache
    /// small enough to force constant eviction. The image must match the
    /// mirror byte for byte after flush.
    #[tokio::test]
    async fn matches_mirror_under_eviction() {
        let size = 256 * 1024 + 1000; // deliberately not block-aligned
        let inner = Arc::new(MemDevice::new(size));
        let dev = CachedDevice::new(inner.clone()).with_capacity(16 * 4096);
        let mut mirror = vec![0u8; size as usize];

        // Deterministic pseudo-random offsets and lengths.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for i in 0..2000u64 {
            let r = next();
            let offset = r % (size - 1);
            let len = (next() % 9000).min(size - offset).max(1);
            match r % 5 {
                0 => {
                    let mut buf = vec![0u8; len as usize];
                    dev.read_at(offset, &mut buf).await.unwrap();
                    assert_eq!(
                        buf,
                        &mirror[offset as usize..(offset + len) as usize],
                        "read mismatch at {offset}+{len} on iteration {i}"
                    );
                }
                4 => {
                    dev.write_zeroes(offset, len).await.unwrap();
                    mirror[offset as usize..(offset + len) as usize].fill(0);
                }
                _ => {
                    let byte = (next() % 255 + 1) as u8;
                    let buf = vec![byte; len as usize];
                    dev.write_at(offset, &buf).await.unwrap();
                    mirror[offset as usize..(offset + len) as usize].fill(byte);
                }
            }
        }
        dev.flush().await.unwrap();
        assert_eq!(inner.to_vec(), mirror, "image differs from mirror after flush");
    }

    /// Without a flush the device must not be trusted — and with one, it must
    /// hold everything, including blocks that were evicted and re-read.
    #[tokio::test]
    async fn eviction_write_back_preserves_data() {
        let inner = Arc::new(MemDevice::new(1 << 20));
        // 8-block capacity (the floor): writing 64 blocks evicts most of them.
        let dev = CachedDevice::new(inner.clone()).with_capacity(1);
        for blk in 0u64..64 {
            let buf = vec![blk as u8 + 1; 4096];
            dev.write_at(blk * 4096, &buf).await.unwrap();
        }
        assert!(dev.stats().evictions > 0, "expected evictions at capacity 8");
        // Read everything back through the cache: evicted blocks come from the
        // device, resident ones from cache, and both must agree.
        for blk in 0u64..64 {
            let mut buf = vec![0u8; 4096];
            dev.read_at(blk * 4096, &mut buf).await.unwrap();
            assert!(buf.iter().all(|&b| b == blk as u8 + 1), "block {blk} corrupt");
        }
        dev.flush().await.unwrap();
        let image = inner.to_vec();
        for blk in 0usize..64 {
            assert!(
                image[blk * 4096..(blk + 1) * 4096]
                    .iter()
                    .all(|&b| b == blk as u8 + 1),
                "block {blk} not on device after flush"
            );
        }
    }

    /// The device's final block can be shorter than the cache block; reads,
    /// writes and write-back must all respect the device's edge.
    #[tokio::test]
    async fn short_final_block_round_trips() {
        let size = 3 * 4096 + 1000;
        let inner = Arc::new(MemDevice::new(size));
        let dev = CachedDevice::new(inner.clone());
        let pattern: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        dev.write_at(0, &pattern).await.unwrap();
        let mut back = vec![0u8; size as usize];
        dev.read_at(0, &mut back).await.unwrap();
        assert_eq!(back, pattern);
        dev.flush().await.unwrap();
        assert_eq!(inner.to_vec(), pattern);
    }

    /// Zeroes over dirty cached data must win, in every overlap shape.
    #[tokio::test]
    async fn write_zeroes_beats_dirty_cache() {
        let inner = Arc::new(MemDevice::new(64 * 1024));
        let dev = CachedDevice::new(inner.clone());
        dev.write_at(0, &vec![0xffu8; 64 * 1024]).await.unwrap();
        // Partial first block, whole middle blocks, partial last block.
        dev.write_zeroes(1000, 3 * 4096).await.unwrap();
        dev.flush().await.unwrap();
        let image = inner.to_vec();
        assert!(image[..1000].iter().all(|&b| b == 0xff));
        assert!(image[1000..1000 + 3 * 4096].iter().all(|&b| b == 0));
        assert!(image[1000 + 3 * 4096..].iter().all(|&b| b == 0xff));
    }

    /// into_inner flushes: the device is complete without an explicit flush.
    #[tokio::test]
    async fn into_inner_flushes() {
        let inner = Arc::new(MemDevice::new(16 * 1024));
        let dev = CachedDevice::new(inner.clone());
        dev.write_at(5000, b"survives").await.unwrap();
        let returned = dev.into_inner().await.unwrap();
        let mut buf = [0u8; 8];
        returned.read_at(5000, &mut buf).await.unwrap();
        assert_eq!(&buf, b"survives");
    }
}
