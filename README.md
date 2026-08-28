# mkfs-ext4

Async, parallel **ext2 / ext3 / ext4** formatter and checker in pure Rust.

A from-scratch reimplementation of `mke2fs` and `e2fsck`, written from the ext4
on-disk specification and then held to real `mke2fs` output: a comparison tool
finds every difference between the two filesystems, and the
[e2fsprogs](https://github.com/tytso/e2fsprogs) source is consulted at those
specific points to establish *why* the difference exists.

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

## Sector size

The block size is never smaller than the device's logical sector, exactly as
`mke2fs` does it — so the same 256 MiB filesystem is **1 KiB-block on a
512-byte-sector device and 4 KiB-block on a 4 KiB one**. Getting this wrong
produces a filesystem that cannot be written a block at a time.

`FileDevice` asks the kernel. If you implement `BlockDevice` over your own
storage, report it:

```rust
impl BlockDevice for MyVolume {
    fn logical_sector_size(&self) -> u32 { 4096 }
    // …
}
```

Or state it per-format, which overrides the device:

```rust
Params::new(Profile::Ext4).sector_size(4096)
```

Drives report one of two logical sector sizes, and both are covered. What we
choose, measured against `mke2fs` 1.47.3 on real loop devices of each:

| logical sector | 16 M | 64 M | 512 M | 1 G | 8 G | 64 G |
|---|---|---|---|---|---|---|
| **512** | 1024 | 1024 | 4096 | 4096 | 4096 | 4096 |
| **4096** | 4096 | 4096 | 4096 | 4096 | 4096 | 4096 |

Identical in every cell to what `mke2fs` chooses. The bottom-left corner is the
one that matters: on a 4 KiB-sector drive a small volume gets 4 KiB blocks and
not the 1 KiB its size class would otherwise call for. A 512-byte sector is a
floor, not a block size — every block size from 1024 up works on such a device,
and all of them pass `e2fsck`, a kernel mount, a write and a second `e2fsck`.

Note that a 512-*byte block* is not a thing ext2/3/4 can express:
`s_log_block_size` is an exponent above 1024, so 1024 is the format's floor.
`mke2fs -b 512` refuses for the same reason.

## Write amplification and the block cache

A write-heavy consumer — `fio-ext4` streaming a file into a volume — re-reads
and re-writes the same metadata blocks (bitmap, inode table, extent nodes,
group descriptor, superblock) for every few KiB of payload. Measured over
NVMe/TCP that reached ~1065x write amplification: ~55.9 GB of device writes to
place a 55 MB file (#4), with the device itself never the limiter.

`CachedDevice` is the fix at the `BlockDevice` seam — a bounded write-back
block cache any consumer can wrap around its device:

```rust
use mkfs_ext4::{CachedDevice, BlockDevice};

let dev = CachedDevice::new(my_volume)   // 4 KiB blocks, 32 MiB by default
    .with_block_size(4096)
    .with_capacity(64 << 20);
// … reads hit the cache, writes become dirty blocks …
dev.flush().await?;                      // write-back, coalesced, then inner flush
```

Hot metadata settles to one read on first touch and one write per `flush()`;
streamed data reaches the device as large coalesced writes. It is write-back:
between flushes the device does not have the dirty blocks, so wrap only a
device whose consumer treats a torn build as discard-and-rebuild and flushes
at its sync points. `stats()` reports what actually reached the device.

## Why

Two properties the C tools cannot offer a Rust storage engine:

- **Async and parallel.** `BlockDevice` takes `&self`, so a single format fans
  out across block groups, and many formats run concurrently. A storage engine
  provisioning volumes formats them all at once, not one after another.
- **No device round trip.** The `BlockDevice` trait is the seam. A consumer
  formats its own in-memory or network-backed volume directly — no loopback,
  no `/dev` node, no shelling out to `mkfs.ext4`.

It is also *correct by reference*. The spec gives the shape; the differences a
comparison against real `mke2fs` output turns up are what give the values. Each
one is chased to a reason rather than adjusted until it disappears — which is
the difference between matching and merely resembling.

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

`MIT OR Apache-2.0`, at your option — the Rust ecosystem's usual pair. The MIT
arm is GPLv2-compatible, so this imposes nothing on a kernel or RHEL consumer.
