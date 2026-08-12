//! Multiple mount protection.
//!
//! Shared block storage can hand one device to two hosts, and ext4 has no other
//! way to notice. MMP is the only multi-host primitive the on-disk format has,
//! and it **fences rather than arbitrates**: a host stamps a sequence number
//! into a reserved block, waits, and looks again. If the number moved, someone
//! else holds the filesystem and this host must not mount it.
//!
//! It is not a lock. It cannot stop a determined writer, and it cannot recover
//! a filesystem two hosts have already written to. What it does is turn a
//! silent, destructive double mount into a refusal that names the other host —
//! `mmp_nodename` exists so the refusal can say *who*, rather than leaving an
//! operator to guess.
//!
//! Mirrors `lib/ext2fs/mmp.c`.

use std::time::Duration;

use crate::bytes::*;
use crate::csum;
use crate::device::BlockDevice;
use crate::error::{Error, Result};
use crate::fs::Filesystem;

/// `EXT4_MMP_MAGIC`
pub const MMP_MAGIC: u32 = 0x004D_4D50;

/// `EXT4_MMP_SEQ_CLEAN` — nobody holds the filesystem.
pub const SEQ_CLEAN: u32 = 0xFF4D_4D50;

/// `EXT4_MMP_SEQ_FSCK` — a checker holds it.
pub const SEQ_FSCK: u32 = 0xE24D_4D50;

/// `EXT4_MMP_SEQ_MAX` — the largest sequence a holder may use.
pub const SEQ_MAX: u32 = 0xE24D_4D4F;

/// `EXT4_MMP_UPDATE_INTERVAL` — default seconds between heartbeats.
pub const UPDATE_INTERVAL: u16 = 5;

/// `EXT4_MMP_MAX_UPDATE_INTERVAL`
pub const MAX_UPDATE_INTERVAL: u16 = 300;

/// `EXT4_MMP_MIN_CHECK_INTERVAL` — never wait less than this.
pub const MIN_CHECK_INTERVAL: u16 = 5;

/// Field offsets within the MMP block.
#[allow(missing_docs)]
pub mod off {
    pub const MMP_MAGIC: usize = 0x00;
    pub const MMP_SEQ: usize = 0x04;
    pub const MMP_TIME: usize = 0x08;
    pub const MMP_NODENAME: usize = 0x10;
    pub const MMP_BDEVNAME: usize = 0x50;
    pub const MMP_CHECK_INTERVAL: usize = 0x70;
    pub const MMP_PAD1: usize = 0x72;
    pub const MMP_PAD2: usize = 0x74;
    pub const MMP_CHECKSUM: usize = 0x3fc;
}

/// The contents of the MMP block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mmp {
    /// Sequence number. [`SEQ_CLEAN`] means unheld.
    pub seq: u32,
    /// When the holder last updated it, seconds since the epoch.
    pub time: u64,
    /// The holder's hostname.
    pub nodename: String,
    /// The device name as the holder knows it.
    pub bdevname: String,
    /// Seconds the holder promises to heartbeat within.
    pub check_interval: u16,
}

impl Default for Mmp {
    fn default() -> Self {
        Self {
            seq: SEQ_CLEAN,
            time: 0,
            nodename: String::new(),
            bdevname: String::new(),
            check_interval: UPDATE_INTERVAL,
        }
    }
}

impl Mmp {
    /// Whether anybody holds the filesystem.
    pub fn is_held(&self) -> bool {
        self.seq != SEQ_CLEAN
    }

    /// Whether a checker holds it.
    pub fn is_fsck(&self) -> bool {
        self.seq == SEQ_FSCK
    }

    /// How the holder should be described in a refusal.
    pub fn holder(&self) -> String {
        match (self.nodename.is_empty(), self.bdevname.is_empty()) {
            (true, true) => "an unnamed host".to_string(),
            (false, true) => self.nodename.clone(),
            (true, false) => format!("a host using {}", self.bdevname),
            (false, false) => format!("{} (as {})", self.nodename, self.bdevname),
        }
    }

    /// Decode from a block.
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let magic = get_u32(buf, off::MMP_MAGIC);
        if magic != MMP_MAGIC {
            return Err(Error::corrupt(
                "mmp block",
                format!("magic {magic:#010x}, expected {MMP_MAGIC:#010x}"),
            ));
        }
        Ok(Self {
            seq: get_u32(buf, off::MMP_SEQ),
            time: get_u64(buf, off::MMP_TIME),
            nodename: field_to_string(&buf[off::MMP_NODENAME..off::MMP_NODENAME + 64]),
            bdevname: field_to_string(&buf[off::MMP_BDEVNAME..off::MMP_BDEVNAME + 32]),
            check_interval: get_u16(buf, off::MMP_CHECK_INTERVAL),
        })
    }

    /// Encode into a block, stamping the checksum when the filesystem carries
    /// them.
    pub fn encode(&self, block_size: usize, metadata_csum: bool, seed: u32) -> Vec<u8> {
        let mut buf = vec![0u8; block_size];
        put_u32(&mut buf, off::MMP_MAGIC, MMP_MAGIC);
        put_u32(&mut buf, off::MMP_SEQ, self.seq);
        put_u64(&mut buf, off::MMP_TIME, self.time);
        put_bytes(&mut buf, off::MMP_NODENAME, 64, self.nodename.as_bytes());
        put_bytes(&mut buf, off::MMP_BDEVNAME, 32, self.bdevname.as_bytes());
        put_u16(&mut buf, off::MMP_CHECK_INTERVAL, self.check_interval);
        put_u16(&mut buf, off::MMP_PAD1, 0);

        if metadata_csum {
            let crc = csum::crc32c(seed, &buf[..off::MMP_CHECKSUM]);
            put_u32(&mut buf, off::MMP_CHECKSUM, crc);
        }
        buf
    }

    /// Verify the checksum of a block this was decoded from.
    pub fn verify_checksum(buf: &[u8], metadata_csum: bool, seed: u32) -> bool {
        if !metadata_csum {
            return true;
        }
        let expect = csum::crc32c(seed, &buf[..off::MMP_CHECKSUM]);
        expect == get_u32(buf, off::MMP_CHECKSUM)
    }
}

/// A sequence number a holder may use — random, and never one of the reserved
/// values.
///
/// Randomness matters: two hosts starting at the same moment must not choose
/// the same number, or each would see its own value and conclude it had won.
pub fn new_seq() -> u32 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    // Keep clear of the reserved range, and of zero.
    (raw % (SEQ_MAX - 1)) + 1
}

/// Why a filesystem could not be held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MmpRefusal {
    /// Another host holds it, and here is which.
    HeldBy {
        /// The holder, as it named itself.
        holder: String,
        /// The sequence number seen.
        seq: u32,
        /// When the holder last updated the block.
        time: u64,
    },
    /// A checker holds it.
    FsckRunning {
        /// The holder, as it named itself.
        holder: String,
    },
    /// The sequence number is not one this implementation understands, which
    /// means something newer is using the filesystem. Refuse rather than guess.
    UnknownSequence {
        /// The sequence number seen.
        seq: u32,
    },
}

impl std::fmt::Display for MmpRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MmpRefusal::HeldBy { holder, seq, .. } => write!(
                f,
                "the filesystem is in use by {holder} (mmp sequence {seq:#010x})"
            ),
            MmpRefusal::FsckRunning { holder } => {
                write!(f, "a filesystem check is running on {holder}")
            }
            MmpRefusal::UnknownSequence { seq } => write!(
                f,
                "the mmp sequence {seq:#010x} is not one this implementation knows; \
                 something newer is using the filesystem"
            ),
        }
    }
}

/// How to take the filesystem.
#[derive(Debug, Clone)]
pub struct MmpOptions {
    /// This host's name, written into the block so a refusal elsewhere can
    /// name it.
    pub nodename: String,
    /// The device as this host knows it.
    pub bdevname: String,
    /// Claim it as a checker rather than as a mounter. A checker's hold is
    /// visible as [`SEQ_FSCK`] and is not heartbeated.
    pub as_fsck: bool,
    /// Override the wait between stamping and re-reading. `None` uses the
    /// protocol's own timing, which is what any other implementation expects.
    ///
    /// Only a test has any business setting this: shortening the wait is
    /// exactly the thing that makes the fence stop working.
    pub wait_override: Option<Duration>,
}

impl Default for MmpOptions {
    fn default() -> Self {
        Self {
            nodename: hostname(),
            bdevname: String::new(),
            as_fsck: false,
            wait_override: None,
        }
    }
}

impl MmpOptions {
    /// Options naming this host and device.
    pub fn new(bdevname: impl Into<String>) -> Self {
        Self {
            bdevname: bdevname.into(),
            ..Default::default()
        }
    }

    /// Claim as a checker.
    pub fn as_fsck(mut self) -> Self {
        self.as_fsck = true;
        self
    }
}

/// A held filesystem.
///
/// While this exists, the filesystem is marked as held by this host. Call
/// [`MmpLease::heartbeat`] within the check interval, or another host will
/// eventually conclude the holder is gone.
#[derive(Debug, Clone)]
pub struct MmpLease {
    /// The sequence number this host claimed.
    pub seq: u32,
    /// The block the MMP structure lives in.
    pub block: u64,
    /// How often this host promised to heartbeat.
    pub check_interval: u16,
    nodename: String,
    bdevname: String,
}

impl MmpLease {
    /// How long a holder may go between heartbeats before another host is
    /// entitled to conclude it has gone.
    pub fn heartbeat_interval(&self) -> Duration {
        Duration::from_secs(self.check_interval.max(MIN_CHECK_INTERVAL) as u64)
    }

    /// Refresh the hold.
    pub async fn heartbeat<D: BlockDevice>(&self, fs: &Filesystem<D>) -> Result<()> {
        let mmp = Mmp {
            seq: self.seq,
            time: now_secs(),
            nodename: self.nodename.clone(),
            bdevname: self.bdevname.clone(),
            check_interval: self.check_interval,
        };
        write_mmp(fs, self.block, &mmp).await
    }

    /// Give the filesystem back.
    pub async fn release<D: BlockDevice>(&self, fs: &Filesystem<D>) -> Result<()> {
        let mmp = Mmp {
            seq: SEQ_CLEAN,
            time: now_secs(),
            nodename: self.nodename.clone(),
            bdevname: self.bdevname.clone(),
            check_interval: self.check_interval,
        };
        write_mmp(fs, self.block, &mmp).await
    }
}

/// Read the MMP block.
pub async fn read_mmp<D: BlockDevice>(fs: &Filesystem<D>, block: u64) -> Result<Mmp> {
    let buf = fs.read_block(block).await?;
    if !Mmp::verify_checksum(&buf, fs.has_metadata_csum(), fs.csum_seed()) {
        return Err(Error::corrupt(
            "mmp block",
            "checksum does not match its contents",
        ));
    }
    Mmp::decode(&buf)
}

/// Write the MMP block.
pub async fn write_mmp<D: BlockDevice>(fs: &Filesystem<D>, block: u64, mmp: &Mmp) -> Result<()> {
    let buf = mmp.encode(
        fs.block_size() as usize,
        fs.has_metadata_csum(),
        fs.csum_seed(),
    );
    fs.write_block(block, &buf).await?;
    fs.device().flush().await
}

/// Take the filesystem, or refuse and say who has it.
///
/// The protocol, from `ext2fs_mmp_start()`:
///
/// 1. Read the block. If it is held, wait out two check intervals and read
///    again — a live holder will have moved its sequence number, a dead one
///    will not.
/// 2. Stamp our own sequence number, hostname and device name.
/// 3. Wait again, and read again. If our number is still there, nobody raced
///    us and the filesystem is ours.
///
/// The two waits are the whole mechanism. Shortening them does not make it
/// faster, it makes it wrong.
pub async fn acquire<D: BlockDevice>(
    fs: &Filesystem<D>,
    options: &MmpOptions,
) -> Result<std::result::Result<MmpLease, MmpRefusal>> {
    let sb = fs.superblock();
    let block = sb.mmp_block;
    if block < sb.first_data_block as u64 || block >= sb.blocks_count {
        return Err(Error::invalid(format!(
            "s_mmp_block is {block}, outside the filesystem"
        )));
    }

    let mut check_interval = sb.mmp_update_interval.max(MIN_CHECK_INTERVAL);
    let existing = read_mmp(fs, block).await?;

    if existing.is_held() {
        if existing.seq == SEQ_FSCK {
            return Ok(Err(MmpRefusal::FsckRunning {
                holder: existing.holder(),
            }));
        }
        if existing.seq > SEQ_FSCK {
            return Ok(Err(MmpRefusal::UnknownSequence {
                seq: existing.seq,
            }));
        }
        // A holder promising a longer interval than the superblock's is
        // believed; waiting less than it asked for would be the same as not
        // waiting.
        check_interval = check_interval.max(existing.check_interval);

        sleep(wait_for(check_interval, options)).await;

        let after = read_mmp(fs, block).await?;
        if after.seq != existing.seq {
            // It moved: somebody is alive on the other end.
            return Ok(Err(MmpRefusal::HeldBy {
                holder: after.holder(),
                seq: after.seq,
                time: after.time,
            }));
        }
        // It did not move. The holder is gone, and we may take over.
    }

    // Stamp our claim.
    let seq = new_seq();
    let claim = Mmp {
        seq,
        time: now_secs(),
        nodename: options.nodename.clone(),
        bdevname: options.bdevname.clone(),
        check_interval,
    };
    write_mmp(fs, block, &claim).await?;

    // And check nobody stamped theirs at the same moment.
    sleep(wait_for(check_interval, options)).await;
    let confirm = read_mmp(fs, block).await?;
    if confirm.seq != seq {
        return Ok(Err(MmpRefusal::HeldBy {
            holder: confirm.holder(),
            seq: confirm.seq,
            time: confirm.time,
        }));
    }

    if options.as_fsck {
        // A checker's hold is visible as such, and is not heartbeated.
        let fsck_claim = Mmp {
            seq: SEQ_FSCK,
            ..claim.clone()
        };
        write_mmp(fs, block, &fsck_claim).await?;
        return Ok(Ok(MmpLease {
            seq: SEQ_FSCK,
            block,
            check_interval,
            nodename: options.nodename.clone(),
            bdevname: options.bdevname.clone(),
        }));
    }

    Ok(Ok(MmpLease {
        seq,
        block,
        check_interval,
        nodename: options.nodename.clone(),
        bdevname: options.bdevname.clone(),
    }))
}

/// `sleep(min(2 * interval + 1, interval + 60))` from `mmp.c`.
fn wait_for(check_interval: u16, options: &MmpOptions) -> Duration {
    if let Some(override_wait) = options.wait_override {
        return override_wait;
    }
    let interval = check_interval as u64;
    Duration::from_secs((2 * interval + 1).min(interval + 60))
}

async fn sleep(duration: Duration) {
    if duration.is_zero() {
        return;
    }
    tokio::time::sleep(duration).await;
}

/// This host's name, for the benefit of whoever is refused next.
pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "unknown host".to_string())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;
    use crate::format::format;
    use crate::params::{Params, Profile};

    const MIB: u64 = 1024 * 1024;

    async fn with_mmp() -> MemDevice {
        let dev = MemDevice::new(16 * MIB);
        let params = Params::new(Profile::Ext4)
            .uuid(*b"0123456789abcdef")
            .mkfs_time(1_700_000_000)
            .features("mmp");
        format(&dev, &params).await.unwrap();
        dev
    }

    fn fast() -> MmpOptions {
        // The waits are the mechanism, so a test that shortens them is testing
        // the bookkeeping around the fence rather than the fence. The timing
        // itself is asserted separately in `wait_follows_the_protocol`.
        MmpOptions {
            nodename: "test-host".into(),
            bdevname: "/dev/test".into(),
            as_fsck: false,
            wait_override: Some(Duration::ZERO),
        }
    }

    #[test]
    fn round_trips_a_block() {
        let mmp = Mmp {
            seq: 0x1234_5678,
            time: 1_700_000_000,
            nodename: "rose1".into(),
            bdevname: "/dev/nvme0n1".into(),
            check_interval: 7,
        };
        let buf = mmp.encode(4096, true, 0xdead_beef);
        assert!(Mmp::verify_checksum(&buf, true, 0xdead_beef));
        assert_eq!(Mmp::decode(&buf).unwrap(), mmp);
    }

    #[test]
    fn offsets_match_ext2_fs_h() {
        assert_eq!(off::MMP_NODENAME, 0x10);
        assert_eq!(off::MMP_BDEVNAME, 0x50);
        assert_eq!(off::MMP_CHECK_INTERVAL, 0x70);
        // mmp_pad2[226] runs from 0x74 up to the checksum with nothing between.
        assert_eq!(off::MMP_PAD2 + 226 * 4, off::MMP_CHECKSUM);
    }

    #[test]
    fn a_new_sequence_is_never_a_reserved_one() {
        for _ in 0..1000 {
            let seq = new_seq();
            assert_ne!(seq, SEQ_CLEAN);
            assert_ne!(seq, SEQ_FSCK);
            assert_ne!(seq, 0);
            assert!(seq < SEQ_MAX);
        }
    }

    #[test]
    fn two_hosts_do_not_pick_the_same_sequence() {
        let a: std::collections::BTreeSet<u32> = (0..200).map(|_| new_seq()).collect();
        assert!(a.len() > 190, "sequences should be well spread: {}", a.len());
    }

    #[test]
    fn wait_follows_the_protocol() {
        let plain = MmpOptions::default();
        // Two intervals plus one, until that exceeds a minute over the interval.
        assert_eq!(wait_for(5, &plain), Duration::from_secs(11));
        assert_eq!(wait_for(30, &plain), Duration::from_secs(61));
        assert_eq!(wait_for(120, &plain), Duration::from_secs(180));
        assert_eq!(wait_for(300, &plain), Duration::from_secs(360));
    }

    #[tokio::test]
    async fn the_formatter_leaves_it_clean() {
        let dev = with_mmp().await;
        let fs = Filesystem::open(&dev).await.unwrap();

        assert!(fs.superblock().mmp_block >= fs.superblock().first_data_block as u64);
        assert_eq!(fs.superblock().mmp_update_interval, UPDATE_INTERVAL);

        let mmp = read_mmp(&fs, fs.superblock().mmp_block).await.unwrap();
        assert_eq!(mmp.seq, SEQ_CLEAN);
        assert!(!mmp.is_held());
    }

    #[tokio::test]
    async fn a_clean_filesystem_can_be_taken() {
        let dev = with_mmp().await;
        let fs = Filesystem::open(&dev).await.unwrap();

        let lease = acquire(&fs, &fast()).await.unwrap().expect("should be free");
        assert_ne!(lease.seq, SEQ_CLEAN);

        let mmp = read_mmp(&fs, lease.block).await.unwrap();
        assert_eq!(mmp.seq, lease.seq);
        assert_eq!(mmp.nodename, "test-host");
        assert_eq!(mmp.bdevname, "/dev/test");
    }

    /// The case the whole mechanism exists for: a second host must be refused,
    /// and told who has it.
    ///
    /// Run with the protocol's real waits — under a paused clock, so they cost
    /// no wall time but still happen in order. The holder's heartbeat lands
    /// *during* the second host's wait, which is exactly what a live holder
    /// looks like from the outside, and exactly what a zero wait cannot show.
    #[tokio::test(start_paused = true)]
    async fn a_second_host_is_refused_and_told_who_holds_it() {
        use std::sync::Arc;

        let dev = Arc::new(with_mmp().await);
        let fs = Filesystem::open(Arc::clone(&dev)).await.unwrap();

        let first = MmpOptions {
            nodename: "rose1".into(),
            bdevname: "/dev/nvme-tcp0".into(),
            wait_override: None,
            as_fsck: false,
        };
        let lease = acquire(&fs, &first).await.unwrap().expect("first host wins");

        // rose1 keeps heartbeating, as a live holder does.
        let beating = {
            let dev = Arc::clone(&dev);
            let lease = lease.clone();
            tokio::spawn(async move {
                let fs = Filesystem::open(dev).await.unwrap();
                for _ in 0..20 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let bumped = Mmp {
                        seq: new_seq(),
                        time: now_secs(),
                        nodename: "rose1".into(),
                        bdevname: "/dev/nvme-tcp0".into(),
                        check_interval: lease.check_interval,
                    };
                    let _ = write_mmp(&fs, lease.block, &bumped).await;
                }
            })
        };

        let second = MmpOptions {
            nodename: "rose2".into(),
            bdevname: "/dev/nvme-tcp1".into(),
            wait_override: None,
            as_fsck: false,
        };
        let fs2 = Filesystem::open(Arc::clone(&dev)).await.unwrap();
        let refusal = acquire(&fs2, &second)
            .await
            .unwrap()
            .expect_err("rose2 must not be allowed in while rose1 is alive");

        beating.abort();

        match refusal {
            MmpRefusal::HeldBy { ref holder, .. } => {
                assert!(
                    holder.contains("rose1"),
                    "the refusal must name the holder: {holder}"
                );
            }
            other => panic!("expected a refusal naming the holder, got {other:?}"),
        }
        assert!(refusal.to_string().contains("rose1"));
    }

    /// A holder that stopped heartbeating is presumed gone, and its hold can be
    /// taken over — otherwise a crashed host would fence the volume forever.
    #[tokio::test]
    async fn a_dead_holder_can_be_taken_over() {
        let dev = with_mmp().await;
        let fs = Filesystem::open(&dev).await.unwrap();

        let stale = Mmp {
            seq: 0x0001_0000,
            time: 1,
            nodename: "crashed-host".into(),
            bdevname: "/dev/old".into(),
            check_interval: UPDATE_INTERVAL,
        };
        let block = fs.superblock().mmp_block;
        write_mmp(&fs, block, &stale).await.unwrap();

        // The sequence does not move during the wait, so the holder is gone.
        let lease = acquire(&fs, &fast())
            .await
            .unwrap()
            .expect("a dead holder should not fence the volume forever");
        assert_ne!(lease.seq, stale.seq);
    }

    #[tokio::test]
    async fn a_running_check_is_refused_differently() {
        let dev = with_mmp().await;
        let fs = Filesystem::open(&dev).await.unwrap();
        let block = fs.superblock().mmp_block;

        let checking = Mmp {
            seq: SEQ_FSCK,
            time: now_secs(),
            nodename: "admin-box".into(),
            bdevname: "/dev/sdb".into(),
            check_interval: UPDATE_INTERVAL,
        };
        write_mmp(&fs, block, &checking).await.unwrap();

        let refusal = acquire(&fs, &fast()).await.unwrap().unwrap_err();
        assert!(matches!(refusal, MmpRefusal::FsckRunning { .. }));
        assert!(refusal.to_string().contains("admin-box"));
    }

    #[tokio::test]
    async fn an_unknown_sequence_is_refused_rather_than_guessed_at() {
        let dev = with_mmp().await;
        let fs = Filesystem::open(&dev).await.unwrap();
        let block = fs.superblock().mmp_block;

        let future = Mmp {
            seq: SEQ_FSCK + 1,
            time: now_secs(),
            nodename: "something-newer".into(),
            ..Default::default()
        };
        write_mmp(&fs, block, &future).await.unwrap();

        let refusal = acquire(&fs, &fast()).await.unwrap().unwrap_err();
        assert!(matches!(refusal, MmpRefusal::UnknownSequence { .. }));
    }

    #[tokio::test]
    async fn releasing_lets_the_next_host_in() {
        let dev = with_mmp().await;
        let fs = Filesystem::open(&dev).await.unwrap();

        let lease = acquire(&fs, &fast()).await.unwrap().expect("free");
        lease.release(&fs).await.unwrap();

        let mmp = read_mmp(&fs, lease.block).await.unwrap();
        assert_eq!(mmp.seq, SEQ_CLEAN);

        let second = acquire(&fs, &fast()).await.unwrap();
        assert!(second.is_ok(), "a released filesystem should be free");
    }

    #[tokio::test]
    async fn a_filesystem_without_mmp_is_left_alone() {
        let dev = MemDevice::new(16 * MIB);
        let params = Params::new(Profile::Ext4)
            .uuid(*b"0123456789abcdef")
            .mkfs_time(1_700_000_000);
        format(&dev, &params).await.unwrap();

        let fs = Filesystem::open(&dev).await.unwrap();
        assert_eq!(fs.superblock().mmp_block, 0);
        // Acquiring on a filesystem that has no MMP block is a caller error.
        assert!(acquire(&fs, &fast()).await.is_err());
    }
}
