//! Journal recovery: replaying a JBD2 journal into the filesystem.
//!
//! What `e2fsck` does before it checks anything (`e2fsck_run_ext3_journal` in
//! `e2fsck/journal.c`), and what the kernel does at mount: the transactions a
//! crash left committed in the log are written to their home locations, so
//! the metadata the check then reads is the metadata the filesystem actually
//! has. Checking without replaying first would "repair" bitmaps and counts
//! against blocks the journal is about to overwrite (mkfs.ext4.rs#7).
//!
//! The replay is `jbd2_journal_recover` (`e2fsck/recovery.c`, shared with the
//! kernel's `fs/jbd2/recovery.c`), in its three passes:
//!
//! 1. **Scan** from `s_start` for the last complete transaction: a run of
//!    descriptor, data, revoke and commit blocks whose sequence numbers follow
//!    on from `s_sequence`, ending at the first block that is not the next one
//!    expected. A transaction without its commit block is discarded.
//! 2. **Revoke**: collect every revoke record, keeping the latest transaction
//!    that revoked each block.
//! 3. **Replay**: write each logged block to its home, unless a transaction at
//!    or after this one revoked it. A block whose first four bytes were the
//!    JBD2 magic was logged escaped, and gets them back.
//!
//! Descriptor tags come in the four layouts the journal's features select
//! (32- or 64-bit block numbers, `csum_v2` with a 16-bit tag checksum,
//! `csum_v3` with a 32-bit one), and with checksums on every descriptor,
//! revoke, commit and data block is verified as the kernel verifies it.
//!
//! Then the journal is marked empty (`s_start = 0`) and restarted past the
//! last transaction, as `e2fsck_journal_release` leaves it.
//!
//! Not replayed, and reported instead as [`Unreplayable`]: an external
//! journal (this crate sees one device), a journal with `fast_commit`
//! (its replay is a separate protocol, `fc_do_one_pass`), and a journal
//! superblock this code does not understand.

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use std::collections::BTreeMap;

use crate::csum;
use crate::device::BlockDevice;
use crate::error::Result;
use crate::features::{CompatFeatures, IncompatFeatures};
use crate::fs::{BlockKind, Filesystem};
use crate::journal::{get_u32_be, jbd2_incompat, off, JBD2_MAGIC, JOURNAL_SB_LEN};
use crate::structs::superblock::ino;

/// `JBD2_DESCRIPTOR_BLOCK`
pub const DESCRIPTOR_BLOCK: u32 = 1;
/// `JBD2_COMMIT_BLOCK`
pub const COMMIT_BLOCK: u32 = 2;
/// `JBD2_SUPERBLOCK_V1`
pub const SUPERBLOCK_V1: u32 = 3;
/// `JBD2_SUPERBLOCK_V2`
pub const SUPERBLOCK_V2: u32 = 4;
/// `JBD2_REVOKE_BLOCK`
pub const REVOKE_BLOCK: u32 = 5;

/// `JBD2_FEATURE_COMPAT_CHECKSUM`: the old crc32 over a transaction's data,
/// in its commit block. Accepted, not verified (see [`recover`]).
pub const COMPAT_CHECKSUM: u32 = 0x1;

/// `JBD2_CRC32C_CHKSUM`, the journal's `s_checksum_type` under csum v2/v3.
pub const CRC32C_CHKSUM: u8 = 4;

/// Descriptor tag flags (`JBD2_FLAG_*`).
pub mod tag_flag {
    /// The block's first four bytes were the JBD2 magic, and were zeroed in
    /// the log so a scan would not take the block for a journal header.
    pub const ESCAPE: u16 = 1;
    /// No UUID follows this tag: it is the previous tag's.
    pub const SAME_UUID: u16 = 2;
    /// `JBD2_FLAG_DELETED`, unused by replay.
    pub const DELETED: u16 = 4;
    /// The last tag in this descriptor block.
    pub const LAST_TAG: u16 = 8;
}

/// Incompatible journal features this code replays.
const KNOWN_INCOMPAT: u32 = jbd2_incompat::REVOKE
    | jbd2_incompat::SIXTY_FOUR_BIT
    | jbd2_incompat::ASYNC_COMMIT
    | jbd2_incompat::CSUM_V2
    | jbd2_incompat::CSUM_V3;

/// Bytes of `journal_header_t`.
const HEADER_LEN: usize = 12;
/// Bytes of `jbd2_journal_revoke_header_t`: the header and `r_count`.
const REVOKE_HEADER_LEN: usize = 16;
/// Bytes of `struct jbd2_journal_block_tail`, at the end of descriptor and
/// revoke blocks under csum v2/v3.
const BLOCK_TAIL_LEN: usize = 4;
/// Offset of `h_chksum[0]` in `struct commit_header`.
const COMMIT_CHKSUM: usize = 16;
/// Offset of `h_commit_sec` in `struct commit_header`.
const COMMIT_SEC: usize = 48;

/// Why a journal that needs replaying cannot be replayed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreplayable(pub String);

impl core::fmt::Display for Unreplayable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The journal superblock fields recovery reads (`journal_superblock_t`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalHeader {
    /// `h_blocktype`: [`SUPERBLOCK_V1`] or [`SUPERBLOCK_V2`].
    pub blocktype: u32,
    /// `s_blocksize`
    pub blocksize: u32,
    /// `s_maxlen`: blocks in the journal, this superblock included.
    pub maxlen: u32,
    /// `s_first`: the first block of the log.
    pub first: u32,
    /// `s_sequence`: the first transaction expected in the log.
    pub sequence: u32,
    /// `s_start`: where the log starts; zero when it is empty.
    pub start: u32,
    /// `s_errno`: nonzero when the kernel aborted the journal.
    pub errno: i32,
    /// `s_feature_compat`
    pub feature_compat: u32,
    /// `s_feature_incompat`
    pub feature_incompat: u32,
    /// `s_feature_ro_compat`
    pub feature_ro_compat: u32,
    /// `s_uuid`
    pub uuid: [u8; 16],
    /// `s_checksum_type`
    pub checksum_type: u8,
    /// `s_num_fc_blks`
    pub num_fc_blks: u32,
}

impl JournalHeader {
    /// Decode a journal superblock, checking the magic and block type.
    pub fn decode(buf: &[u8]) -> core::result::Result<Self, Unreplayable> {
        if buf.len() < JOURNAL_SB_LEN || get_u32_be(buf, off::H_MAGIC) != JBD2_MAGIC {
            return Err(Unreplayable("journal superblock has no JBD2 magic".into()));
        }
        let blocktype = get_u32_be(buf, off::H_BLOCKTYPE);
        if blocktype != SUPERBLOCK_V1 && blocktype != SUPERBLOCK_V2 {
            return Err(Unreplayable(format!(
                "journal superblock has block type {blocktype}"
            )));
        }
        let v2 = blocktype == SUPERBLOCK_V2;
        let field = |at: usize| if v2 { get_u32_be(buf, at) } else { 0 };
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&buf[off::S_UUID..off::S_UUID + 16]);
        Ok(Self {
            blocktype,
            blocksize: get_u32_be(buf, off::S_BLOCKSIZE),
            maxlen: get_u32_be(buf, off::S_MAXLEN),
            first: get_u32_be(buf, off::S_FIRST),
            sequence: get_u32_be(buf, off::S_SEQUENCE),
            start: get_u32_be(buf, off::S_START),
            errno: get_u32_be(buf, off::S_ERRNO) as i32,
            feature_compat: field(off::S_FEATURE_COMPAT),
            feature_incompat: field(off::S_FEATURE_INCOMPAT),
            feature_ro_compat: field(off::S_FEATURE_RO_COMPAT),
            uuid,
            checksum_type: if v2 { buf[off::S_CHECKSUM_TYPE] } else { 0 },
            num_fc_blks: field(off::S_NUM_FC_BLKS),
        })
    }

    fn has(&self, feature: u32) -> bool {
        self.feature_incompat & feature != 0
    }

    /// Whether descriptor, revoke, commit and data blocks carry crc32c.
    pub fn csum_v2_or_v3(&self) -> bool {
        self.has(jbd2_incompat::CSUM_V2) || self.has(jbd2_incompat::CSUM_V3)
    }

    /// Bytes of one descriptor tag, before any UUID: `journal_tag_bytes`.
    pub fn tag_bytes(&self) -> usize {
        if self.has(jbd2_incompat::CSUM_V3) {
            return 16;
        }
        let mut size = 12;
        if self.has(jbd2_incompat::CSUM_V2) {
            size += 2;
        }
        if self.has(jbd2_incompat::SIXTY_FOUR_BIT) {
            size
        } else {
            size - 4
        }
    }
}

/// An internal journal, opened for recovery.
pub struct Journal {
    /// The decoded journal superblock.
    pub header: JournalHeader,
    /// The journal superblock block as read, for rewriting it.
    raw: Vec<u8>,
    /// Physical block of each logical journal block.
    map: Vec<u64>,
    /// `j_csum_seed`: crc32c of the journal UUID.
    csum_seed: u32,
    /// One past the last block of the log (`j_last`).
    last: u32,
}

/// What a replay did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovery {
    /// The first transaction replayed, and one past the last.
    pub transactions: (u32, u32),
    /// Blocks written to their home locations.
    pub blocks_replayed: u64,
    /// Logged blocks skipped because a later transaction revoked them.
    pub revoke_hits: u64,
    /// Revoke records read.
    pub revokes: u64,
    /// What went wrong, if anything: a checksum that did not match, a
    /// transaction found corrupt. Replay goes on past a bad data block as the
    /// kernel does; any entry here means the filesystem wants a full check.
    pub errors: Vec<String>,
    /// The journal's `s_errno`: nonzero when the kernel had aborted it.
    pub errno: i32,
}

impl Recovery {
    /// Transactions replayed.
    pub fn transaction_count(&self) -> u32 {
        self.transactions.1.wrapping_sub(self.transactions.0)
    }
}

/// `tid_gt`: sequence numbers compare modulo 2^32.
fn tid_gt(x: u32, y: u32) -> bool {
    (x.wrapping_sub(y) as i32) > 0
}

/// `tid_geq`
fn tid_geq(x: u32, y: u32) -> bool {
    (x.wrapping_sub(y) as i32) >= 0
}

impl Journal {
    /// Open the filesystem's internal journal.
    ///
    /// `Ok(Err(_))` is a journal that exists but cannot be replayed here: no
    /// internal journal inode, a superblock that does not decode, features
    /// this code does not know. `e2fsck_journal_load`'s checks, in its order.
    pub async fn open<D: BlockDevice>(
        fs: &Filesystem<D>,
    ) -> Result<core::result::Result<Self, Unreplayable>> {
        let sb = fs.superblock();
        if !sb.feature_compat.contains(CompatFeatures::HAS_JOURNAL) {
            return Ok(Err(Unreplayable("the filesystem has no journal".into())));
        }
        if sb.feature_incompat.contains(IncompatFeatures::JOURNAL_DEV) {
            return Ok(Err(Unreplayable("this is an external journal device".into())));
        }
        if sb.journal_inum == 0 {
            return Ok(Err(Unreplayable(
                "the journal is external (s_journal_inum 0); only an internal journal can be replayed"
                    .into(),
            )));
        }
        if sb.journal_inum != ino::JOURNAL {
            return Ok(Err(Unreplayable(format!(
                "the journal is inode {}, not the reserved journal inode {}",
                sb.journal_inum,
                ino::JOURNAL
            ))));
        }

        let inode = fs.read_inode(sb.journal_inum).await?;
        if !inode.has_block_map() || inode.links_count == 0 {
            return Ok(Err(Unreplayable("the journal inode is not in use".into())));
        }
        let block_size = fs.block_size() as u64;
        let blocks = inode.size / block_size;
        if blocks < 2 {
            return Ok(Err(Unreplayable(format!(
                "the journal inode is {} bytes, too small to be a journal",
                inode.size
            ))));
        }

        // The whole map up front: a journal is at most a few hundred thousand
        // blocks, and replay reads it in order, wrapping round.
        let mut map = vec![0u64; blocks as usize];
        let mut bad = None;
        let walked = fs
            .walk_blocks(&inode, |b| {
                if b.kind != BlockKind::Data {
                    return;
                }
                if let Some(logical) = b.logical {
                    if logical < blocks {
                        map[logical as usize] = b.physical;
                    }
                }
            })
            .await;
        if let Err(e) = walked {
            return Ok(Err(Unreplayable(format!("the journal inode's blocks: {e}"))));
        }
        for (logical, &physical) in map.iter().enumerate() {
            if physical == 0 || physical >= sb.blocks_count {
                bad = Some(logical);
                break;
            }
        }
        if let Some(logical) = bad {
            return Ok(Err(Unreplayable(format!(
                "journal block {logical} is not mapped to a block of the filesystem"
            ))));
        }

        let raw = fs.read_block(map[0]).await?;
        let header = match JournalHeader::decode(&raw) {
            Ok(h) => h,
            Err(e) => return Ok(Err(e)),
        };

        if header.blocksize as u64 != block_size {
            return Ok(Err(Unreplayable(format!(
                "journal block size {} differs from the filesystem's {block_size}",
                header.blocksize
            ))));
        }
        let unknown = header.feature_incompat & !KNOWN_INCOMPAT;
        if header.has(jbd2_incompat::FAST_COMMIT) {
            return Ok(Err(Unreplayable(
                "the journal uses fast_commit, which this checker does not replay".into(),
            )));
        }
        if unknown != 0 {
            return Ok(Err(Unreplayable(format!(
                "unknown journal incompatible features {unknown:#x}"
            ))));
        }
        if header.feature_ro_compat != 0 {
            return Ok(Err(Unreplayable(format!(
                "unknown journal read-only features {:#x}",
                header.feature_ro_compat
            ))));
        }
        if header.has(jbd2_incompat::CSUM_V2) && header.has(jbd2_incompat::CSUM_V3) {
            return Ok(Err(Unreplayable(
                "the journal superblock claims both csum v2 and v3".into(),
            )));
        }
        if header.csum_v2_or_v3() {
            if header.checksum_type != CRC32C_CHKSUM {
                return Ok(Err(Unreplayable(format!(
                    "journal checksum type {} is not crc32c",
                    header.checksum_type
                ))));
            }
            let mut copy = raw[..JOURNAL_SB_LEN].to_vec();
            copy[off::S_CHECKSUM..off::S_CHECKSUM + 4].fill(0);
            let want = csum::crc32c(!0, &copy);
            if get_u32_be(&raw, off::S_CHECKSUM) != want {
                return Ok(Err(Unreplayable("journal superblock checksum does not match".into())));
            }
        }
        // e2fsck shortens a journal whose superblock claims less than its
        // inode holds, and refuses one that claims more.
        if header.maxlen as u64 > blocks {
            return Ok(Err(Unreplayable(format!(
                "journal superblock claims {} blocks, the inode holds {blocks}",
                header.maxlen
            ))));
        }
        if header.first == 0 || header.first >= header.maxlen {
            return Ok(Err(Unreplayable(format!(
                "journal first block {} is outside 1..{}",
                header.first, header.maxlen
            ))));
        }
        if header.start != 0 && (header.start < header.first || header.start >= header.maxlen) {
            return Ok(Err(Unreplayable(format!(
                "journal log start {} is outside {}..{}",
                header.start, header.first, header.maxlen
            ))));
        }

        let csum_seed = csum::crc32c(!0, &header.uuid);
        let last = header.maxlen;
        map.truncate(last as usize);
        Ok(Ok(Self {
            header,
            raw,
            map,
            csum_seed,
            last,
        }))
    }

    /// Whether the log holds nothing to replay.
    pub fn is_empty(&self) -> bool {
        self.header.start == 0
    }

    /// The next log block after `block`, wrapping from the end of the log
    /// back to `s_first`.
    fn wrap(&self, block: u32) -> u32 {
        if block >= self.last {
            block - (self.last - self.header.first)
        } else {
            block
        }
    }

    async fn read<D: BlockDevice>(&self, fs: &Filesystem<D>, block: u32) -> Result<Vec<u8>> {
        fs.read_block(self.map[block as usize]).await
    }

    /// `jbd2_descriptor_block_csum_verify`, for descriptor and revoke blocks.
    fn block_tail_ok(&self, buf: &[u8]) -> bool {
        if !self.header.csum_v2_or_v3() {
            return true;
        }
        let at = buf.len() - BLOCK_TAIL_LEN;
        let provided = get_u32_be(buf, at);
        let mut copy = buf.to_vec();
        copy[at..].fill(0);
        provided == csum::crc32c(self.csum_seed, &copy)
    }

    /// `jbd2_commit_block_csum_verify`.
    fn commit_ok(&self, buf: &[u8]) -> bool {
        if !self.header.csum_v2_or_v3() {
            return true;
        }
        let provided = get_u32_be(buf, COMMIT_CHKSUM);
        let mut copy = buf.to_vec();
        copy[COMMIT_CHKSUM..COMMIT_CHKSUM + 4].fill(0);
        provided == csum::crc32c(self.csum_seed, &copy)
    }

    /// `jbd2_block_tag_csum_verify`.
    fn data_ok(&self, tag: &[u8], data: &[u8], sequence: u32) -> bool {
        if !self.header.csum_v2_or_v3() {
            return true;
        }
        let mut crc = csum::crc32c(self.csum_seed, &sequence.to_be_bytes());
        crc = csum::crc32c(crc, data);
        if self.header.has(jbd2_incompat::CSUM_V3) {
            get_u32_be(tag, 12) == crc
        } else {
            u16::from_be_bytes([tag[4], tag[5]]) == crc as u16
        }
    }

    /// The tags in a descriptor block: (tag bytes, home block, flags).
    fn tags<'a>(&self, buf: &'a [u8]) -> Vec<(&'a [u8], u64, u16)> {
        let tag_bytes = self.header.tag_bytes();
        let tail = if self.header.csum_v2_or_v3() { BLOCK_TAIL_LEN } else { 0 };
        let limit = buf.len() - tail;
        let mut out = Vec::new();
        let mut at = HEADER_LEN;
        while at + tag_bytes <= limit {
            let tag = &buf[at..at + tag_bytes];
            // t_flags is the low half of a 32-bit field in a v3 tag and a
            // 16-bit field in the others, at the same offset either way.
            let flags = u16::from_be_bytes([tag[6], tag[7]]);
            let mut block = get_u32_be(tag, 0) as u64;
            if self.header.has(jbd2_incompat::SIXTY_FOUR_BIT) {
                block |= (get_u32_be(tag, 8) as u64) << 32;
            }
            out.push((tag, block, flags));
            at += tag_bytes;
            if flags & tag_flag::SAME_UUID == 0 {
                at += 16;
            }
            if flags & tag_flag::LAST_TAG != 0 {
                break;
            }
        }
        out
    }

    /// Revoke records in a revoke block: `scan_revoke_records`.
    fn revoke_records(&self, buf: &[u8]) -> core::result::Result<Vec<u64>, String> {
        let tail = if self.header.csum_v2_or_v3() { BLOCK_TAIL_LEN } else { 0 };
        let count = get_u32_be(buf, HEADER_LEN) as usize;
        if count > buf.len() - tail {
            return Err(format!("revoke block claims {count} bytes of records"));
        }
        let record = if self.header.has(jbd2_incompat::SIXTY_FOUR_BIT) { 8 } else { 4 };
        let mut out = Vec::new();
        let mut at = REVOKE_HEADER_LEN;
        while at + record <= count {
            out.push(if record == 4 {
                get_u32_be(buf, at) as u64
            } else {
                (get_u32_be(buf, at) as u64) << 32 | get_u32_be(buf, at + 4) as u64
            });
            at += record;
        }
        Ok(out)
    }

    /// Pass one: find the end of the log. Returns one past the last complete
    /// transaction, and an error that fails the whole recovery, if any.
    async fn scan<D: BlockDevice>(
        &self,
        fs: &Filesystem<D>,
        recovery: &mut Recovery,
    ) -> Result<core::result::Result<u32, String>> {
        let mut next_commit = self.header.sequence;
        let mut next_block = self.header.start;
        let mut last_commit_time = 0u64;
        let mut need_check_commit_time = false;
        // Bounded by the log: a scan that has read every block once is done.
        for _ in 0..self.last {
            let buf = self.read(fs, next_block).await?;
            next_block = self.wrap(next_block + 1);
            if get_u32_be(&buf, 0) != JBD2_MAGIC || get_u32_be(&buf, 8) != next_commit {
                break;
            }
            match get_u32_be(&buf, 4) {
                DESCRIPTOR_BLOCK => {
                    // A stale descriptor from before lazy journal init can fail
                    // its checksum; whether it matters is settled at the commit.
                    if !self.block_tail_ok(&buf) {
                        need_check_commit_time = true;
                    }
                    next_block = self.wrap(next_block + self.tags(&buf).len() as u32);
                }
                COMMIT_BLOCK => {
                    let commit_time = u64::from_be_bytes(
                        buf[COMMIT_SEC..COMMIT_SEC + 8].try_into().unwrap(),
                    );
                    if !self.commit_ok(&buf) {
                        if commit_time < last_commit_time {
                            // An older journal's leftovers, not a torn commit.
                            break;
                        }
                        recovery.errors.push(format!(
                            "journal transaction {next_commit} was corrupt, replay was aborted"
                        ));
                        if !self.header.has(jbd2_incompat::ASYNC_COMMIT) {
                            recovery.errno = -22; // -EINVAL, as e2fsck records it
                        }
                        return Ok(Ok(next_commit));
                    }
                    if need_check_commit_time {
                        if commit_time >= last_commit_time {
                            return Ok(Err(format!(
                                "invalid checksum found in journal transaction {next_commit}"
                            )));
                        }
                        // Stale data from an earlier use of the journal.
                        break;
                    }
                    last_commit_time = commit_time;
                    next_commit = next_commit.wrapping_add(1);
                }
                REVOKE_BLOCK => {
                    if !self.block_tail_ok(&buf) {
                        need_check_commit_time = true;
                    }
                }
                _ => break,
            }
        }
        Ok(Ok(next_commit))
    }

    /// Passes two and three: revoke records, then the replay, each walking the
    /// log from `s_start` up to `end`.
    async fn revoke_or_replay<D: BlockDevice>(
        &self,
        fs: &Filesystem<D>,
        end: u32,
        revoked: &mut BTreeMap<u64, u32>,
        replay: bool,
        recovery: &mut Recovery,
    ) -> Result<core::result::Result<(), String>> {
        let mut next_commit = self.header.sequence;
        let mut next_block = self.header.start;
        while !tid_geq(next_commit, end) {
            let buf = self.read(fs, next_block).await?;
            next_block = self.wrap(next_block + 1);
            if get_u32_be(&buf, 0) != JBD2_MAGIC || get_u32_be(&buf, 8) != next_commit {
                break;
            }
            match get_u32_be(&buf, 4) {
                DESCRIPTOR_BLOCK => {
                    if replay && !self.block_tail_ok(&buf) {
                        return Ok(Err(format!(
                            "invalid checksum on a descriptor block of journal transaction {next_commit}"
                        )));
                    }
                    for (tag, home, flags) in self.tags(&buf) {
                        let logged = next_block;
                        next_block = self.wrap(next_block + 1);
                        if !replay {
                            continue;
                        }
                        if revoked.get(&home).is_some_and(|&seq| !tid_gt(next_commit, seq)) {
                            recovery.revoke_hits += 1;
                            continue;
                        }
                        let mut data = self.read(fs, logged).await?;
                        if !self.data_ok(tag, &data, next_commit) {
                            recovery.errors.push(format!(
                                "invalid checksum recovering data block {home} in the journal"
                            ));
                            continue;
                        }
                        if home >= fs.superblock().blocks_count {
                            recovery.errors.push(format!(
                                "journal block for block {home}, past the end of the filesystem"
                            ));
                            continue;
                        }
                        if flags & tag_flag::ESCAPE != 0 {
                            data[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
                        }
                        fs.write_block(home, &data).await?;
                        recovery.blocks_replayed += 1;
                    }
                }
                COMMIT_BLOCK => next_commit = next_commit.wrapping_add(1),
                REVOKE_BLOCK => {
                    if replay {
                        continue;
                    }
                    let records = match self.revoke_records(&buf) {
                        Ok(r) => r,
                        Err(e) => return Ok(Err(e)),
                    };
                    for block in records {
                        recovery.revokes += 1;
                        let seq = revoked.entry(block).or_insert(next_commit);
                        if tid_gt(next_commit, *seq) {
                            *seq = next_commit;
                        }
                    }
                }
                _ => break,
            }
        }
        if next_commit != end {
            return Ok(Err(format!(
                "journal recovery ended at transaction {next_commit}, expected {end}"
            )));
        }
        Ok(Ok(()))
    }

    /// Clear `s_errno`, once the caller has carried it into the filesystem's
    /// `ERROR_FS`: what `e2fsck_check_ext3_journal` does on every check.
    pub async fn clear_errno<D: BlockDevice>(&mut self, fs: &Filesystem<D>) -> Result<()> {
        self.raw[off::S_ERRNO..off::S_ERRNO + 4].fill(0);
        self.header.errno = 0;
        self.stamp();
        fs.write_block(self.map[0], &self.raw).await
    }

    /// Recompute the journal superblock checksum, under csum v2/v3.
    fn stamp(&mut self) {
        if self.header.csum_v2_or_v3() {
            let buf = &mut self.raw;
            buf[off::S_CHECKSUM..off::S_CHECKSUM + 4].fill(0);
            let crc = csum::crc32c(!0, &buf[..JOURNAL_SB_LEN]);
            buf[off::S_CHECKSUM..off::S_CHECKSUM + 4].copy_from_slice(&crc.to_be_bytes());
        }
    }

    /// Mark the journal empty, starting again at `sequence`, and clear
    /// `s_errno`: what `e2fsck_journal_release(.., reset = 1, ..)` writes.
    async fn reset<D: BlockDevice>(&mut self, fs: &Filesystem<D>, sequence: u32) -> Result<()> {
        let buf = &mut self.raw;
        buf[off::S_SEQUENCE..off::S_SEQUENCE + 4].copy_from_slice(&sequence.to_be_bytes());
        buf[off::S_START..off::S_START + 4].fill(0);
        buf[off::S_ERRNO..off::S_ERRNO + 4].fill(0);
        self.stamp();
        self.header.sequence = sequence;
        self.header.start = 0;
        self.header.errno = 0;
        fs.write_block(self.map[0], &self.raw).await
    }
}

/// Replay the journal into the filesystem and mark the journal empty.
///
/// The caller then reloads the filesystem ([`Filesystem::reload`]), since the
/// superblock and descriptors may be among the blocks replayed, and clears
/// `needs_recovery`; [`crate::fsck`] does both. An error in [`Recovery`]
/// means some of the log was not replayed, and the filesystem wants a full
/// check.
///
/// The old `JBD2_FEATURE_COMPAT_CHECKSUM` (a crc32 of a transaction's data in
/// its commit block, superseded by csum v2/v3) is not verified: a commit
/// block is only written after the blocks it covers.
pub async fn recover<D: BlockDevice>(fs: &Filesystem<D>, journal: &mut Journal) -> Result<Recovery> {
    let mut recovery = Recovery {
        errno: journal.header.errno,
        ..Default::default()
    };
    let start = journal.header.sequence;
    if journal.is_empty() {
        // jbd2_journal_recover: "No recovery required", and the next
        // transaction is the one after s_sequence.
        recovery.transactions = (start, start);
        journal.reset(fs, start.wrapping_add(1)).await?;
        return Ok(recovery);
    }

    let mut revoked = BTreeMap::new();
    let outcome: core::result::Result<u32, Option<String>> = match journal
        .scan(fs, &mut recovery)
        .await?
    {
        Err(e) => Err(Some(e)),
        Ok(end) => {
            recovery.transactions = (start, end);
            match journal
                .revoke_or_replay(fs, end, &mut revoked, false, &mut recovery)
                .await?
            {
                Err(e) => Err(Some(e)),
                Ok(()) => {
                    // A data block that fails its checksum is skipped and the
                    // replay goes on, but the recovery as a whole has failed
                    // (`block_error` in do_one_pass), as any error here has.
                    let before = recovery.errors.len();
                    match journal
                        .revoke_or_replay(fs, end, &mut revoked, true, &mut recovery)
                        .await?
                    {
                        Ok(()) if recovery.errors.len() == before => Ok(end),
                        Ok(()) => Err(None),
                        Err(e) => Err(Some(e)),
                    }
                }
            }
        }
    };
    let sequence = match outcome {
        // The log restarts past every transaction it held, so nothing in it
        // can be taken for a commit again.
        Ok(end) => end.wrapping_add(1),
        Err(e) => {
            // e2fsck still empties the journal, at its old sequence.
            recovery.errors.extend(e);
            start
        }
    };
    fs.device().flush().await?;
    journal.reset(fs, sequence).await?;
    fs.device().flush().await?;
    Ok(recovery)
}

#[cfg(test)]
pub(crate) mod tests;
