# Changelog

## [Unreleased]

### 2026-09-02
- **docs(presentation):** the cloned voice sounded stuffy and flat. Measured
  the speaker references: `speaker.wav` carries 25 dB less relative energy
  above 3 kHz than `eref1.wav` — a muffled reference is cloned into every
  word. Switched `CLONE_REF` to `eref1.wav` and widened the expression dials
  (`EMO_GAIN` 2.2, `E_EXPAND` 0.15:1.0, `SM_TEMPO` 0.84:1.08).

### 2026-09-01
- **docs(presentation):** narration catches up with the 36-slide deck for the
  video re-render — three new scripts (the phantom journal, the write-amplification
  measurement, the release ledger), the tail renumbered 33→36, and the stats
  narration updated to twenty thousand lines / two hundred and twenty nine
  tests. `slidemaker.conf` bumped to `SLIDES=36`.

### 2026-08-31
- **docs:** README gains a Presentation section linking the PDF and HTML deck
  in `docs/presentation/`, and the copy in the presentations repo.
- **docs(presentation):** three "Since this talk" epilogue slides — the phantom
  journal (#3, fixed in v2.0.2), the 280× write-amplification measurement and
  `CachedDevice` (#4, v2.1.0), and a release ledger through mkfs-ext4 v2.1.0 /
  fio-ext4 v1.5.0. Stats refreshed to 20k lines, 229 tests, 24 releases. Slide
  counters now number each slide statically, so a printed page shows its own
  position instead of the slide the browser was on. First PDF render of the
  deck committed at `docs/presentation/ext4-rust.pdf` (36 pages, headless
  Chrome, the deck's own `@page` geometry). The slidemaker production tree
  (narration, frames, voice) still reflects the 33-slide deck the video was
  produced from; a re-render needs narration for the three new slides.

## [v2.1.0] — 2026-08-27

### Fixed
- **fix:** vendor the golden reference images the gitignore silently kept out.
  The blanket `*.img.*` rule caught `tests/golden/*.img.gz`, so only the
  `.dump` files were ever tracked and every fresh clone failed
  `golden_compare` with "No such file or directory" — the references existed
  only on the machine that recorded them.

### Added
- **feat:** `cache::CachedDevice` — a write-back block cache over any
  `BlockDevice` (#4). sbregistry measured ~280x write amplification placing a
  9.7 MB file and ~1065x placing a 55 MB one through fio-ext4 over NVMe/TCP,
  because every few KiB of payload re-read and re-wrote the same bitmap,
  inode-table, extent-node, group-descriptor and superblock blocks, with the
  device as the only metadata cache. The wrapper holds a bounded LRU of blocks:
  reads are served from cache with misses fetched as coalesced contiguous
  runs, writes are absorbed as dirty bytes costing no device I/O until
  eviction or `flush()` — sub-block valid/dirty tracking means even a partial
  write to an uncached block pays no read-modify-write read — and write-back
  coalesces device-contiguous dirty blocks into single writes. `CacheStats`
  counts what reaches the device.
  Write-back durability is by design lazy between sync points — the measured
  consumer discards torn builds and flushes before sealing; a consumer that
  needs every `write_at` durable must not wrap its device in this.

## [v2.0.4] — 2026-08-23

### Fixed
- **fix:** restore `Cargo.toml`, which v2.0.3 shipped empty. An in-place edit
  truncated the file before reading it, and the tag was pushed without a build
  in between. **Do not use v2.0.3** — its manifest does not parse, so cargo
  cannot resolve it at all; v2.0.4 is v2.0.3 with the manifest intact and
  nothing else changed. The tag is left in place rather than moved, because a
  published tag that something may already have fetched should not change
  meaning underneath it.

## [v2.0.3] — 2026-08-23

**Broken — do not use.** Shipped with an empty `Cargo.toml`; superseded by
v2.0.4, which is identical apart from the restored manifest.

### Added
- **test:** `fsck_reports_a_journal_the_superblock_only_claims_to_have` —
  v2.0.2 added the `journal-advertised-but-absent` check but nothing proved it
  fires. The test builds a 64 MiB filesystem that really does have a journal,
  takes the journal inode away while leaving `has_journal` set — re-encoding
  the superblock so its checksum stays valid and nothing but this check can
  notice — and requires `fsck` to name it at pass 0, `Serious`. A check with
  no test is the same position the defect was found in.

## [v2.0.2] — 2026-08-23

### Fixed
- **fix:** a filesystem below the journal size class's floor advertised a
  journal it did not have (#3). `has_journal` comes from the profile and the
  journal's *size* from `default_journal_blocks`, which returns nothing below
  2048 blocks — and the guard only ran one way: no feature meant no journal,
  but no journal did not clear the feature. The result was a superblock with
  `has_journal` set, zero journal blocks and `s_journal_inum` unset. `mke2fs`
  never emits that shape and a kernel will not mount it: it looks the journal
  inode up, finds nothing and refuses. It landed on filesystems of 1–7 MiB at
  4 KiB blocks, which is where config, secret and log volumes live and where
  it was least likely to be noticed. `Geometry::compute` now clears the
  feature once the block count is known, and re-runs `normalise` rather than
  clearing the dependent features by hand — so the orphan file goes with it,
  since replaying one is a journal's job. On a 1 MiB filesystem that orphan
  file was 128 KiB, an eighth of the whole thing.
- **fix:** `fsck` passed those filesystems. Every pass ran and found nothing,
  which is why the defect survived: a writer's output verified only by its own
  reader proves nothing. Pass 0 now reports `journal-advertised-but-absent`
  when the superblock sets `has_journal` and names no journal inode.

### Added
- **test:** `tests/journal_floor.rs` — every size from 1 to 7 MiB claims no
  journal, 8 MiB and up still get one, the orphan file follows the journal,
  and an explicitly sized `JournalSize::Blocks` is not second-guessed by the
  size class.

### Note
- **Output changes for filesystems under 8 MiB at 4 KiB blocks.** They are
  smaller and now mountable. Nothing above that floor changes.

## [v2.0.1] — 2026-08-20

### Fixed
- **fix:** `no_std` builds could not link at all: `crc32c` detects SSE4.2 at
  runtime and so requires `std`, and `f64::floor` is not in `core`. `crc32c` is
  now optional behind `std`, with a software `crc32c_sw` for firmware —
  checksumming a superblock and a few inodes, not gigabytes — and the reserved
  block count truncates by cast, which is the same value for a non-negative
  number and what `mke2fs` computes anyway.
- **fix:** The software crc32c initially omitted the inversion at both ends that
  `crc32c::crc32c_append` performs, so it produced different values — a
  filesystem written on a host would have disagreed with firmware about every
  checksum it carries. Caught by the agreement test rather than in the field.

### Added
- **test:** `csum::sw_tests` asserts the software and accelerated crc32c agree
  across five inputs and three seeds. This is the test that found the inversion
  bug above, and it is the reason to have written it.

## [v2.0.0] — 2026-08-19

### Breaking
- **BREAKING:** `std` is now a default feature. A consumer using
  `default-features = false` previously still got the whole crate; it now gets
  only the `no_std` core, and must ask for `features = ["std"]` to keep the
  formatter, checker and device layer. **Consumers using default features are
  unaffected** — `std` is on, every module is present, and the API is unchanged.

### Added
- **feat:** `read` — a synchronous, `no_std`, read-only path. `BlockReader` is
  the seam (`fn read_at(&self, offset, buf)`), and `Ext4` mounts a filesystem,
  resolves a path, walks an inode's extent tree and reads a file. No writing,
  no allocation policy, no journal replay.

  This exists because the consumer that needs an ext4 reader most cannot have a
  runtime: **a UEFI driver reads a kernel out of a filesystem before the kernel
  exists.** Writing a second reader for firmware would mean two hand-maintained
  implementations of one on-disk format, drifting, where the failure mode is
  *the node does not boot*. So the format has one definition and two ways to
  reach it — the async `fs` for hosts, `read` for firmware, both over the same
  `structs`.
- **feat:** `Error::DeviceRead { offset }` — what the `no_std` path returns when
  a read fails. `Error::Io` wraps `std::io::Error` and so is `std` only;
  firmware has no `io::Error` to wrap.
- **test:** Four round-trip tests read back filesystems *this crate formatted*,
  at 1 KiB and 4 KiB blocks — superblock geometry, the root directory's `.` and
  `..`, path resolution, and refusing a zeroed device. If the formatter and the
  reader ever disagree about the layout, that is where it surfaces.

### Changed
- `tokio`, `async-trait`, `futures`, `uuid` and `tracing` are now optional,
  enabled by `std`. `thiserror`, `crc32c` and `bitflags` build with
  `default-features = false`.
- `compare`, `device`, `format`, `fs`, `fsck` and `mmp` are gated on `std` —
  they are the async I/O layer. `bytes`, `csum`, `features`, `journal`,
  `layout`, `params` and `structs` were already synchronous and need nothing.
- `Superblock::uuid_string` is `std` only; it needs the `uuid` crate. A `no_std`
  consumer has `sb.uuid` as raw bytes, which is what firmware wants anyway.

## [v1.4.0] — 2026-08-18

### Added
- **feat:** `fsck` verifies the checksum on every extent-tree node that lives
  in a block of its own, and `Filesystem::bad_extent_checksums` exposes the
  same check. This is the check whose absence let v1.3.1's bug through: walking
  an extent tree reads the entries and never looks at the four bytes after
  them, so a tail written where no other reader looks passed our own `fsck`
  while `e2fsck` reported "extent block passes checks, but checksum does not
  match extent" and the kernel refused the file with EIO. A new test corrupts
  a leaf's tail, leaving every entry intact, and asserts the check reports it —
  a filesystem that only fails on a foreign reader is one we could not see
  before. Relates to #1 and fio.ext4.rs#2.

## [v1.3.1] — 2026-08-18

### Fixed
- **fix:** The journal's extent leaf writes its checksum at
  `EXT4_EXTENT_TAIL_OFFSET` — `sizeof(header) + sizeof(extent) * eh_max`, where
  the kernel and `e2fsck` read it and the last byte they checksum — instead of
  at the end of the block. The two coincide only when the room after the header
  divides into entries with exactly four bytes spare: true at 1 KiB and 4 KiB
  blocks, false at 2 KiB, 8 KiB and 32 KiB, where the checksum landed four
  bytes past where any other reader looks. On such a filesystem a journal
  needing more than the four extents an inode holds got a leaf no foreign
  reader would accept — reachable at 2 KiB, which `mke2fs` picks by size class.
  Our own reader derived the offset the same wrong way, so `fsck::check` stayed
  clean and only a real `e2fsck` objected. `extent::tail_offset` now names the
  offset once, and the journal-extent test runs at 1 KiB, 2 KiB and 4 KiB —
  without the fix only the 2 KiB case fails. Closes #1.

## [v1.3.0] — 2026-08-18

### Added
- **feat:** `Params::zeroed_medium` — say that the device already reads back as
  zeros, and the inode tables and journal body are not written, because they
  are already what they must be. The image is **byte-identical** either way,
  which a test asserts across ext2/3/4; the difference is only in how much was
  written to produce it. On a 1 TiB volume: 17.15 GiB and 4m 11s becomes
  151.8 MiB and 2s. True of a fresh sparse file, an untouched thin volume, or a
  device just discarded — and of nothing else. Also `--zeroed-medium` on the
  CLI.
- **test:** `tests/sector_size.rs` — the block size we choose for each
  combination of the two sector sizes real drives report (512 and 4096) and a
  range of capacities, asserted against values measured from `mke2fs` 1.47.3 on
  real loop devices. The row that matters is a small volume on 4 KiB sectors:
  its size class asks for 1024-byte blocks and the sector size overrules it,
  which is the case that produced a mountable, unwritable filesystem in
  stormblock#39. `verify-on-linux.sh` now re-derives the table from a real
  `mke2fs` on every run, so it cannot go stale unnoticed.

### Fixed
- **fix:** `examples/writemap` credited a group's bitmaps and inode table to
  whichever group the blocks physically sit in. With `flex_bg` they sit in the
  flex group's leader, so only the leader's were recognised and every other
  group's landed in the catch-all row: inode tables read 1,024 MiB when they
  were 16,384, and 62 MiB of bitmaps hid in "journal, root, lost+found,
  other". The map is now built from every group's own descriptors, and the
  leftover is two runs — the journal, and root plus `lost+found` in group 0.
  The totals were never wrong, so the measurement in the entry below stands.
- **fix:** `read_block_bitmap` and `read_inode_bitmap` returned the block on
  disk even for a group flagged `BLOCK_UNINIT` or `INODE_UNINIT`, where
  nothing was ever written to it — the flag says to compute the bitmap from
  the geometry, and the descriptor checksum vouches for the flag. Once the
  formatter stopped writing those bitmaps, our own `fsck` read them and
  reported every uninitialised group as differing from the inodes in use. On a
  fresh device the two agree by accident, because unwritten reads back as
  zeros and an all-free bitmap is zeros; `a_dirty_medium_formats_just_as_clean`
  formats onto a device filled with `0xff` first, so the difference between
  "wrote zeros" and "wrote nothing" is visible to a test at all.
- **fix:** a journal larger than the four extents an inode holds — anything
  above about 56 GiB — moved its extents to an extent tree block of their own,
  and that block was written with neither the checksum tail `metadata_csum`
  requires nor a `max` that left room for one. The filesystem was correct
  everywhere else and its journal could not be read at all: `e2fsck` reported
  "Superblock has an invalid journal (inode 8)" and refused to check further.
  Every fixture and every verification case stopped below 56 GiB, so nothing
  reached the path. Now checked from 56 GiB to 1 TiB, and covered by a test
  that asserts the leaf's checksum directly.
- **fix(docs):** the deck's table headers were invisible on dark slides. The
  file carries no doctype, so a browser opening it from disk renders it in
  quirks mode, where a table does not inherit colour from its ancestors — the
  header fell back to black on a black slide. Colour is now stated on `th` and
  `td` instead of inherited, and the copy in this repository is a complete
  document rather than a fragment. Every slide is checked for overflow at four
  aspect ratios.

### Changed
- **perf:** two writes that no reader reads and that `mke2fs` does not make.
  Reserved GDT blocks in a backup group are reserved space and nothing more —
  only group 0's are the resize inode's indirect blocks, listing every backup
  copy — so only group 0's are written. And a group whose descriptor carries
  `BLOCK_UNINIT` or `INODE_UNINIT` has no authoritative bitmap on disk: the
  flag says to compute it from the geometry, and the descriptor checksum
  vouches for the flag. The bitmaps are still built, because their checksums
  go in the descriptor either way; they are simply not written. On a 1 TiB
  ext4 with 8,192 groups: **17,563.7 MiB in 35,463 writes becomes 17,429.8 MiB
  in 19,547** — 133.9 MiB and 45% of the write calls, of which 72 MiB is the
  reserved GDT alone (80.0 MiB down to 8.0). Measured with
  `examples/writemap`, before and after, rather than reasoned about.

### Documentation
- **docs:** `examples/writemap.rs` — a device that stores nothing and records
  every write, so a 1 TiB format costs no disk and still says exactly which
  bytes it would have touched, classified block by block into what lives
  there. A single write can span a whole flex group's worth of bitmaps, and
  attributing all of it to the first block's kind is how a measurement lies to
  you, so it does not.
- **docs:** `examples/fsckgolden.rs` — runs our `fsck` over the recorded
  real-`mke2fs` images, so "our checker is happy with their filesystems" is a
  command rather than a claim.
- **docs:** `examples/replay.rs` — diffs a directory of raw images against the
  golden `mke2fs` references, so the deck's claim about what the compare tool
  first reported is a command that can be run rather than a recollection.
  Replayed against the formatter as it stood at `6beafd3`, immediately before
  those differences were fixed: 33 structural differences over the six
  references, in four kinds — `bg_flags` missing `INODE_ZEROED` (16), `bg_flags`
  missing `BLOCK_UNINIT` (8), `s_lpf_ino` recorded rather than left zero (6),
  and the resize inode's block map, which *is* the reserved GDT blocks (3).
- **docs:** removed a wrong figure from the deck. It claimed a 1 TiB filesystem
  formats in 7 seconds and allocates 353 MiB. Measured: **4m 11s and 17.15 GiB**
  by default, or 26s and 1.15 GiB with `lazy_itable_init`. Real `mke2fs` on the
  same geometry: under a second and 18 MiB. The gap is the inode table — 16 GiB
  of zeros we write and `mke2fs` does not, plus a 1 GiB journal body it also
  leaves unwritten. See the open issue on defaults.
- **docs:** the deck's subject is now the method rather than the filesystem —
  *Writing a Rust Library with AI*, with ext4 as the worked example. Four new
  slides: the five moves in order; what the assistant was good at and what it
  never once did; the phrasings that focused it, against the ones that did not;
  and how the method transfers to protocols, codecs and parsers — including
  what to do when there is no reference implementation to diff against.
- **docs:** corrected how this crate describes its own provenance, in the
  README, `CLAUDE.md` and the deck. It was written **from the ext4
  specification**, then held to real `mke2fs` output by the comparison tool,
  with the e2fsprogs source consulted at the specific fields where the two
  disagreed — to establish *why* a difference is there. Saying it was "derived
  from the e2fsprogs source" described neither the method nor the provenance
  accurately.
- **docs:** the deck now names the culprit instead of describing the mechanism
  around it. The analysis loop is drawn as a cartoon strip — six panels, the
  time between them, and the assistant's own confident wrong answers in full,
  including twice blaming MikroTik for a correctly-behaving implementation.
  Three new slides trace the actual fault: a hand-rolled formatter whose
  feature list was chosen by whether the symptom moved, and then, after it was
  replaced, a `logical_sector_size()` left to its 512-byte default by a thin
  volume that is not a real block device and had to report it.
- **docs:** `docs/presentation/thumbnail.html` and two rendered thumbnails.
- **docs:** `docs/presentation/ext4-rust.html` — a deck on how the two crates
  were built: the analysis loop that spent three days on the wrong code, the
  definition and the single rule that broke it, the features, and the seven
  kinds of verification with what each one caught that the others missed.

## [v1.2.0] — 2026-08-14

### Added
- **`structs::htree`** — the `dir_index` on-disk format: the directory hash
  (legacy, half-MD4 and TEA, in both the signed and unsigned conventions), the
  root and interior node layouts, and the index block checksum. Hash values are
  asserted against `debugfs -R "dx_hash ..."` from e2fsprogs 1.47.3 rather than
  against this implementation, because a wrong hash builds a filesystem that is
  structurally perfect and still broken.
- `Filesystem::lookup` walks a directory's hash index when it has one,
  answering in a two- or three-block walk instead of a read of the whole
  directory. Runs of names that share a hash are followed across leaves.
- `Superblock::has_dir_index` and `Superblock::has_filetype`.

### Fixed
- `htree::set_entry` no longer writes the first entry's hash. Those four bytes
  are the index's count and limit — `struct ext2_dx_countlimit` and the first
  `struct ext2_dx_entry` deliberately share an address. Writing a hash there set
  the limit to zero, which put the checksum tail on top of the first entry's
  block pointer: a tree that passed every structural check and pointed at
  nothing.

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
