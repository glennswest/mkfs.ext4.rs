//! Peak memory of a format, by filesystem size (#10).
//!
//! Formats a sparse in-memory device that stores only the non-zero 4 KiB pages
//! written to it, with lazy inode-table initialisation on a zeroed medium, as
//! the measurement in #10 did. Prints the peak RSS (`VmHWM`) and the bytes the
//! device ended up holding, so the formatter's own share is the difference.
//!
//! One size per process, so each peak is its own:
//!
//! ```text
//! cargo run --release --example formatscale -- 256   # TiB
//! ```
//!
//! `--check` after the size runs a forced check (`e2fsck -fn`) of the result
//! and exits non-zero unless it is clean (#9), then prints the peak RSS again:
//! the check's block map is held as runs of used blocks (#11), so its share is
//! the growth over the format's peak.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use mkfs_ext4::device::BlockDevice;
use mkfs_ext4::{format, fsck, FsckOptions, Params, Profile};

const PAGE: u64 = 4096;

struct SparseDevice {
    size: u64,
    pages: Mutex<HashMap<u64, Box<[u8]>>>,
}

impl SparseDevice {
    fn stored_bytes(&self) -> u64 {
        self.pages.lock().unwrap().len() as u64 * PAGE
    }
}

#[async_trait::async_trait]
impl BlockDevice for SparseDevice {
    fn size(&self) -> u64 {
        self.size
    }

    fn logical_sector_size(&self) -> u32 {
        4096
    }

    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> mkfs_ext4::Result<()> {
        let pages = self.pages.lock().unwrap();
        for (i, byte) in buf.iter_mut().enumerate() {
            let at = offset + i as u64;
            *byte = pages
                .get(&(at / PAGE))
                .map(|p| p[(at % PAGE) as usize])
                .unwrap_or(0);
        }
        Ok(())
    }

    async fn write_at(&self, offset: u64, buf: &[u8]) -> mkfs_ext4::Result<()> {
        let mut pages = self.pages.lock().unwrap();
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset + done as u64;
            let (page, within) = (at / PAGE, (at % PAGE) as usize);
            let n = (PAGE as usize - within).min(buf.len() - done);
            let src = &buf[done..done + n];
            let entry = pages
                .entry(page)
                .or_insert_with(|| vec![0u8; PAGE as usize].into_boxed_slice());
            entry[within..within + n].copy_from_slice(src);
            if entry.iter().all(|&b| b == 0) {
                pages.remove(&page);
            }
            done += n;
        }
        Ok(())
    }

    async fn write_zeroes(&self, offset: u64, len: u64) -> mkfs_ext4::Result<()> {
        // Whole pages are forgotten; a partial page at either end is written.
        let end = offset + len;
        let first_whole = offset.div_ceil(PAGE);
        let last_whole = end / PAGE;
        if first_whole >= last_whole {
            return self.write_at(offset, &vec![0u8; len as usize]).await;
        }
        {
            let mut pages = self.pages.lock().unwrap();
            if last_whole - first_whole < pages.len() as u64 {
                for page in first_whole..last_whole {
                    pages.remove(&page);
                }
            } else {
                pages.retain(|&p, _| p < first_whole || p >= last_whole);
            }
        }
        if offset < first_whole * PAGE {
            self.write_at(offset, &vec![0u8; (first_whole * PAGE - offset) as usize])
                .await?;
        }
        if last_whole * PAGE < end {
            self.write_at(last_whole * PAGE, &vec![0u8; (end - last_whole * PAGE) as usize])
                .await?;
        }
        Ok(())
    }

    async fn flush(&self) -> mkfs_ext4::Result<()> {
        Ok(())
    }
}

fn vm_hwm_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let check = std::env::args().nth(2).as_deref() == Some("--check");
    let tib: u64 = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: formatscale TIB"))?
        .parse()?;
    let dev = SparseDevice {
        size: tib << 40,
        pages: Mutex::new(HashMap::new()),
    };
    let params = Params::new(Profile::Ext4)
        .lazy_itable_init(true)
        .zeroed_medium(true);

    let started = Instant::now();
    let report = format(&dev, &params).await?;
    let secs = started.elapsed().as_secs_f64();

    let stored = dev.stored_bytes();
    let hwm = vm_hwm_kib() * 1024;
    println!(
        "{tib} TiB: {} groups, {} inodes ({} per group), {:.1} s, stored {:.1} MiB, peak RSS {:.1} MiB, RSS - stored {:.1} MiB",
        report.group_count,
        report.inodes_count,
        report.inodes_count / report.group_count,
        secs,
        stored as f64 / (1 << 20) as f64,
        hwm as f64 / (1 << 20) as f64,
        hwm.saturating_sub(stored) as f64 / (1 << 20) as f64,
    );

    if check {
        let options = FsckOptions {
            force: true,
            ..FsckOptions::check_only()
        };
        let started = Instant::now();
        let fsck = fsck::check(&dev, &options).await?;
        let hwm_after = vm_hwm_kib() * 1024;
        println!(
            "{tib} TiB: fsck -fn {:.1} s, peak RSS {:.1} MiB (+{:.1} MiB over the format), {}/{} inodes, {}/{} blocks, {} problems",
            started.elapsed().as_secs_f64(),
            hwm_after as f64 / (1 << 20) as f64,
            hwm_after.saturating_sub(hwm) as f64 / (1 << 20) as f64,
            fsck.inodes_used,
            fsck.inodes_count,
            fsck.blocks_used,
            fsck.blocks_count,
            fsck.problems.len(),
        );
        for p in fsck.problems.iter().take(20) {
            println!("  pass {} {}: {}", p.pass, p.code, p.message);
        }
        anyhow::ensure!(fsck.is_clean(), "{tib} TiB is not clean");
    }
    Ok(())
}
