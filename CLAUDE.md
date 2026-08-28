# CLAUDE.md — mkfs-ext4

Async, parallel ext2/ext3/ext4 formatter and checker in pure Rust. Reimplements
`mke2fs` and `e2fsck` from the ext4 specification, held to real `mke2fs`
output by a comparison tool, with the e2fsprogs source consulted at the
specific points where the two differ.

- **Crate:** `mkfs-ext4` (lib `mkfs_ext4`)
- **Version:** 2.0.1 — see `Cargo.toml` (single version location)
- **License:** MIT OR Apache-2.0
- **Repo:** https://github.com/glennswest/mkfs.ext4.rs
- **Directory:** `~/projects/mkfs.ext4.rs`. The crate covers ext2/ext3/ext4
  from one code path, exactly as `mke2fs` does.

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
- [ ] `cache` — write-back block cache over `BlockDevice` (issue #4). sbregistry
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
- [ ] stormblock integration path (file against stormblock#39, do not edit it
      from this repo)

## Features

| Feature | Default | What it brings |
|---|---|---|
| `std` | yes | the async formatter, checker and device layer — everything that was here before |
| `cli` | yes | the `mkfs-ext4` / `fsck-ext4` binaries |
| *(neither)* | — | `structs`, `layout`, `csum`, `bytes` and the synchronous `read` path: what a UEFI driver links |

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
