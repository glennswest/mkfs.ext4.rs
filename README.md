# mkfs-ext4

Async, parallel **ext2 / ext3 / ext4** formatter and checker in pure Rust.

A from-scratch reimplementation of `mke2fs` and `e2fsck`, written against the
[e2fsprogs](https://github.com/tytso/e2fsprogs) source as the reference for
every on-disk field, default and geometry rule.

## Using it

Not on crates.io; take it by git, pinned to a tag so builds are reproducible:

```toml
[dependencies]
mkfs-ext4 = { git = "https://github.com/glennswest/mkfs.ext4.rs", tag = "v1.0.0" }
```

```rust
use mkfs_ext4::{format, FileDevice, Params, Profile};

let dev = FileDevice::open("/dev/sdb1").await?;
let report = format(&dev, &Params::new(Profile::Ext4).label("data")).await?;
println!("{} blocks, {} inodes", report.blocks_count, report.inodes_count);
```

## Why

Two properties the C tools cannot offer a Rust storage engine:

- **Async and parallel.** `BlockDevice` takes `&self`, so a single format fans
  out across block groups, and many formats run concurrently. A storage engine
  provisioning volumes formats them all at once, not one after another.
- **No device round trip.** The `BlockDevice` trait is the seam. A consumer
  formats its own in-memory or network-backed volume directly — no loopback,
  no `/dev` node, no shelling out to `mkfs.ext4`.

It is also *correct by reference*: defaults and layout come from what `mke2fs`
actually computes, not from a reading of the spec plus a guess.

## Status

The formatter works and is verified against a real kernel. `tests/verify-on-linux.sh`
builds images, ships them to a Linux host and runs each through
`e2fsck -fn` -> loop mount read-write -> write -> unmount -> `e2fsck -fn`.
All eight configurations pass: ext2, ext3 and ext4, with and without a journal,
at 1 KiB and 4 KiB blocks, from 16 MiB to 1 GiB.

Geometry and feature masks are asserted against golden filesystems produced by
real `mke2fs` 1.47.3, which is byte-reproducible once the UUID, hash seed and
`SOURCE_DATE_EPOCH` are pinned.

See `CLAUDE.md` for the work plan and what is still outstanding.

## Consumers

- [`fio-ext4`](https://github.com/glennswest/fio.ext4.rs) — reads and writes
  files inside the filesystems this crate creates, in userspace
- [`stormblock`](https://github.com/glennswest/stormblock) — filesystem
  templates ("mkfs once, clone forever")

## Licence

GPL-2.0-or-later, matching e2fsprogs, from which the on-disk format details and
defaults are derived.
