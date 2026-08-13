//! Hashed directory trees — the `dir_index` feature.
//!
//! An indexed directory is still a perfectly ordinary directory to anything
//! that does not know about the index. Its first block holds `.` and `..`
//! followed by an index, but the `..` entry's `rec_len` covers the rest of the
//! block, so a reader walking entries linearly sees the two names and steps
//! straight over the index. Interior nodes are whole blocks claimed by a single
//! entry with inode 0. That is the trick the whole design rests on: the tree is
//! invisible to a linear reader, which is why `dir_index` is a *compatible*
//! feature and an old kernel can still mount the filesystem.
//!
//! Mirrors `struct ext2_dx_root_info`, `struct ext2_dx_entry`,
//! `struct ext2_dx_countlimit` and `struct ext2_dx_tail` from
//! e2fsprogs `lib/ext2fs/ext2_fs.h`, with the hash from
//! `lib/ext2fs/dirhash.c`.

use crate::bytes::{get_u16, get_u32, put_u16, put_u32};
use crate::csum::crc32c;
use crate::error::{Error, Result};
use crate::structs::dirent;

/// One index entry: a hash and the block that holds names at or above it.
pub const ENTRY_LEN: usize = 8;

/// `struct ext2_dx_tail` — the checksum that closes an index block.
pub const TAIL_LEN: usize = 8;

/// Where `struct ext2_dx_root_info` sits: after the fake `.` and `..`.
pub const ROOT_INFO_OFFSET: usize = 24;

/// `info_length` — the size of `struct ext2_dx_root_info`.
pub const ROOT_INFO_LEN: u8 = 8;

/// Where the count/limit pair sits in a root block.
pub const ROOT_COUNT_OFFSET: usize = 32;

/// Where the count/limit pair sits in an interior node block.
pub const NODE_COUNT_OFFSET: usize = 8;

/// Hash algorithms (`s_def_hash_version`).
///
/// The three `_UNSIGNED` numbers never appear on disk. They exist because C's
/// `char` is signed on x86 and unsigned on ARM, so the same name hashed to two
/// different values depending on who built the filesystem. The superblock
/// records which convention was used in `s_flags`, and these select it.
pub mod version {
    /// `EXT2_HASH_LEGACY`
    pub const LEGACY: u8 = 0;
    /// `EXT2_HASH_HALF_MD4`
    pub const HALF_MD4: u8 = 1;
    /// `EXT2_HASH_TEA`
    pub const TEA: u8 = 2;
    /// `EXT2_HASH_LEGACY_UNSIGNED`
    pub const LEGACY_UNSIGNED: u8 = 3;
    /// `EXT2_HASH_HALF_MD4_UNSIGNED`
    pub const HALF_MD4_UNSIGNED: u8 = 4;
    /// `EXT2_HASH_TEA_UNSIGNED`
    pub const TEA_UNSIGNED: u8 = 5;
}

/// The hash of a name, as `(hash, minor_hash)`.
///
/// The low bit of `hash` is always clear, which the tree then uses to mean
/// "this block continues a run of equal hashes from the block before it".
pub fn dirhash(version: u8, name: &[u8], seed: &[u32; 4]) -> (u32, u32) {
    // A seed of all zeros means "no seed", and the MD4 initial values stand.
    let mut buf = if seed.iter().any(|&w| w != 0) {
        *seed
    } else {
        [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476]
    };

    let signed = matches!(
        version,
        version::LEGACY | version::HALF_MD4 | version::TEA
    );

    let (hash, minor) = match version {
        version::LEGACY | version::LEGACY_UNSIGNED => (legacy(name, signed), 0),
        version::TEA | version::TEA_UNSIGNED => {
            let mut at = 0;
            while at < name.len() {
                let block = str2hashbuf(&name[at..], 4, signed);
                tea_transform(&mut buf, &block[..4]);
                at += 16;
            }
            (buf[0], buf[1])
        }
        // Half MD4 is what mke2fs has chosen by default for twenty years, and
        // is the case that matters.
        _ => {
            let mut at = 0;
            while at < name.len() {
                let block = str2hashbuf(&name[at..], 8, signed);
                half_md4_transform(&mut buf, &block);
                at += 32;
            }
            (buf[1], buf[2])
        }
    };

    (hash & !1, minor)
}

/// Pick the hash version to use, given what the superblock says.
///
/// `s_flags` records whether the filesystem was built somewhere `char` is
/// signed. Getting this wrong does not corrupt anything — it puts names in the
/// wrong leaf, so lookups miss and `e2fsck` reports the directory as damaged.
pub fn version_for(def_hash_version: u8, flags: u32) -> u8 {
    use crate::structs::superblock::flags as sb_flags;
    if flags & sb_flags::UNSIGNED_HASH != 0 && def_hash_version < 3 {
        def_hash_version + 3
    } else {
        def_hash_version
    }
}

/// The old hash, kept only for filesystems that already use it.
fn legacy(name: &[u8], signed: bool) -> u32 {
    let mut hash0: u32 = 0x12a3_fe2d;
    let mut hash1: u32 = 0x37ab_e8f9;
    for &byte in name {
        let c = char_value(byte, signed);
        let hash = hash1.wrapping_add(hash0 ^ c.wrapping_mul(7_152_373));
        let hash = if hash & 0x8000_0000 != 0 {
            hash.wrapping_sub(0x7fff_ffff)
        } else {
            hash
        };
        hash1 = hash0;
        hash0 = hash;
    }
    hash0 << 1
}

/// One byte of a name, read the way the filesystem's creator read it.
fn char_value(byte: u8, signed: bool) -> u32 {
    if signed {
        (byte as i8) as i32 as u32
    } else {
        byte as u32
    }
}

/// Pack up to `num` words of the name, padding with its length.
///
/// Mirrors `str2hashbuf`. The padding is the *unclamped* length repeated into
/// all four bytes, which is why it is computed before the clamp.
fn str2hashbuf(msg: &[u8], num: usize, signed: bool) -> [u32; 8] {
    let mut out = [0u32; 8];
    let mut at = 0;

    let pad = msg.len() as u32 | ((msg.len() as u32) << 8);
    let pad = pad | (pad << 16);
    let mut val = pad;

    let mut left = num as isize;
    let len = msg.len().min(num * 4);
    for (i, &byte) in msg[..len].iter().enumerate() {
        val = char_value(byte, signed).wrapping_add(val << 8);
        if i % 4 == 3 {
            out[at] = val;
            at += 1;
            val = pad;
            left -= 1;
        }
    }
    left -= 1;
    if left >= 0 {
        out[at] = val;
        at += 1;
    }
    loop {
        left -= 1;
        if left < 0 {
            break;
        }
        out[at] = pad;
        at += 1;
    }
    out
}

/// TEA in a Davies-Meyer construction, over four words.
fn tea_transform(buf: &mut [u32; 4], input: &[u32]) {
    const DELTA: u32 = 0x9e37_79b9;
    let mut sum: u32 = 0;
    let (mut b0, mut b1) = (buf[0], buf[1]);
    let (a, b, c, d) = (input[0], input[1], input[2], input[3]);

    for _ in 0..16 {
        sum = sum.wrapping_add(DELTA);
        b0 = b0.wrapping_add(
            ((b1 << 4).wrapping_add(a)) ^ b1.wrapping_add(sum) ^ ((b1 >> 5).wrapping_add(b)),
        );
        b1 = b1.wrapping_add(
            ((b0 << 4).wrapping_add(c)) ^ b0.wrapping_add(sum) ^ ((b0 >> 5).wrapping_add(d)),
        );
    }

    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

/// A cut-down MD4 that keeps only 32 bits of result.
fn half_md4_transform(buf: &mut [u32; 4], input: &[u32; 8]) {
    /// Selection.
    fn f(x: u32, y: u32, z: u32) -> u32 {
        z ^ (x & (y ^ z))
    }
    /// Majority.
    fn g(x: u32, y: u32, z: u32) -> u32 {
        (x & y).wrapping_add((x ^ y) & z)
    }
    /// Parity.
    fn h(x: u32, y: u32, z: u32) -> u32 {
        x ^ y ^ z
    }

    const K2: u32 = 0o13240474631;
    const K3: u32 = 0o15666365641;

    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);

    // Rotation is kept separate from the addition, as in the original, so the
    // intermediate does not have to be recomputed.
    macro_rules! round {
        ($fn:ident, $a:ident, $b:ident, $c:ident, $d:ident, $x:expr, $s:expr) => {
            $a = $a.wrapping_add($fn($b, $c, $d)).wrapping_add($x);
            $a = ($a << $s) | ($a >> (32 - $s));
        };
    }

    round!(f, a, b, c, d, input[0], 3);
    round!(f, d, a, b, c, input[1], 7);
    round!(f, c, d, a, b, input[2], 11);
    round!(f, b, c, d, a, input[3], 19);
    round!(f, a, b, c, d, input[4], 3);
    round!(f, d, a, b, c, input[5], 7);
    round!(f, c, d, a, b, input[6], 11);
    round!(f, b, c, d, a, input[7], 19);

    round!(g, a, b, c, d, input[1].wrapping_add(K2), 3);
    round!(g, d, a, b, c, input[3].wrapping_add(K2), 5);
    round!(g, c, d, a, b, input[5].wrapping_add(K2), 9);
    round!(g, b, c, d, a, input[7].wrapping_add(K2), 13);
    round!(g, a, b, c, d, input[0].wrapping_add(K2), 3);
    round!(g, d, a, b, c, input[2].wrapping_add(K2), 5);
    round!(g, c, d, a, b, input[4].wrapping_add(K2), 9);
    round!(g, b, c, d, a, input[6].wrapping_add(K2), 13);

    round!(h, a, b, c, d, input[3].wrapping_add(K3), 3);
    round!(h, d, a, b, c, input[7].wrapping_add(K3), 9);
    round!(h, c, d, a, b, input[2].wrapping_add(K3), 11);
    round!(h, b, c, d, a, input[6].wrapping_add(K3), 15);
    round!(h, a, b, c, d, input[1].wrapping_add(K3), 3);
    round!(h, d, a, b, c, input[5].wrapping_add(K3), 9);
    round!(h, c, d, a, b, input[0].wrapping_add(K3), 11);
    round!(h, b, c, d, a, input[4].wrapping_add(K3), 15);

    buf[0] = buf[0].wrapping_add(a);
    buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c);
    buf[3] = buf[3].wrapping_add(d);
}

/// How many index entries a block can hold, given where they start.
pub fn limit(block_size: usize, count_offset: usize, has_csum: bool) -> u16 {
    let tail = if has_csum { TAIL_LEN } else { 0 };
    ((block_size - count_offset - tail) / ENTRY_LEN) as u16
}

/// Build a root block: `.`, `..`, the root info, and an empty index.
///
/// `parent` is the directory's parent inode, since `..` is a real entry here
/// and has to keep working for anything reading the directory linearly.
pub fn build_root(
    block_size: usize,
    ino: u32,
    parent: u32,
    hash_version: u8,
    indirect_levels: u8,
    filetype: bool,
    has_csum: bool,
) -> Vec<u8> {
    let mut buf = vec![0u8; block_size];
    let ft = if filetype { dirent::file_type::DIR } else { 0 };

    // "." — a normal entry of the usual 12 bytes.
    put_u32(&mut buf, 0, ino);
    put_u16(&mut buf, 4, 12);
    buf[6] = 1;
    buf[7] = ft;
    buf[8] = b'.';

    // ".." — claims the whole rest of the block, hiding the index behind it.
    put_u32(&mut buf, 12, parent);
    put_u16(&mut buf, 16, (block_size - 12) as u16);
    buf[18] = 2;
    buf[19] = ft;
    buf[20] = b'.';
    buf[21] = b'.';

    // struct ext2_dx_root_info
    put_u32(&mut buf, ROOT_INFO_OFFSET, 0);
    buf[ROOT_INFO_OFFSET + 4] = hash_version;
    buf[ROOT_INFO_OFFSET + 5] = ROOT_INFO_LEN;
    buf[ROOT_INFO_OFFSET + 6] = indirect_levels;
    buf[ROOT_INFO_OFFSET + 7] = 0;

    put_u16(
        &mut buf,
        ROOT_COUNT_OFFSET,
        limit(block_size, ROOT_COUNT_OFFSET, has_csum),
    );
    put_u16(&mut buf, ROOT_COUNT_OFFSET + 2, 0);
    buf
}

/// Build an interior node block: one entry claiming everything, then an index.
pub fn build_node(block_size: usize, has_csum: bool) -> Vec<u8> {
    let mut buf = vec![0u8; block_size];
    put_u32(&mut buf, 0, 0);
    put_u16(&mut buf, 4, block_size as u16);
    put_u16(
        &mut buf,
        NODE_COUNT_OFFSET,
        limit(block_size, NODE_COUNT_OFFSET, has_csum),
    );
    put_u16(&mut buf, NODE_COUNT_OFFSET + 2, 0);
    buf
}

/// Where a block's index starts, or `None` if it holds no index.
///
/// This is the same test e2fsprogs makes: a block whose first entry covers all
/// of it is an interior node, and one whose first entry is exactly 12 bytes
/// followed by a `..` covering the rest is a root.
pub fn count_offset(block: &[u8], block_size: usize) -> Option<usize> {
    if block.len() < ROOT_COUNT_OFFSET + 4 {
        return None;
    }
    let rec_len = get_u16(block, 4) as usize;
    let name_len = block[6];

    if rec_len == block_size && name_len == 0 {
        return Some(NODE_COUNT_OFFSET);
    }
    if rec_len == 12 {
        let second = get_u16(block, 12 + 4) as usize;
        if second != block_size - 12 {
            return None;
        }
        // The root info's reserved word is zero and its length is 8; a real
        // directory entry landing here would not satisfy both.
        if get_u32(block, ROOT_INFO_OFFSET) != 0 || block[ROOT_INFO_OFFSET + 5] != ROOT_INFO_LEN {
            return None;
        }
        return Some(ROOT_COUNT_OFFSET);
    }
    None
}

/// How many levels of interior nodes a root block declares.
pub fn indirect_levels(block: &[u8]) -> u8 {
    block[ROOT_INFO_OFFSET + 6]
}

/// The hash version a root block declares.
pub fn root_hash_version(block: &[u8]) -> u8 {
    block[ROOT_INFO_OFFSET + 4]
}

/// How many index entries a block currently holds.
pub fn count(block: &[u8], count_offset: usize) -> u16 {
    get_u16(block, count_offset + 2)
}

/// Set how many index entries a block holds.
pub fn set_count(block: &mut [u8], count_offset: usize, count: u16) {
    put_u16(block, count_offset + 2, count);
}

/// Read one index entry as `(hash, block)`.
///
/// The first entry of a block carries no hash — everything below the next
/// entry's hash belongs to it — so its hash reads as zero.
pub fn entry(block: &[u8], count_offset: usize, index: usize) -> (u32, u32) {
    let at = count_offset + index * ENTRY_LEN;
    (get_u32(block, at), get_u32(block, at + 4))
}

/// Write one index entry.
pub fn set_entry(block: &mut [u8], count_offset: usize, index: usize, hash: u32, target: u32) {
    let at = count_offset + index * ENTRY_LEN;
    put_u32(block, at, hash);
    put_u32(block, at + 4, target);
}

/// Which index entry covers a hash.
///
/// Entries are in ascending hash order and the first one's hash is ignored, so
/// this is the last entry whose hash is not greater than the one wanted.
pub fn find(block: &[u8], count_offset: usize, hash: u32) -> Result<usize> {
    let count = count(block, count_offset) as usize;
    if count == 0 {
        return Err(Error::corrupt("htree", "index block holds no entries"));
    }
    let mut chosen = 0;
    for i in 1..count {
        let (at, _) = entry(block, count_offset, i);
        if at > hash {
            break;
        }
        chosen = i;
    }
    Ok(chosen)
}

/// Stamp an index block's checksum.
///
/// The checksum covers the header and the entries in use, then the tail's own
/// reserved word and four zero bytes standing in for the checksum field —
/// which is what makes the result independent of what was there before.
pub fn stamp_csum(
    block: &mut [u8],
    block_size: usize,
    seed: u32,
    inum: u32,
    generation: u32,
    count_offset: usize,
) {
    let limit = get_u16(block, count_offset) as usize;
    let tail_at = count_offset + limit * ENTRY_LEN;
    if tail_at + TAIL_LEN > block_size {
        return;
    }
    let used = count_offset + count(block, count_offset) as usize * ENTRY_LEN;

    let mut crc = crc32c(seed, &inum.to_le_bytes());
    crc = crc32c(crc, &generation.to_le_bytes());
    crc = crc32c(crc, &block[..used]);
    crc = crc32c(crc, &block[tail_at..tail_at + 4]);
    crc = crc32c(crc, &[0u8; 4]);

    put_u32(block, tail_at + 4, crc);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seed `debugfs -s 12345678-1234-1234-1234-123456789abc` means: the
    /// UUID's sixteen bytes, read as four native words.
    const SEED: [u32; 4] = [0x7856_3412, 0x3412_3412, 0x3412_3412, 0xbc9a_7856];

    /// Every value here came out of `debugfs -R "dx_hash ..."` from e2fsprogs
    /// 1.47.3, not from this implementation.
    ///
    /// A hash cannot be checked against itself. Nothing about a wrong hash is
    /// visible in the filesystem it builds — the structure is valid, the
    /// checksums are right, and every name is in a leaf. It is just the wrong
    /// leaf, so lookups miss and `e2fsck` calls the directory damaged. The
    /// only way to know is to ask the implementation everyone else uses.
    #[test]
    fn every_hash_matches_debugfs() {
        const LONG: &str = "a-name-long-enough-to-need-several-transform-blocks-to-hash-it";

        let cases: &[(u8, [u32; 4], &str, u32, u32)] = &[
            // The legacy hash takes no seed, so both columns agree.
            (version::LEGACY, [0; 4], "a", 0xe74b_53e2, 0),
            (version::LEGACY, [0; 4], "hello", 0x3225_2546, 0),
            (version::LEGACY, [0; 4], "passwd", 0x98b8_cd7c, 0),
            (version::LEGACY, [0; 4], "file-000042", 0x3623_8ac6, 0),
            (version::LEGACY, [0; 4], LONG, 0xe143_7aea, 0),
            (version::LEGACY, SEED, "hello", 0x3225_2546, 0),

            (version::HALF_MD4, [0; 4], "a", 0xd5fa_7d7a, 0xacb4_8187),
            (version::HALF_MD4, [0; 4], "hello", 0x1746_da32, 0x4200_13b5),
            (version::HALF_MD4, [0; 4], "passwd", 0xeadd_3d7e, 0xcbee_5b04),
            (version::HALF_MD4, [0; 4], "file-000042", 0x1c70_cbc8, 0x6df1_63d9),
            (version::HALF_MD4, [0; 4], LONG, 0x8bec_e040, 0x927b_dfe8),
            (version::HALF_MD4, SEED, "a", 0x09da_0480, 0x8fd8_b27e),
            (version::HALF_MD4, SEED, "hello", 0x06cc_2f6e, 0x1596_7186),
            (version::HALF_MD4, SEED, "passwd", 0x2ce8_6364, 0xbc63_7495),
            (version::HALF_MD4, SEED, "file-000042", 0x3d25_e014, 0xeae5_56ba),
            (version::HALF_MD4, SEED, LONG, 0x434f_388c, 0xb0ad_5187),

            (version::TEA, [0; 4], "a", 0x6d0e_a4c0, 0xc189_22df),
            (version::TEA, [0; 4], "hello", 0x6f5b_b1a8, 0x2319_17c2),
            (version::TEA, [0; 4], "passwd", 0x3ae7_fc66, 0x924c_c71a),
            (version::TEA, [0; 4], "file-000042", 0x3e46_73c6, 0x7904_0b73),
            (version::TEA, [0; 4], LONG, 0x0b20_ad52, 0x7d27_3c2b),
            (version::TEA, SEED, "a", 0x19de_2d0a, 0x641e_1d92),
            (version::TEA, SEED, "hello", 0x4498_1648, 0xaed7_d8c6),
            (version::TEA, SEED, "passwd", 0xbe3b_94e6, 0x69e0_1ce1),
            (version::TEA, SEED, "file-000042", 0xdcaf_b5bc, 0xe15b_0ae9),
            (version::TEA, SEED, LONG, 0x7fd9_05b4, 0xf720_63d5),
        ];

        for &(version, seed, name, want, want_minor) in cases {
            let (hash, minor) = dirhash(version, name.as_bytes(), &seed);
            assert_eq!(
                hash, want,
                "hash {version} of {name:?}: got {hash:#010x}, debugfs says {want:#010x}"
            );
            assert_eq!(
                minor, want_minor,
                "minor hash {version} of {name:?}: got {minor:#010x}, debugfs says {want_minor:#010x}"
            );
        }
    }

    #[test]
    fn signedness_changes_the_hash_of_a_high_byte() {
        let seed = [1u32, 2, 3, 4];
        let name = b"caf\xe9";
        let (signed, _) = dirhash(version::HALF_MD4, name, &seed);
        let (unsigned, _) = dirhash(version::HALF_MD4_UNSIGNED, name, &seed);
        assert_ne!(
            signed, unsigned,
            "a byte above 127 must hash differently under the two conventions"
        );
        // Plain ASCII cannot tell the two apart, which is why this went
        // unnoticed on x86 for years.
        let (a, _) = dirhash(version::HALF_MD4, b"plain", &seed);
        let (b, _) = dirhash(version::HALF_MD4_UNSIGNED, b"plain", &seed);
        assert_eq!(a, b);
    }

    #[test]
    fn every_algorithm_clears_the_low_bit() {
        let seed = [9u32, 8, 7, 6];
        for version in [
            version::LEGACY,
            version::HALF_MD4,
            version::TEA,
            version::LEGACY_UNSIGNED,
            version::HALF_MD4_UNSIGNED,
            version::TEA_UNSIGNED,
        ] {
            for name in ["a", "bb", "a-longer-name-than-one-block-of-the-transform-takes"] {
                let (hash, _) = dirhash(version, name.as_bytes(), &seed);
                assert_eq!(hash & 1, 0, "{version} on {name:?}");
            }
        }
    }

    #[test]
    fn names_spread_across_the_hash_space() {
        // A hash that clustered would still pass every structural check and
        // would quietly make the tree useless, so this asserts it does not.
        let seed = [0x1234_5678, 0x9abc_def0, 0x0f1e_2d3c, 0x4b5a_6978];
        let mut buckets = [0usize; 16];
        for i in 0..4096 {
            let name = format!("file-{i:06}");
            let (hash, _) = dirhash(version::HALF_MD4, name.as_bytes(), &seed);
            buckets[(hash >> 28) as usize] += 1;
        }
        for (i, &n) in buckets.iter().enumerate() {
            assert!(
                (128..512).contains(&n),
                "bucket {i} holds {n} of 4096 names, which is not a spread"
            );
        }
    }

    #[test]
    fn a_root_block_reads_as_a_root() {
        let block = build_root(4096, 12, 2, version::HALF_MD4, 0, true, true);
        assert_eq!(count_offset(&block, 4096), Some(ROOT_COUNT_OFFSET));
        assert_eq!(indirect_levels(&block), 0);
        assert_eq!(root_hash_version(&block), version::HALF_MD4);
        // With a checksum tail, one entry's worth of room is given up.
        assert_eq!(get_u16(&block, ROOT_COUNT_OFFSET), (4096 - 32 - 8) / 8);
        assert_eq!(count(&block, ROOT_COUNT_OFFSET), 0);

        // And it is still an ordinary directory to a linear reader.
        let entries = dirent::parse_block(&block).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, b".");
        assert_eq!(entries[1].name, b"..");
        assert_eq!(entries[1].inode, 2);
    }

    #[test]
    fn an_interior_node_hides_from_a_linear_reader() {
        let block = build_node(1024, false);
        assert_eq!(count_offset(&block, 1024), Some(NODE_COUNT_OFFSET));
        assert_eq!(get_u16(&block, NODE_COUNT_OFFSET), (1024 - 8) / 8);

        // One entry with inode 0 covering the block, so nothing is a name.
        let entries = dirent::parse_block(&block).unwrap();
        assert!(entries.iter().all(|e| e.inode == 0));
    }

    #[test]
    fn an_ordinary_directory_block_holds_no_index() {
        let entries = vec![
            dirent::DirEntry::new(2, b".", dirent::file_type::DIR).unwrap(),
            dirent::DirEntry::new(2, b"..", dirent::file_type::DIR).unwrap(),
            dirent::DirEntry::new(12, b"hostname", dirent::file_type::REG_FILE).unwrap(),
        ];
        let block = dirent::build_block(&entries, 1024, false).unwrap();
        assert_eq!(count_offset(&block, 1024), None);
    }

    #[test]
    fn find_picks_the_last_entry_at_or_below_the_hash() {
        let mut block = build_node(1024, false);
        // The first entry's hash is never read: everything below the second
        // entry's hash belongs to it.
        for (i, (hash, target)) in [(0u32, 10u32), (0x4000, 11), (0x8000, 12)]
            .into_iter()
            .enumerate()
        {
            set_entry(&mut block, NODE_COUNT_OFFSET, i, hash, target);
        }
        set_count(&mut block, NODE_COUNT_OFFSET, 3);

        assert_eq!(find(&block, NODE_COUNT_OFFSET, 0).unwrap(), 0);
        assert_eq!(find(&block, NODE_COUNT_OFFSET, 0x3fff).unwrap(), 0);
        assert_eq!(find(&block, NODE_COUNT_OFFSET, 0x4000).unwrap(), 1);
        assert_eq!(find(&block, NODE_COUNT_OFFSET, 0x7fff).unwrap(), 1);
        assert_eq!(find(&block, NODE_COUNT_OFFSET, 0x8000).unwrap(), 2);
        assert_eq!(find(&block, NODE_COUNT_OFFSET, u32::MAX).unwrap(), 2);
    }
}
