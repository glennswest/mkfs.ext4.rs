//! Journal recovery against logs written here, in every tag layout.
//!
//! The writer follows `fs/jbd2/commit.c` and `revoke.c` for the block
//! formats. That the two agree with the kernel is the differential test's
//! job (`tests/journal_e2fsprogs.rs`, against `debugfs` and `e2fsck`); these
//! cover the cases a real log rarely shows on demand — torn commits, revokes,
//! wrap-around, every checksum failing in turn.

use super::*;
use crate::device::MemDevice;
use crate::features::IncompatFeatures;
use crate::format::format;
use crate::fsck::{check, FsckOptions};
use crate::params::{Params, Profile};

const MIB: u64 = 1024 * 1024;

/// A log being written into a filesystem's journal.
pub(crate) struct LogWriter {
    pub incompat: u32,
    pub map: Vec<u64>,
    pub block_size: usize,
    pub first: u32,
    pub maxlen: u32,
    /// The first transaction in the log.
    pub start_seq: u32,
    /// The block the log starts at.
    pub start: u32,
    /// Next transaction to write.
    pub seq: u32,
    /// Next log block to write.
    pub at: u32,
    pub csum_seed: u32,
    pub uuid: [u8; 16],
    pub commit_time: u64,
    /// The journal superblock as found.
    pub raw: Vec<u8>,
}

impl LogWriter {
    pub async fn new(fs: &Filesystem<&MemDevice>, incompat: u32) -> Self {
        let journal = Journal::open(fs).await.unwrap().unwrap();
        let mut uuid = journal.header.uuid;
        if uuid == [0; 16] {
            uuid = fs.superblock().uuid;
        }
        Self {
            incompat,
            block_size: fs.block_size() as usize,
            first: journal.header.first,
            maxlen: journal.header.maxlen,
            start_seq: journal.header.sequence,
            start: journal.header.first,
            seq: journal.header.sequence,
            at: journal.header.first,
            csum_seed: csum::crc32c(!0, &uuid),
            uuid,
            commit_time: 1_700_000_000,
            raw: journal.raw.clone(),
            map: journal.map.clone(),
        }
    }

    fn has(&self, f: u32) -> bool {
        self.incompat & f != 0
    }

    fn csum(&self) -> bool {
        self.has(jbd2_incompat::CSUM_V2) || self.has(jbd2_incompat::CSUM_V3)
    }

    /// Begin the log at `block`, for wrap-around tests.
    pub fn start_at(&mut self, block: u32) {
        self.start = block;
        self.at = block;
    }

    fn next(&mut self) -> u32 {
        let b = self.at;
        self.at += 1;
        if self.at >= self.maxlen {
            self.at = self.first;
        }
        b
    }

    async fn put(&mut self, fs: &Filesystem<&MemDevice>, buf: &[u8]) -> u32 {
        let b = self.next();
        fs.write_block(self.map[b as usize], buf).await.unwrap();
        b
    }

    fn header(&self, kind: u32) -> Vec<u8> {
        let mut buf = vec![0u8; self.block_size];
        buf[0..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        buf[4..8].copy_from_slice(&kind.to_be_bytes());
        buf[8..12].copy_from_slice(&self.seq.to_be_bytes());
        buf
    }

    fn stamp_tail(&self, buf: &mut [u8]) {
        if self.csum() {
            let at = buf.len() - 4;
            buf[at..].fill(0);
            let c = csum::crc32c(self.csum_seed, buf);
            buf[at..].copy_from_slice(&c.to_be_bytes());
        }
    }

    /// A descriptor block and its data blocks. The data's checksums are
    /// computed over what is logged, so `corrupt` can spoil one afterwards.
    pub async fn data(&mut self, fs: &Filesystem<&MemDevice>, blocks: &[(u64, Vec<u8>)]) -> Vec<u32> {
        let mut desc = self.header(DESCRIPTOR_BLOCK);
        let tag_bytes = JournalHeader {
            blocktype: SUPERBLOCK_V2,
            blocksize: 0,
            maxlen: 0,
            first: 0,
            sequence: 0,
            start: 0,
            errno: 0,
            feature_compat: 0,
            feature_incompat: self.incompat,
            feature_ro_compat: 0,
            uuid: [0; 16],
            checksum_type: 0,
            num_fc_blks: 0,
        }
        .tag_bytes();
        let mut at = HEADER_LEN;
        let mut logged = Vec::new();
        for (i, (home, data)) in blocks.iter().enumerate() {
            let mut data = data.clone();
            let mut flags = 0u16;
            if get_u32_be(&data, 0) == JBD2_MAGIC {
                data[..4].fill(0);
                flags |= tag_flag::ESCAPE;
            }
            if i > 0 {
                flags |= tag_flag::SAME_UUID;
            }
            if i + 1 == blocks.len() {
                flags |= tag_flag::LAST_TAG;
            }
            let tag = &mut desc[at..at + tag_bytes];
            tag[0..4].copy_from_slice(&(*home as u32).to_be_bytes());
            if self.has(jbd2_incompat::CSUM_V3) {
                tag[4..8].copy_from_slice(&(flags as u32).to_be_bytes());
            } else {
                tag[6..8].copy_from_slice(&flags.to_be_bytes());
            }
            if self.has(jbd2_incompat::SIXTY_FOUR_BIT) {
                tag[8..12].copy_from_slice(&((*home >> 32) as u32).to_be_bytes());
            }
            // Summed as logged, escaped: the kernel sums the escaped copy it
            // writes, and recovery verifies the logged copy before undoing
            // the escape.
            if self.csum() {
                let mut crc = csum::crc32c(self.csum_seed, &self.seq.to_be_bytes());
                crc = csum::crc32c(crc, &data);
                if self.has(jbd2_incompat::CSUM_V3) {
                    tag[12..16].copy_from_slice(&crc.to_be_bytes());
                } else {
                    tag[4..6].copy_from_slice(&(crc as u16).to_be_bytes());
                }
            }
            at += tag_bytes;
            if flags & tag_flag::SAME_UUID == 0 {
                desc[at..at + 16].copy_from_slice(&self.uuid);
                at += 16;
            }
            logged.push(data);
        }
        self.stamp_tail(&mut desc);
        self.put(fs, &desc).await;
        let mut where_ = Vec::new();
        for data in logged {
            where_.push(self.put(fs, &data).await);
        }
        where_
    }

    /// A revoke block.
    pub async fn revoke(&mut self, fs: &Filesystem<&MemDevice>, blocks: &[u64]) {
        let mut buf = self.header(REVOKE_BLOCK);
        let record = if self.has(jbd2_incompat::SIXTY_FOUR_BIT) { 8 } else { 4 };
        let mut at = REVOKE_HEADER_LEN;
        for &b in blocks {
            if record == 8 {
                buf[at..at + 8].copy_from_slice(&b.to_be_bytes());
            } else {
                buf[at..at + 4].copy_from_slice(&(b as u32).to_be_bytes());
            }
            at += record;
        }
        buf[12..16].copy_from_slice(&(at as u32).to_be_bytes());
        self.stamp_tail(&mut buf);
        self.put(fs, &buf).await;
    }

    /// A commit block, ending the transaction.
    pub async fn commit(&mut self, fs: &Filesystem<&MemDevice>) -> u32 {
        let mut buf = self.header(COMMIT_BLOCK);
        buf[COMMIT_SEC..COMMIT_SEC + 8].copy_from_slice(&self.commit_time.to_be_bytes());
        if self.csum() {
            buf[12] = CRC32C_CHKSUM;
            buf[13] = 4;
            let c = csum::crc32c(self.csum_seed, &buf);
            buf[COMMIT_CHKSUM..COMMIT_CHKSUM + 4].copy_from_slice(&c.to_be_bytes());
        }
        let b = self.put(fs, &buf).await;
        self.seq = self.seq.wrapping_add(1);
        self.commit_time += 1;
        b
    }

    /// Point the journal superblock at the log, with these features, and set
    /// the filesystem's needs_recovery flag.
    pub async fn finish(&self, fs: &mut Filesystem<&MemDevice>, set_recover: bool) {
        let mut sb = self.raw.clone();
        sb[off::S_FEATURE_INCOMPAT..off::S_FEATURE_INCOMPAT + 4]
            .copy_from_slice(&self.incompat.to_be_bytes());
        sb[off::S_SEQUENCE..off::S_SEQUENCE + 4].copy_from_slice(&self.start_seq.to_be_bytes());
        sb[off::S_START..off::S_START + 4].copy_from_slice(&self.start.to_be_bytes());
        sb[off::S_UUID..off::S_UUID + 16].copy_from_slice(&self.uuid);
        if self.csum() {
            sb[off::S_CHECKSUM_TYPE] = CRC32C_CHKSUM;
            sb[off::S_CHECKSUM..off::S_CHECKSUM + 4].fill(0);
            let c = csum::crc32c(!0, &sb[..JOURNAL_SB_LEN]);
            sb[off::S_CHECKSUM..off::S_CHECKSUM + 4].copy_from_slice(&c.to_be_bytes());
        }
        fs.write_block(self.map[0], &sb).await.unwrap();
        if set_recover {
            fs.superblock_mut().feature_incompat.insert(IncompatFeatures::RECOVER);
            fs.flush_superblock().await.unwrap();
        }
    }
}

async fn formatted(profile: Profile, size: u64) -> MemDevice {
    let dev = MemDevice::new(size);
    let params = Params::new(profile)
        .uuid(*b"0123456789abcdef")
        .mkfs_time(1_700_000_000);
    format(&dev, &params).await.unwrap();
    dev
}

/// A block full of `byte`, starting with `head`.
fn pattern(block_size: usize, head: u32, byte: u8) -> Vec<u8> {
    let mut b = vec![byte; block_size];
    b[..4].copy_from_slice(&head.to_be_bytes());
    b
}

/// Free blocks near the end of the filesystem, where nothing lives.
fn spare(fs: &Filesystem<&MemDevice>, n: u64) -> u64 {
    fs.superblock().blocks_count - 64 + n
}

const LAYOUTS: &[(&str, u32)] = &[
    ("32-bit", jbd2_incompat::REVOKE),
    ("64-bit", jbd2_incompat::REVOKE | jbd2_incompat::SIXTY_FOUR_BIT),
    ("csum v2", jbd2_incompat::REVOKE | jbd2_incompat::CSUM_V2),
    ("csum v2 64-bit", jbd2_incompat::REVOKE | jbd2_incompat::CSUM_V2 | jbd2_incompat::SIXTY_FOUR_BIT),
    ("csum v3", jbd2_incompat::REVOKE | jbd2_incompat::CSUM_V3),
    ("csum v3 64-bit", jbd2_incompat::REVOKE | jbd2_incompat::CSUM_V3 | jbd2_incompat::SIXTY_FOUR_BIT),
];

#[test]
fn tag_sizes_are_journal_tag_bytes() {
    let header = |incompat| JournalHeader {
        blocktype: SUPERBLOCK_V2,
        blocksize: 4096,
        maxlen: 1024,
        first: 1,
        sequence: 1,
        start: 0,
        errno: 0,
        feature_compat: 0,
        feature_incompat: incompat,
        feature_ro_compat: 0,
        uuid: [0; 16],
        checksum_type: 0,
        num_fc_blks: 0,
    };
    let sizes: Vec<usize> = LAYOUTS.iter().map(|&(_, f)| header(f).tag_bytes()).collect();
    assert_eq!(sizes, [8, 12, 10, 14, 16, 16]);
}

#[tokio::test]
async fn every_tag_layout_replays_its_blocks() {
    for &(name, incompat) in LAYOUTS {
        let dev = formatted(Profile::Ext4, 64 * MIB).await;
        let mut fs = Filesystem::open(&dev).await.unwrap();
        let bs = fs.block_size() as usize;
        let (a, b, c) = (spare(&fs, 0), spare(&fs, 1), spare(&fs, 2));
        let mut log = LogWriter::new(&fs, incompat).await;
        log.data(&fs, &[(a, pattern(bs, 1, 0xa1)), (b, pattern(bs, JBD2_MAGIC, 0xb2))])
            .await;
        log.commit(&fs).await;
        log.data(&fs, &[(c, pattern(bs, 3, 0xc3))]).await;
        log.commit(&fs).await;
        log.finish(&mut fs, true).await;

        let mut journal = Journal::open(&fs).await.unwrap().unwrap();
        let r = recover(&fs, &mut journal).await.unwrap();
        assert!(r.errors.is_empty(), "{name}: {:?}", r.errors);
        assert_eq!(r.transaction_count(), 2, "{name}");
        assert_eq!(r.blocks_replayed, 3, "{name}");
        assert_eq!(fs.read_block(a).await.unwrap(), pattern(bs, 1, 0xa1), "{name}");
        // The escaped block gets its magic back.
        assert_eq!(fs.read_block(b).await.unwrap(), pattern(bs, JBD2_MAGIC, 0xb2), "{name}");
        assert_eq!(fs.read_block(c).await.unwrap(), pattern(bs, 3, 0xc3), "{name}");

        // Empty now, and restarted past both transactions.
        let journal = Journal::open(&fs).await.unwrap().unwrap();
        assert!(journal.is_empty(), "{name}");
        assert_eq!(journal.header.sequence, log.start_seq + 3, "{name}");
    }
}

#[tokio::test]
async fn a_transaction_without_its_commit_is_discarded() {
    let dev = formatted(Profile::Ext4, 64 * MIB).await;
    let mut fs = Filesystem::open(&dev).await.unwrap();
    let bs = fs.block_size() as usize;
    let (a, b) = (spare(&fs, 0), spare(&fs, 1));
    let mut log = LogWriter::new(&fs, jbd2_incompat::CSUM_V3).await;
    log.data(&fs, &[(a, pattern(bs, 1, 0x11))]).await;
    log.commit(&fs).await;
    log.data(&fs, &[(b, pattern(bs, 2, 0x22))]).await;
    log.finish(&mut fs, true).await;

    let mut journal = Journal::open(&fs).await.unwrap().unwrap();
    let r = recover(&fs, &mut journal).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.transaction_count(), 1);
    assert_eq!(fs.read_block(a).await.unwrap(), pattern(bs, 1, 0x11));
    assert_eq!(fs.read_block(b).await.unwrap(), vec![0u8; bs]);
}

#[tokio::test]
async fn a_revoke_stops_earlier_copies_but_not_later_ones() {
    for &(name, incompat) in LAYOUTS {
        let dev = formatted(Profile::Ext4, 64 * MIB).await;
        let mut fs = Filesystem::open(&dev).await.unwrap();
        let bs = fs.block_size() as usize;
        let (a, b) = (spare(&fs, 0), spare(&fs, 1));
        let mut log = LogWriter::new(&fs, incompat).await;
        // T1 logs a and b; T2 revokes both; T3 logs b again.
        log.data(&fs, &[(a, pattern(bs, 1, 0x11)), (b, pattern(bs, 1, 0x12))]).await;
        log.commit(&fs).await;
        log.revoke(&fs, &[a, b]).await;
        log.commit(&fs).await;
        log.data(&fs, &[(b, pattern(bs, 3, 0x33))]).await;
        log.commit(&fs).await;
        log.finish(&mut fs, true).await;

        let mut journal = Journal::open(&fs).await.unwrap().unwrap();
        let r = recover(&fs, &mut journal).await.unwrap();
        assert!(r.errors.is_empty(), "{name}: {:?}", r.errors);
        assert_eq!(r.revokes, 2, "{name}");
        assert_eq!(r.revoke_hits, 2, "{name}");
        assert_eq!(fs.read_block(a).await.unwrap(), vec![0u8; bs], "{name}");
        assert_eq!(fs.read_block(b).await.unwrap(), pattern(bs, 3, 0x33), "{name}");
    }
}

#[tokio::test]
async fn the_log_wraps_from_its_end_to_its_first_block() {
    let dev = formatted(Profile::Ext4, 64 * MIB).await;
    let mut fs = Filesystem::open(&dev).await.unwrap();
    let bs = fs.block_size() as usize;
    let mut log = LogWriter::new(&fs, jbd2_incompat::CSUM_V3 | jbd2_incompat::REVOKE).await;
    log.start_at(log.maxlen - 2);
    let homes: Vec<u64> = (0..4).map(|i| spare(&fs, i)).collect();
    let blocks: Vec<(u64, Vec<u8>)> =
        homes.iter().map(|&h| (h, pattern(bs, h as u32, h as u8))).collect();
    let logged = log.data(&fs, &blocks).await;
    assert!(logged.contains(&log.first), "the test must actually wrap: {logged:?}");
    log.commit(&fs).await;
    log.finish(&mut fs, true).await;

    let mut journal = Journal::open(&fs).await.unwrap().unwrap();
    let r = recover(&fs, &mut journal).await.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    for (home, data) in blocks {
        assert_eq!(fs.read_block(home).await.unwrap(), data);
    }
}

#[tokio::test]
async fn a_data_block_failing_its_checksum_is_skipped_and_the_rest_replayed() {
    for incompat in [jbd2_incompat::CSUM_V2, jbd2_incompat::CSUM_V3] {
        let dev = formatted(Profile::Ext4, 64 * MIB).await;
        let mut fs = Filesystem::open(&dev).await.unwrap();
        let bs = fs.block_size() as usize;
        let (a, b) = (spare(&fs, 0), spare(&fs, 1));
        let mut log = LogWriter::new(&fs, incompat).await;
        let logged = log.data(&fs, &[(a, pattern(bs, 1, 0x11)), (b, pattern(bs, 2, 0x22))]).await;
        log.commit(&fs).await;
        log.finish(&mut fs, true).await;
        // Spoil a's logged copy after its checksum was taken.
        fs.write_block(log.map[logged[0] as usize], &pattern(bs, 1, 0x99)).await.unwrap();

        let mut journal = Journal::open(&fs).await.unwrap().unwrap();
        let r = recover(&fs, &mut journal).await.unwrap();
        assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
        assert_eq!(fs.read_block(a).await.unwrap(), vec![0u8; bs]);
        assert_eq!(fs.read_block(b).await.unwrap(), pattern(bs, 2, 0x22));
        // A failed recovery empties the journal at its old sequence.
        let journal = Journal::open(&fs).await.unwrap().unwrap();
        assert!(journal.is_empty());
        assert_eq!(journal.header.sequence, log.start_seq);
    }
}

#[tokio::test]
async fn a_commit_failing_its_checksum_ends_the_replay_before_it() {
    let dev = formatted(Profile::Ext4, 64 * MIB).await;
    let mut fs = Filesystem::open(&dev).await.unwrap();
    let bs = fs.block_size() as usize;
    let (a, b) = (spare(&fs, 0), spare(&fs, 1));
    let mut log = LogWriter::new(&fs, jbd2_incompat::CSUM_V3).await;
    log.data(&fs, &[(a, pattern(bs, 1, 0x11))]).await;
    log.commit(&fs).await;
    log.data(&fs, &[(b, pattern(bs, 2, 0x22))]).await;
    let commit = log.commit(&fs).await;
    log.finish(&mut fs, true).await;
    let mut torn = fs.read_block(log.map[commit as usize]).await.unwrap();
    torn[100] ^= 0xff;
    fs.write_block(log.map[commit as usize], &torn).await.unwrap();

    let mut journal = Journal::open(&fs).await.unwrap().unwrap();
    let r = recover(&fs, &mut journal).await.unwrap();
    assert_eq!(r.transaction_count(), 1);
    assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
    assert_ne!(r.errno, 0, "e2fsck records the corrupt transaction in s_errno");
    assert_eq!(fs.read_block(a).await.unwrap(), pattern(bs, 1, 0x11));
    assert_eq!(fs.read_block(b).await.unwrap(), vec![0u8; bs]);
}

#[tokio::test]
async fn fast_commit_and_unknown_features_are_not_replayed() {
    for incompat in [jbd2_incompat::FAST_COMMIT, 0x8000_0000] {
        let dev = formatted(Profile::Ext4, 64 * MIB).await;
        let mut fs = Filesystem::open(&dev).await.unwrap();
        let mut log = LogWriter::new(&fs, incompat).await;
        log.commit(&fs).await;
        log.finish(&mut fs, true).await;
        assert!(Journal::open(&fs).await.unwrap().is_err(), "{incompat:#x}");
    }
}

/// A repairing check replays, reports a note rather than a repair, and
/// finds the replayed filesystem clean: exit 0, as `e2fsck` exits.
#[tokio::test]
async fn fsck_replays_before_checking_and_does_not_count_it_as_a_repair() {
    let dev = formatted(Profile::Ext4, 64 * MIB).await;
    let mut fs = Filesystem::open(&dev).await.unwrap();
    let bs = fs.block_size() as usize;
    let a = spare(&fs, 0);
    let mut log = LogWriter::new(&fs, jbd2_incompat::CSUM_V3 | jbd2_incompat::SIXTY_FOUR_BIT).await;
    log.data(&fs, &[(a, pattern(bs, 7, 0x77))]).await;
    log.commit(&fs).await;
    log.finish(&mut fs, true).await;
    drop(fs);

    let report = check(&dev, &FsckOptions::repair()).await.unwrap();
    let codes: Vec<_> = report.problems.iter().map(|p| p.code).collect();
    assert_eq!(codes, ["journal-recovered"], "{:?}", report.problems);
    assert_eq!(report.exit_code(), 0);
    assert!(report.is_clean());

    let fs = Filesystem::open(&dev).await.unwrap();
    assert!(!fs.superblock().feature_incompat.contains(IncompatFeatures::RECOVER));
    assert_eq!(fs.read_block(a).await.unwrap(), pattern(bs, 7, 0x77));
    assert!(check(&dev, &FsckOptions::check_only().force(true)).await.unwrap().is_clean());
}

/// The replay can rewrite the superblock itself; the check must read the
/// replayed one, not the one it opened.
#[tokio::test]
async fn fsck_reloads_a_superblock_the_journal_replays() {
    let dev = formatted(Profile::Ext4, 64 * MIB).await;
    let mut fs = Filesystem::open(&dev).await.unwrap();
    // A superblock block with a new label, as a kernel would have logged it.
    fs.superblock_mut().volume_name[..8].copy_from_slice(b"replayed");
    fs.flush_superblock().await.unwrap();
    let block0 = fs.read_block(0).await.unwrap();
    fs.superblock_mut().volume_name = [0; 16];
    fs.flush_superblock().await.unwrap();

    let mut log = LogWriter::new(&fs, jbd2_incompat::CSUM_V3).await;
    log.data(&fs, &[(0, block0)]).await;
    log.commit(&fs).await;
    log.finish(&mut fs, true).await;
    drop(fs);

    let report = check(&dev, &FsckOptions::repair()).await.unwrap();
    assert_eq!(report.exit_code(), 0, "{:?}", report.problems);
    let fs = Filesystem::open(&dev).await.unwrap();
    assert_eq!(&fs.superblock().volume_name[..8], b"replayed");
    assert!(!fs.superblock().feature_incompat.contains(IncompatFeatures::RECOVER));
}

#[tokio::test]
async fn a_read_only_check_skips_recovery_and_writes_nothing() {
    let dev = formatted(Profile::Ext4, 64 * MIB).await;
    let mut fs = Filesystem::open(&dev).await.unwrap();
    let bs = fs.block_size() as usize;
    let mut log = LogWriter::new(&fs, jbd2_incompat::CSUM_V3).await;
    log.data(&fs, &[(spare(&fs, 0), pattern(bs, 7, 0x77))]).await;
    log.commit(&fs).await;
    log.finish(&mut fs, true).await;
    drop(fs);
    let before = dev.to_vec();

    let report = check(&dev, &FsckOptions::check_only()).await.unwrap();
    assert!(report.notes().any(|p| p.code == "journal-recovery-skipped"));
    assert_eq!(report.exit_code(), 0, "{:?}", report.problems);
    assert!(dev.to_vec() == before, "a read-only check wrote to the device");
}

#[tokio::test]
async fn journal_data_with_the_flag_clear_is_replayed_by_y_and_stops_p() {
    let setup = || async {
        let dev = formatted(Profile::Ext4, 64 * MIB).await;
        let mut fs = Filesystem::open(&dev).await.unwrap();
        let bs = fs.block_size() as usize;
        let mut log = LogWriter::new(&fs, jbd2_incompat::CSUM_V3).await;
        log.data(&fs, &[(spare(&fs, 0), pattern(bs, 7, 0x77))]).await;
        log.commit(&fs).await;
        log.finish(&mut fs, false).await;
        dev
    };

    let dev = setup().await;
    let report = check(&dev, &FsckOptions::repair()).await.unwrap();
    let p = report.problems.iter().find(|p| p.code == "journal-has-data").unwrap();
    assert!(p.fixed);
    assert_eq!(report.exit_code(), 1);
    let fs = Filesystem::open(&dev).await.unwrap();
    assert!(Journal::open(&fs).await.unwrap().unwrap().is_empty());

    let dev = setup().await;
    let report = check(&dev, &FsckOptions::preen()).await.unwrap();
    assert!(report.preen_halted);
    assert_eq!(report.exit_code(), 4);
    let fs = Filesystem::open(&dev).await.unwrap();
    assert!(!Journal::open(&fs).await.unwrap().unwrap().is_empty());
}

/// A journal that needs recovery and cannot be replayed here: nothing
/// written, whatever the mode, and the filesystem is not called clean.
#[tokio::test]
async fn an_unreplayable_journal_stops_a_repair_before_it_writes() {
    let dev = formatted(Profile::Ext4, 64 * MIB).await;
    let mut fs = Filesystem::open(&dev).await.unwrap();
    let mut log = LogWriter::new(&fs, jbd2_incompat::FAST_COMMIT).await;
    log.commit(&fs).await;
    log.finish(&mut fs, true).await;
    drop(fs);
    let before = dev.to_vec();

    let report = check(&dev, &FsckOptions::repair()).await.unwrap();
    assert!(report.problems.iter().any(|p| p.code == "journal-unreplayable"));
    assert_eq!(report.exit_code(), 4);
    assert!(dev.to_vec() == before, "the repair wrote under an unreplayed journal");
}

#[tokio::test]
async fn an_aborted_journal_marks_the_filesystem_as_having_errors() {
    let dev = formatted(Profile::Ext4, 64 * MIB).await;
    let fs = Filesystem::open(&dev).await.unwrap();
    let journal = Journal::open(&fs).await.unwrap().unwrap();
    let mut raw = journal.raw.clone();
    raw[off::S_ERRNO..off::S_ERRNO + 4].copy_from_slice(&(-5i32).to_be_bytes());
    fs.write_block(journal.map[0], &raw).await.unwrap();
    drop(fs);

    let report = check(&dev, &FsckOptions::repair()).await.unwrap();
    assert!(report.problems.iter().any(|p| p.code == "fs-has-errors"), "{:?}", report.problems);
    let fs = Filesystem::open(&dev).await.unwrap();
    assert_eq!(Journal::open(&fs).await.unwrap().unwrap().header.errno, 0);
}

/// A caller may spawn a check on a multi-threaded runtime, so its future
/// must stay `Send` with replay and orphan release in it.
#[test]
fn the_check_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let dev = MemDevice::new(MIB);
    let options = FsckOptions::repair();
    let future = check(&dev, &options);
    assert_send(&future);
}
