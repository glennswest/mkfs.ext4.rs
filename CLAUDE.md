# CLAUDE.md — mkfs-ext4

Async, parallel ext2/ext3/ext4 formatter and checker in pure Rust. Reimplements
`mke2fs` and `e2fsck` from the e2fsprogs source as the reference.

- **Crate:** `mkfs-ext4` (lib `mkfs_ext4`)
- **Version:** 0.1.0 — see `Cargo.toml` (single version location)
- **License:** GPL-2.0-or-later
- **Directory note:** the repo directory is still `mkfs.ext3.rs` from the
  original ask. The crate covers ext2/ext3/ext4, exactly as `mke2fs` does, so
  the directory name is stale rather than wrong. Rename when convenient.

## Why this exists

`stormblock` has a formatter at `src/fs/ext4.rs` that produces filesystems the
Linux kernel mounts and writes, and that **RouterOS (lwext4) mounts but refuses
every write to** — stormblock#39. Three days went into adjusting that code
against the symptom. This crate does not extend it and does not start from it.

The rule here: **match what `mke2fs` actually writes, field for field, derived
from the e2fsprogs source.** Where a value is a judgement call in our code and a
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

- [x] Study the e2fsprogs source (`ext2_fs.h`, `mke2fs.c`, `initialize.c`,
      `alloc_tables.c`, `csum.c`, `mkjournal.c`, `mke2fs.conf.in`)
- [ ] Scaffold the crate
- [ ] `device` — async `BlockDevice` trait, file / memory implementations
- [ ] `structs` — superblock, group descriptors, inode, dirent, extents, JBD2
- [ ] `csum` — crc32c metadata_csum, crc16 legacy GDT, checksum seed
- [ ] `layout` — mke2fs geometry: block/inode sizing, groups, flex_bg,
      sparse_super backups, reserved GDT blocks
- [ ] `format` — parallel async formatter
- [ ] `journal` — ext3/ext4 journal creation
- [ ] `params` — mke2fs option and profile parity
- [ ] `fsck` — check passes plus repair
- [ ] CLI binaries `mkfs.ext4`, `fsck.ext4`
- [ ] Tests: round-trip, concurrent formats, golden comparison against mke2fs
- [ ] stormblock integration path (file against stormblock#39, do not edit it
      from this repo)

## Conventions

- Every on-disk structure carries a comment naming the e2fsprogs struct and
  field it mirrors. Offsets are asserted in tests, not assumed.
- No `unsafe`. Structures are encoded field by field in little-endian, never by
  casting a repr(C) struct over a buffer.
- Nothing in this crate reads or writes a path outside the device it was given.
