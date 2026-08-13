# Changelog

## [Unreleased]

## [v1.1.0] — 2026-08-13

### Added
- **Extended attribute codecs** — `structs::xattr`, covering both places
  ext4 keeps them: in the spare room after the inode's fixed fields, and in a
  block of their own when they do not fit. Handles the name-prefix indices
  (`user.`, `security.`, `system.`, `trusted.`), the entry hash that block
  storage requires, and the block's checksum. This is what lets a filesystem
  carry SELinux labels and POSIX ACLs, and so what lets a container image or
  an unpacked tarball work rather than merely exist.

## [v1.0.2] — 2026-08-12

### Fixed

- **`FileDevice` now asks the kernel for the device's logical sector size**
  instead of assuming 512. v1.0.1 added the sector-size *mechanism* but no
  detection, so on a real 4 KiB-sector device it still produced a 1 KiB-block
  filesystem unless the caller passed `--sector-size` — which is not a fix.
  Measured on a 4 KiB loop device: ours now chooses 4096 with no flag, matching
  `mke2fs`; on a 512-byte device both still choose 1024.

### Added

- Three ways to state a sector size, for storage that cannot be probed:
  override `BlockDevice::logical_sector_size()`, pass `Params::sector_size`,
  or use `--sector-size`. `Params` wins over the device. Documented in the
  crate docs and README, since a consumer implementing `BlockDevice` over its
  own volumes is exactly who needs it.

## [v1.0.1] — 2026-08-12

### Changed

- **Licence is now `MIT OR Apache-2.0`**, the Rust ecosystem's usual pair,
  replacing GPL-2.0-or-later. The MIT arm is GPLv2-compatible, so nothing here
  constrains a kernel or RHEL consumer.

## [v1.0.0] — 2026-08-12

First stable release. The public API is settled and the full semver contract
applies from here: a breaking change needs a major bump.

**What 1.0 claims.** The formatter produces filesystems with *zero structural
differences* from real `mke2fs` 1.47.3 across seven golden references — ext2,
ext3 and ext4, at 1 KiB and 4 KiB blocks, with and without a journal, on
512-byte and 4 KiB-sector devices. Every configuration is mounted and written
by a real Linux kernel before release. Feature parity with `mke2fs` is exact:
the known-gaps list in the parity test is empty.

**What 1.0 does not claim.** Feature completeness. `meta_bg` is written but only
reachable past ~200 TiB; triple indirection is read but not written; journal
*replay* is not implemented, only journal creation. None of those require an
API change to add.

### Added

- **Async, parallel formatter.** `BlockDevice` takes `&self` for reads *and*
  writes, so one format fans out across block groups and many formats run at
  once. File, raw-device and in-memory implementations; a consumer can plug in
  storage that is not a block device at all.
- **ext2, ext3 and ext4** from one code path, with `mke2fs.conf` size-class
  defaults, profiles and `-O` feature parsing.
- **Byte-exact on-disk structures** — superblock, group descriptors, inode,
  directory entries, extents, JBD2 — with offsets asserted against `ext2_fs.h`
  rather than assumed. No `unsafe` in the library.
- **Geometry** matching `mke2fs`: block and inode sizing, `sparse_super`
  backups, reserved GDT blocks, `flex_bg` table placement allocated around
  superblock backups, and `meta_bg` distribution past ~200 TiB. Group placement
  is computed, never materialised — 64 TiB yields 524 288 groups and any one of
  them is O(1) to place.
- **Journal** creation: JBD2 superblock, `mke2fs` sizing, fragmented allocation
  where no contiguous run exists, and the `s_jnl_blocks` inode backup.
- **`orphan_file`**, `resize_inode`, `64bit`, `huge_file`, `dir_nlink`,
  `extra_isize`, `metadata_csum` and `metadata_csum_seed`.
- **`fsck`** — the six e2fsck passes, check and repair. Checking never writes;
  repair writes only what a pass proved wrong and records every change.
  `e2fsck`-compatible exit codes.
- **`compare`** — a field-by-field structural diff between two filesystems,
  classing each difference as identity, incidental or structural so a real
  divergence is not lost among the UUIDs and timestamps that always differ.
- **`mmp`** — multiple mount protection: the race-and-wait fence that stops two
  hosts mounting one shared volume read-write. A refusal names the holder.
- **`mkfs-ext4` and `fsck-ext4` binaries** with `mke2fs`- and `e2fsck`-compatible
  flags and exit codes. Install them as `mkfs.ext4` and `fsck.ext4`.

### Fixed

Every one of these was found by diffing against real `mke2fs`, by `e2fsck`, or
by a kernel mount — not by inspection:

- **crc32c convention.** `ext2fs_crc32c_le` is the bare table update with no
  complement at either end; the `crc32c` crate's `crc32c_append` complements on
  both. Every metadata checksum was wrong and e2fsck rejected the superblock.
- **Journal checksums** are not written at format time, even with
  `metadata_csum`. Setting `csum_v3` produced "Journal superblock is corrupt".
- **Reserved GDT blocks** are the resize inode's indirect blocks, listing their
  own backup locations — not padding to be zeroed.
- **`flex_bg` placement** allocates around superblock backups rather than laying
  metadata out as one contiguous run, which overlapped group 1's backup at
  256 MiB.
- **`meta_bg`**: a group can hold a descriptor block copy and no superblock
  backup, so the placement guard must not test `has_super` first.
- **`BLOCK_UNINIT`, `INODE_ZEROED` and `s_lpf_ino`** now match what `mke2fs`
  writes.
- **Journal placement** follows `get_midpoint_journal_block()` — the emptiest
  group beside the midpoint, not the midpoint — and a block-mapped journal
  interleaves its indirect blocks with its data.
- **The device's logical sector size sets the floor for the block size.** The
  same 256 MiB filesystem gets 1 KiB blocks on a 512-byte-sector device and
  4 KiB blocks on a 4 KiB one. A filesystem whose blocks are smaller than the
  device's sector cannot be written a block at a time.
- **Special files**: an inode's `i_block` is not always a block map. For a
  device it holds the device number and for a fast symlink the target path;
  walking those as block pointers reads a major/minor pair as a physical block.

### Verified

- `tests/golden_compare.rs` — zero structural differences from real `mke2fs`
  across seven reference filesystems, diffing the images themselves rather than
  their `dumpe2fs` text.
- `tests/verify-on-linux.sh` — eleven configurations put through
  `e2fsck -fn` → loop mount → write → unmount → `e2fsck -fn` on Fedora 43,
  Linux 6.17; plus five named corruptions repaired by our checker and accepted
  by e2fsprogs.
- A 1 TiB filesystem, where `meta_bg` triggers on its own, formats in 7 seconds
  using 353 MiB of actual allocation, mounts showing 957 GiB free, and checks
  clean.
- The kernel takes our MMP fence: the sequence moves off `SEQ_CLEAN` and it sets
  its own check interval.

### Development history

The entries below record how the above was reached, newest last.

### 2026-08-12
- **docs:** Add `mmp` (multiple mount protection) to the work plan. On shared
  block storage — which is exactly what stormblock exports — two hosts can be
  handed the same device, and ext4 has no way to notice. MMP is the only
  multi-host primitive the on-disk format has, and it exists to prevent
  concurrent read-write mounts rather than to arbitrate them.
- **feat:** Scaffold the `mkfs-ext4` crate — async, parallel ext2/ext3/ext4
  formatter and checker, written from the e2fsprogs source as reference rather
  than derived from any existing in-tree formatter.
- **docs:** Record the work plan, licensing posture (GPL-2.0-or-later, matching
  e2fsprogs) and the reason this is a from-scratch build in `CLAUDE.md`.
- **test:** Vendor six deterministic golden filesystems generated by real
  `mke2fs` 1.47.3 on dev.g8.lo (ext2/ext3/ext4, 1 KiB and 4 KiB blocks, with
  and without a journal), plus their `dumpe2fs` output. Pinning `-U`,
  `-E hash_seed` and `SOURCE_DATE_EPOCH` makes `mke2fs` byte-reproducible,
  which turns "match the reference" into an exact test rather than a judgement.
- **feat:** Add `params` (mke2fs profiles, size-class defaults, feature
  resolution) and `layout` (block/inode geometry, sparse_super backups,
  reserved GDT blocks, flex_bg table placement). Group placement is computed on
  demand rather than materialised, so a 64 TiB filesystem with 524 288 groups
  costs nothing to describe.
- **test:** Geometry and feature masks are asserted against all six golden
  `mke2fs` references, plus every size class from floppy to 128 TiB, the 16 TiB
  `64bit` boundary and the `meta_bg` limit.
- **feat:** Add `journal` (JBD2 superblock, mke2fs journal sizing) and `format`
  (the parallel async formatter). ext2, ext3 and ext4 images now pass real
  `e2fsck -fn`, mount read-write under a Linux 6.17 kernel, accept writes, and
  stay clean after unmount — verified for 16 MiB to 1 GiB at 1 KiB and 4 KiB
  blocks by `tests/verify-on-linux.sh`.
- **fix:** `csum::crc32c` now matches `ext2fs_crc32c_le`, which is the bare
  table update with no complement at either end. The `crc32c` crate's
  `crc32c_append` complements on entry and exit, so every metadata checksum was
  wrong and e2fsck rejected the superblock outright.
- **fix:** Write no journal features and no journal checksum at format time.
  `mke2fs` leaves "Journal features: (none)" even on a `metadata_csum`
  filesystem; setting `csum_v3` made e2fsck report "Journal superblock is
  corrupt".
- **fix:** Reserved GDT blocks are the resize inode's indirect blocks, each
  listing where its own backup copies live — not padding to be zeroed.
  Zeroing them left e2fsck with "Resize inode not valid".
- **fix:** Place flex_bg bitmaps and inode tables by allocation rather than
  arithmetic, stepping over superblock backups and descriptor tables in the
  way. A contiguous run overlapped group 1's backup superblock on a 256 MiB
  filesystem and e2fsck rejected the descriptors.
- **feat:** Record `s_jnl_blocks` and `s_jnl_backup_type`, the journal inode
  backup `mke2fs` writes ("Journal backup: inode blocks").
- **perf:** Build block bitmaps by marking ranges instead of querying placement
  once per block. The unit suite went from 7.7s back to 0.05s; the per-block
  form would have dominated the cost of formatting anything large.
- **feat:** Add `fs` — an opened filesystem: superblock and group descriptor
  decoding, inode lookup, logical-to-physical resolution through extent trees
  and indirect blocks, directory walking, path resolution, and opening from a
  superblock backup. Shared by `fsck` and the forthcoming `fio.ext4.rs`.
- **feat:** Add `fsck` — the six e2fsck passes, check and repair. Detects and
  corrects wrong free counts, wrong bitmaps, wrong link counts, bad checksums,
  duplicate block claims, malformed directories and disconnected trees.
  Checking never writes; repair records every change and re-checks clean.
- **fix:** Allocate the journal in as many runs as the free space allows. A
  filesystem without `flex_bg` opens every group with its own metadata, so the
  longest contiguous run is shorter than a group — an 8192-block journal on a
  256 MiB ext3 filesystem had nowhere to go and the format failed outright.
  Extent-mapped journals gain a leaf block when more than four extents are
  needed.
- **feat:** `BlockDevice` is now implemented for `&D`, so a device can be
  formatted and then checked without giving up ownership.
- **feat:** Add the `mkfs-ext4` and `fsck-ext4` binaries, with `mke2fs`- and
  `e2fsck`-compatible flags and exit codes. Install them as `mkfs.ext4` and
  `fsck.ext4` — Rust will not accept a `.` in a crate name, and those are the
  names `mkfs -t ext4` and `fsck -t ext4` dispatch on.
- **test:** `verify-on-linux.sh` now judges our repairs by real `e2fsck`. Five
  named corruptions are introduced, repaired with our fsck, and handed to
  e2fsprogs for a verdict; all five are accepted. A checker whose repairs the
  reference implementation rejects is worse than no checker.
- **fix:** A plain `cargo build` failed: the default feature declared two
  binaries that did not exist.
- **feat:** Implement `meta_bg`. Past the point where a contiguous descriptor
  table would take three quarters of a block group, the table is distributed:
  one descriptor block per meta block group, kept in that group's first, second
  and last group, and the resize inode is dropped. Geometry turns it on the way
  `mke2fs` does, and `-O meta_bg` forces it at any size.
  Verified at scale: a 1 TiB filesystem formats in 7 seconds, occupies 353 MiB
  on a sparse file, passes `e2fsck -fn`, mounts, takes writes, and checks clean
  again.
- **fix:** A group can hold a descriptor block copy and no superblock backup —
  under meta_bg the last group of a meta block group does exactly that. Both
  the bitmap builder and `in_super_region` tested `has_super` first and so left
  that block marked free; e2fsck reported thousands of block bitmap
  differences. `super_overhead` already accounts for both, so the guard was
  simply wrong.
- **feat:** Implement `orphan_file`, closing the last feature gap against real
  `mke2fs`. The ext4 profile now creates an orphan-tracking file at inode 12,
  sized as `ext2fs_default_orphan_file_blocks()` computes, each block carrying
  the `EXT4_ORPHAN_BLOCK_MAGIC` tail and a checksum bound to the inode, its
  generation and the block number. Silently dropped when there is no journal,
  as `mke2fs` does — the orphan list exists for a journal to replay.
  `tests/golden_geometry.rs` no longer needs a known-gaps list.
- **fix:** fsck no longer reports the orphan file as an unreferenced inode. It
  sits outside the directory tree by design, reachable only through
  `s_orphan_file_inum`, so having no name is its normal state.
- **feat:** Add `compare` — a structural diff between two filesystems, field by
  field, with differences classed as identity, incidental or structural so a
  real divergence is not lost among expected ones. Features are named rather
  than printed as hexadecimal masks.
- **test:** `tests/golden_compare.rs` diffs our output against the golden
  `mke2fs` images themselves, not just their `dumpe2fs` text. **Zero structural
  differences** across all six references.
- **fix:** Four divergences from `mke2fs` that the compare module found on its
  first run:
  - `s_lpf_ino` is left zero; `mke2fs` never sets it.
  - `INODE_ZEROED` is set whenever the inode table was written, checksums or
    not — which is why a plain ext2 filesystem carries it and nothing else.
  - `BLOCK_UNINIT` is set on checksummed filesystems for any group with no
    blocks in use, except the last, whose bitmap carries padding.
  - The journal goes where `get_midpoint_journal_block()` puts it: the emptiest
    of the groups either side of the midpoint, not the midpoint itself. On a
    64 MiB ext4 filesystem that is block 16385, not 30720.
- **fix:** A block-mapped journal is allocated block by block with its indirect
  blocks interleaved, as `mke2fs` does, rather than all data first and the
  indirect blocks after. Same file, and each indirect block now sits beside the
  data it describes.
- **feat:** Implement `mmp` — multiple mount protection. Shared block storage
  can hand one device to two hosts and ext4 has no other way to notice; MMP is
  the only multi-host primitive the format has, and it fences rather than
  arbitrates. Format-time allocates `s_mmp_block` and writes the structure
  clean; `-O mmp` and `--mmp-update-interval` control it. Open-time implements
  the race-and-wait of `ext2fs_mmp_start()`: stamp a sequence, wait out two
  check intervals, refuse if it moved. A refusal names the holder from
  `mmp_nodename` rather than leaving an operator to guess.
  A dead holder can be taken over; a running check and an unknown sequence are
  refused distinctly.
  Verified with a real kernel: it mounts our MMP filesystem and **takes the
  fence** — the sequence moves from `SEQ_CLEAN` to its own and it sets its own
  check interval.
- **fix:** fsck now claims the MMP block. It belongs to no inode and no group's
  metadata, so nothing else would ever account for it.
- **feat:** `Inode` learned about special files: `has_block_map()` tells a
  caller when `i_block` holds something other than block pointers, and
  `device_numbers()` / `set_device_numbers()` encode a device the way the
  kernel reads it — the classic 16-bit slot for small numbers, the wider
  encoding beyond. Without this a checker walks a device's major and minor
  number as if it were a physical block.
- **fix:** The device's logical sector size now sets the floor for the block
  size, as `mke2fs` does. This changes the answer: the same 256 MiB filesystem
  gets **1 KiB blocks on a 512-byte-sector device and 4 KiB blocks on a 4 KiB
  one**, because the size-class default is raised to the sector size. A
  filesystem with blocks smaller than the device's sector cannot be written a
  block at a time, and an implementation is entitled to refuse it — which makes
  this a candidate cause of stormblock#39, where a 256 MiB template was
  exported over a volume reporting 4096-byte sectors.
  `BlockDevice::logical_sector_size()` reports it, `Params::sector_size` and
  `--sector-size` override it, and an explicit block size below the sector is
  refused with a message rather than accepted.
- **test:** An eighth golden reference generated on a 4 KiB-sector loop device.
  Zero structural differences there too.
