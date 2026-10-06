# CLAUDE.md — mkfs-ext4

Async, parallel ext2/ext3/ext4 formatter and checker in pure Rust. Reimplements
`mke2fs` and `e2fsck` from the ext4 specification, held to real `mke2fs`
output by a comparison tool, with the e2fsprogs source consulted at the
specific points where the two differ.

- **Crate:** `mkfs-ext4` (lib `mkfs_ext4`)
- **Version:** 4.1.0 — see `Cargo.toml` (single version location)
- **License:** MIT OR Apache-2.0
- **Repo:** https://github.com/glennswest/mkfs.ext4.rs
- **Directory:** `~/src/mkfs.ext4.rs` (the stormcentral session checkout). The
  crate covers ext2/ext3/ext4 from one code path, exactly as `mke2fs` does.
- **Ships as:** a git dependency pinned to a release tag. It is not on
  crates.io and has no container or golden. Consumers are stormblock (tag
  `v3.0.0`, `default-features = false, features = ["std"]`) and fio-ext4. The
  `cli` feature also builds the `mkfs-ext4` / `fsck-ext4` binaries.
- **Build/test:** `sc-build` (`cargo build && cargo test`). The golden,
  sector-size, journal-floor and strict-sector suites in `tests/` need no
  privilege. `tests/verify-on-linux.sh` loop-mounts, so it needs root on its
  target host (`root@dev.g8.lo` by default). A session does not run it: see the
  root rule in `../CLAUDE.md`.

## Why this exists

`stormblock` has a formatter at `src/fs/ext4.rs` that produces filesystems the
Linux kernel mounts and writes, and that **RouterOS (lwext4) mounts but refuses
every write to** — stormblock#39. Three days went into adjusting that code
against the symptom. This crate does not extend it and does not start from it.

The rule here: **match what `mke2fs` actually writes, field for field.** Write
from the spec, diff the result against real `mke2fs` output, and for every
difference go to the source at that one point to find out why it is there. Where a value is a judgement call in our code and a
computed default in `mke2fs`, we compute it the way `mke2fs` does. A consumer
that disagrees with real `mke2fs` output is then a consumer bug with a reference
to point at, not an open-ended guess about feature flags.

## Design constraints

1. **Async, and parallel across devices.** `BlockDevice` takes `&self` for
   reads and writes, so one formatter run fans out across block groups and many
   formatter runs proceed concurrently. stormblock formats many volumes at once;
   the original was serial.
2. **Pure Rust, no C.** The engine consuming this is pure Rust by design.
3. **Device-agnostic.** The `BlockDevice` trait is the seam. stormblock plugs
   its thin volumes in directly — no file, no loopback, no round trip.
4. **Byte-exactness is testable.** Golden tests compare our output against
   recorded real `mke2fs` output for the same geometry.

## Work plan

- [x] Work the specification, with the e2fsprogs source (`ext2_fs.h`,
      `mke2fs.c`, `initialize.c`, `alloc_tables.c`, `csum.c`, `mkjournal.c`,
      `mke2fs.conf.in`) as the place to look up why a specific difference
      exists
- [x] Scaffold the crate
- [x] `device` — async `BlockDevice` trait, file / memory implementations
- [x] `structs` — superblock, group descriptors, inode, dirent, extents
- [x] `csum` — crc32c metadata_csum, crc16 legacy GDT, checksum seed
- [x] `layout` — mke2fs geometry: block/inode sizing, groups, flex_bg,
      sparse_super backups, reserved GDT blocks
- [x] `format` — parallel async formatter
- [x] `journal` — ext3/ext4 journal creation (JBD2 superblock, mke2fs sizing)
- [x] `params` — mke2fs profiles, size-class defaults, `-O` feature parsing
- [x] Golden references captured from real mke2fs; geometry and features
      asserted against all six
- [x] `tests/verify-on-linux.sh` — e2fsck, mount, write, unmount, e2fsck on a
      real kernel. All eight configurations pass.
- [x] `compare` — structural diff between two filesystems; zero structural
      difference from real mke2fs across all six golden references
- [x] `fsck` — check passes plus repair
- [x] CLI binaries `mkfs-ext4`, `fsck-ext4`
- [x] `../fio.ext4.rs` — async userspace read/write into the image, no kernel
- [x] `mmp` — multiple mount protection (`EXT4_FEATURE_INCOMPAT_MMP`, 0x100).
      Format-time: allocate `s_mmp_block`, write `mmp_struct` with
      `EXT4_MMP_SEQ_CLEAN`, honour `-O mmp` and `-E mmp_update_interval`.
      Open-time: the race-and-wait protocol — stamp a sequence, sleep
      `2 × mmp_check_interval`, re-read, refuse if it moved; then heartbeat
      while the device is held. This is the fence that stops two hosts from
      mounting one stormblock volume read-write and destroying it, and
      `mmp_nodename` makes the refusal name the holder instead of guessing.
- [x] `meta_bg` (past ~200 TiB) and `orphan_file` (last feature gap)
- [x] `dir_index` — the directory hash and htree format here, maintained in
      `fio-ext4`. Hash values asserted against `debugfs -R "dx_hash"`; trees
      checked by real `e2fsck` and walked by a real kernel.
- [x] `read` — synchronous `no_std` read path, so firmware can link the reader
      instead of a second implementation of the format drifting against this one
      (issue #2). `structs` was already sync; only `fs` and `device` were async.
      `BlockReader::read_at` fails with `ReadError`, the device's status word
      carried through to `Error::DeviceRead` (3.0.0).
- [x] `cache` — write-back block cache over `BlockDevice` (issue #4). sbregistry
      measured ~280x–1065x write amplification unpacking layers through
      fio-ext4 over NVMe/TCP: every few KiB of payload re-reads and re-writes
      the same bitmap, inode-table, extent-node, group-descriptor and
      superblock blocks, with the device as the only metadata cache. The fix at
      this seam: `CachedDevice<D>` wraps any `BlockDevice` with a bounded
      write-back LRU at block granularity — reads served from cache, writes
      absorbed as dirty blocks, batch eviction and `flush()` write back
      contiguous runs coalesced. Lazy durability between sync points is the
      consumer's stated contract (discard-and-rebuild on a torn build, `flush()`
      before seal). The O(1) tail-append allocation scan is fio-ext4's half —
      filed there, not fixed here.
- [x] Issue #5 (fio.ext4.rs#4 is the read side): a device that enforces its
      4096-byte logical block refuses the sub-block I/O this crate issues at
      aligned offsets — the 1024-byte superblock, individual inodes, the group
      descriptor table at its exact byte length. A loop device hid it with a
      kernel read-modify-write. Not fixed with a wrapper that does the same:
      `Geometry` already guarantees a block is never smaller than a sector and
      is whole sectors, so **every device operation is a whole filesystem
      block at a block boundary** and alignment follows. The formatter owns
      every block it writes and assembles full ones (block 0 is boot area plus
      superblock in one write, the descriptor table is padded to blocks as
      `mke2fs` writes it, reserved inodes go out grouped by inode-table
      block); `Filesystem` reads and writes the block an inode or the
      superblock lives in, the unit the kernel's buffer heads use. `open`
      alone consults the sector size, before the block size is known.
      `MemDevice::strict` is the test device that refuses unaligned I/O.
- [x] stormblock integration path: stormblock formats its templates through
      the `BlockDevice` seam and depends on `v3.0.0`. stormblock#39 is closed.
- [x] Issue #6 — **decided (owner, on #6): match `e2fsck`.** A clean
      filesystem is skipped unless `-f` / `FsckOptions::force`. Pass 0 runs first, then `e2fsck`'s `check_if_skip` reasons in its order
      (errors or pass-0 findings, not cleanly unmounted, backup superblock
      differs when repairing, mount count, last-check time in the future,
      check interval). Two added reasons in the safe direction: journal needs
      recovery, orphans pending (this checker replays and releases neither).
      A skip reports `clean, N/M files, A/B blocks` from the superblock, and when
      repairing it first updates the superblock's free counts from the
      descriptors. A full check that repairs sets `s_state`, `s_lastcheck` and
      `s_mnt_count` the way `e2fsck` does, or a filesystem due by mount count
      would stay due for ever. `FsckReport::scope` says which happened.
      `FsckReport` gains a field, so this is 4.0.0.
      **State 2026-09-27 (session restart):** code 53019ac, docs 5e6312f, both
      pushed. `sc-build` on 53019ac exited 0 (integration suites seen
      passing; the lib unit-test summary was cut off by `tail`). The
      `sc-build 'cargo test --lib fsck::'` re-run never started: dev.g8.lo
      dropped the connection, then refused SSH (2026-09-27, doc refresh
      527a10d). The `fsck::tests` (19) passed by name on bca2817 (#10's
      sc-build). **Released** in v4.0.0 (#14).
- [x] Issue #7 (P2): check and repair run without journal replay or orphan
      release. **Done, released in v4.1.0.** Option 1 of the issue (the owner's #6 rule:
      match `e2fsck`). Plan, in `e2fsck`'s order (`unix.c`, `journal.c`,
      `super.c`):
      1. `recovery` module: load the internal journal (inode 8), validate
         the JBD2 superblock, and replay as `jbd2_journal_recover` does —
         PASS_SCAN / PASS_REVOKE / PASS_REPLAY, descriptor tags in all four
         layouts (32/64-bit, csum v2, csum v3), escaped blocks, revoke
         records, wrap-around, commit/descriptor/data checksums. Then
         `s_start = 0`, `s_sequence = end + 1`, `s_errno` carried into
         `ERROR_FS`, the filesystem reloaded and `needs_recovery` cleared.
         Read-only (`-n`): skipped with `e2fsck`'s warning. Journal data with
         the flag clear: `PR_0_JOURNAL_RUN` (fixed with `-y`, halts `-p`).
         A journal this code cannot replay (external, fast_commit, unknown
         features, corrupt superblock): Serious, and nothing is written.
      2. Orphan release after pass 0, writing modes only (`release_orphan_
         inodes`): the `s_last_orphan` chain and `orphan_file` entries;
         links 0 → free blocks, xattr block ref, inode; links > 0 → truncate
         to `i_size` (extent trees and indirect maps). Bitmaps, descriptors
         and superblock counts updated as `ext2fs_*_alloc_stats2` does.
      3. Both are reported as notes (`e2fsck`'s messages, which it does not
         count as fixes: exit 0 alone), not as repairs. Non-breaking: 4.1.0.
      4. Tests: synthetic journals in every tag layout; and, where `debugfs`
         and `e2fsck` are on the host, a differential test — journal written
         by `debugfs jo/jw/jc`, replayed by real `e2fsck` and by us, images
         compared.
      **State 2026-10-06:** all four done (`src/recovery.rs`,
      `src/orphan.rs`, fsck wiring, `tests/journal_e2fsprogs.rs`). sc-build on
      89edf35: all five journal differential cases byte-identical to real
      `e2fsck -fy`; the orphan case differed only in backup descriptor
      blocks (e2fsck writes the primary only, `MASTER_SB_ONLY`) — fixed in
      783da0b with `Filesystem::flush_primary_group_descs`, plus two
      test-helper bugs. sc-build on 3bdfd42: 195 lib tests, every suite,
      the 6 differential cases, clippy `-D warnings` on both feature sets.
- [x] Issue #10 (P1): `format()` RSS grows ~8 KiB per group (18 GiB at
      256 TiB, 1 PiB > 32 GiB). Cause: `write_filesystem` builds every
      group's `GroupState` (a block-sized block bitmap and inode bitmap each)
      and the whole descriptor table before writing. Plan: stream it. Walk the
      descriptor table in bounded chunks of descriptor blocks, and build,
      write and drop each group's bitmaps inside the chunk, keeping only its
      encoded descriptor. Write each finished chunk to every descriptor copy
      (group 0 and the classic backups at `loc + chunk`, and each meta_bg
      block's own three groups). Memory is then bounded by concurrency × 2
      blocks plus one chunk. Byte-for-byte identical output: asserted by a
      test that formats with a 1-block chunk and with the default, plus the
      golden suites. fsck's share of the issue (same peak at 256 TiB) is
      filed on its own if it has the same cause.
      **Done** (bca2817, measured on 5754365): sc-build passed every suite and
      clippy `-D warnings`. `examples/formatscale.rs`, RSS less stored
      metadata: 20 MiB at 256 TiB, 71 MiB at 1 PiB. fsck's cause is
      different (a flat one-bit-per-block map), filed as #11 (P2). Next
      release: this is a perf fix plus a `Params::concurrency` fix, and it
      rides in 4.0.0 with #6.
- [x] Issue #9 (P1): the default inode count wraps at 256 TiB —
      `(bytes / ratio) as u32` is exactly 2^32, so 0, and the layout gets 8
      inodes per group. Fix as `mke2fs` does: past 2^32 − 1 requested inodes,
      cap at 2^32 − 1 with `64bit` and refuse without it ("raise inode
      ratio?"); then `initialize.c`'s `ipg_retry` — while `ipg × groups`
      overflows 32 bits, `ipg − 1` and round again. fsck's "not clean" is the
      formatter: inodes 1–11 (12 with orphan_file) are written to group 0, and
      at 8 per group inodes 9–11 belong to group 1 (group 0's free-inode count
      underflows). `Geometry::compute` refuses a layout whose group 0 cannot
      hold the reserved inodes. Tests at 256 TiB and 1 PiB.
      **Done** (77609b8, 16895a2, tests pinned to mke2fs in the next commit):
      inode counts equal real `mke2fs -n` 1.47.3 at 256, 300 and 1024 TiB;
      `formatscale 256 --check` formats and force-checks clean (0 problems).
      Rides in 4.0.0 with #6 and #10.
- [x] Issue #8 (P2): `fsck-ext4` takes only `-n -y -f -v`. `e2fsck`'s `-p`/`-a`,
      `-C` and the rest are clap usage errors that exit 2 ("corrected,
      reboot") instead of working or exiting 16. Against the owner's #6 rule
      that e2fsck scripts must work unchanged. **In progress.** Plan:
      clap errors exit 16 (`--help`/`-V` 0); `-p`/`-a` = new
      `FsckOptions::preen`: repair only what `e2fsck` flags `PR_PREEN_OK`
      (the link-count, bitmap and free-count repairs this checker makes),
      and on anything else write nothing, set `ERROR_FS` as `preenhalt`
      does and exit 4 with "UNEXPECTED INCONSISTENCY; RUN fsck MANUALLY";
      pass 4 decides before it writes so a halt leaves no partial repair.
      `-p`/`-a`/`-n`/`-y` conflicts exit 8 with e2fsck's message (owner,
      on #8). `-C fd` accepted and ignored, `-t` prints elapsed time.
      `-b -B -c -D -E -j -k -l -L -z` refused with exit 16. README lists both.
      **Done** (5790681): sc-build passed every suite and clippy
      `-D warnings`; `tests/fsck_cli.rs` runs the binary for each exit code,
      `fsck::tests::preening_*` cover the library. Rides in 4.0.0 with #6,
      #9 and #10 (`FsckOptions` gains `preen`, `FsckReport` `preen_halted`).
- [x] Issue #14 (P2): release #10's streaming format as a tag — consumers
      pinned to `v3.0.0` still held ~18 GiB at 256 TiB. `chore(release):
      v4.0.0` (with #6, #8, #9, #10), tagged after sc-build passed on the
      release commit (all suites, clippy `-D warnings`, `formatscale 256
      --check`: 20.6 MiB over stored, 0 problems). Tag pushed, #14 and #6
      closed; stormblock#300 / #289 told to move their pin.

## Features

| Feature | Default | What it brings |
|---|---|---|
| `std` | yes | the async formatter, checker, `Filesystem`, block cache and device layer |
| `cli` | yes | the `mkfs-ext4` / `fsck-ext4` binaries |
| *(neither)* | — | `structs`, `layout`, `csum`, `features`, `params`, `journal`, `bytes` and the synchronous `read` path: what a UEFI driver links |

`default-features = false` used to leave the crate whole. As of 2.0.0 it leaves
the `no_std` core, so a library consumer that wants the formatter asks for
`features = ["std"]` explicitly.

## Verified

`./tests/verify-on-linux.sh` builds images and puts them in front of a real
Linux kernel on dev.g8.lo (Fedora 43, e2fsprogs 1.47.3). As of the formatter
landing, all eight configurations pass every stage — ext2, ext3, ext4 with and
without a journal, 1 KiB and 4 KiB blocks, 16 MiB to 1 GiB:

    e2fsck -fn -> loop mount rw -> write -> mkdir -> 4 MiB write
      -> unmount -> e2fsck -fn

The second e2fsck is the one that counts. "Mounts read-write" and "is writable"
are different claims (stormblock#39), and only a completed write proves the
second.

## Conventions

- Every on-disk structure carries a comment naming the e2fsprogs struct and
  field it mirrors. Offsets are asserted in tests, not assumed.
- No `unsafe`. Structures are encoded field by field in little-endian, never by
  casting a repr(C) struct over a buffer.
- Nothing in this crate reads or writes a path outside the device it was given.
