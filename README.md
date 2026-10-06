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
mkfs-ext4 = { git = "https://github.com/glennswest/mkfs.ext4.rs", tag = "v4.0.0", default-features = false, features = ["std"] }
```

| Feature | Default | What it brings |
|---|---|---|
| `std` | yes | the async formatter, checker, `Filesystem`, block cache and device layer |
| `cli` | yes | the `mkfs-ext4` / `fsck-ext4` binaries (implies `std`) |
| *(neither)* | — | the `no_std` core: `structs`, `layout`, `csum`, `features`, `params`, `journal`, `bytes` and the synchronous `read` path |

A library consumer turns `cli` off and asks for `std` explicitly, as above.
Since 2.0.0, `default-features = false` alone leaves only the `no_std` core.

```rust
use mkfs_ext4::{format, FileDevice, Params, Profile};

let dev = FileDevice::open("/dev/sdb1").await?;
let report = format(&dev, &Params::new(Profile::Ext4).label("data")).await?;
println!("{} blocks, {} inodes", report.blocks_count, report.inodes_count);
```

### Reading without a runtime

The `read` module is a synchronous, read-only path over the same on-disk
structures, and it builds with neither feature, for a UEFI driver loading a
kernel before any runtime exists. You implement `BlockReader::read_at`, and a
failed read returns `ReadError::new(status)` carrying the device's own status
word (an `EFI_STATUS` or an errno). That status comes back, together with the
offset, in `Error::DeviceRead`.

```rust
use mkfs_ext4::{BlockReader, Ext4};

let fs = Ext4::open(&dev)?;
let kernel = fs.read_file(&dev, "/vmlinuz")?;
```

### Command line

`cargo install --git https://github.com/glennswest/mkfs.ext4.rs --tag v4.0.0`
builds `mkfs-ext4` and `fsck-ext4`. Rust does not allow a `.` in a binary name,
so to have `mkfs -t ext4` / `fsck -t ext4` dispatch to them, install them as
`mkfs.ext4` and `fsck.ext4`.

- `mkfs-ext4 [options] DEVICE [BLOCKS]`: `-t ext2|ext3|ext4` (default
  `ext4`), `-b` block size, `--sector-size`, `-I` inode size, `-N` inode
  count, `-i` bytes per inode, `-m` reserved percent (default 5), `-L` label,
  `-U` UUID, `-O` features in `mke2fs -O` syntax (`^feature` clears a
  feature), `-g` blocks per group, `-G` flex_bg size, `-J` journal blocks (0
  means no journal), `--no-journal`, `--lazy-itable-init`, `--zeroed-medium`
  (the device already reads back as zeros, so the inode tables and journal
  body are not written), `--mkfs-time` for reproducible images,
  `--mmp-update-interval` (implies `-O mmp`), `-n` dry run and `-q` quiet.
- `fsck-ext4 [-n|-y|-p|-a] [-f] [-v] [-t] [-C fd] DEVICE`: `-n` (the
  default) reports and changes nothing, and `-y` repairs. `-p` (or `-a`)
  preens, as `fsck -A` and `systemd-fsck` run it at boot: it repairs without
  asking what `e2fsck -p` does (`e2fsck`'s `PR_PREEN_OK` problems; here link
  counts, bitmaps and free counts), and on anything else it repairs nothing,
  marks the superblock as having errors, prints `DEVICE: UNEXPECTED
  INCONSISTENCY; RUN fsck MANUALLY.` and exits 4. As with `e2fsck`, a
  filesystem that is clean and not due for a check is skipped (`DEVICE: clean,
  N/M files, A/B blocks`) unless `-f` is given; `-y` or `-p` alone does not
  force it. A check falls due for `e2fsck`'s reasons — errors recorded or
  found in the superblock and descriptors, not cleanly unmounted, the backup
  superblock differs (when repairing), the mount count or check interval
  reached — and, erring towards checking, for a journal that needs recovery
  or orphans still to release, since this checker does neither; `fsck-ext4`
  then prints `DEVICE <reason>, check forced.` before the passes run. A
  repairing check records itself the way `e2fsck` does (`s_state`,
  `s_lastcheck`, `s_mnt_count`). `-t` prints the time taken. `-C fd` is
  accepted and ignored: this checker reports no progress.

  Exit codes follow `e2fsck`: 0 clean, 1 errors corrected, 4 errors left
  uncorrected, 8 operational error, or more than one of `-p`/`-a`, `-n` and
  `-y` (`e2fsck`'s "Only one of the options -p/-a, -n or -y may be
  specified."), 16 usage error. `e2fsck`'s flags this checker cannot honour —
  `-b`, `-B`, `-c`, `-D`, `-E`, `-j`, `-k`, `-l`, `-L` and `-z` — are refused
  by name with 16; none is accepted and ignored. An unknown flag or a missing
  device is 16 as well, never 2 (to an `e2fsck` caller, "errors corrected,
  reboot").

  In the library, `FsckOptions::check_only()` is `e2fsck -n` and skips a
  clean filesystem; `.force(true)` makes it `-fn`. `FsckOptions::repair()` is
  `-fy`, and `FsckOptions::preen()` is `-p`. `FsckReport::scope` says whether
  the passes ran and why, and `FsckReport::preen_halted` that a preen stopped.

  The skip, preening and this command line arrived in 4.0.0 (a breaking
  change: `FsckReport` gains `scope` and `preen_halted`, `FsckOptions` gains
  `preen`). At tag `v3.0.0`, `-f` and `FsckOptions::force` are accepted but
  not read, every check runs every pass, and the command line is only
  `-n -y -f -v`, with clap's exit 2 for anything else: don't put that
  release's `fsck.ext4` in a boot path.

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

Or state it per-format. The device's own sector is a floor: `Params` can
raise it — an image built in a file for a 4 KiB-sector drive — but never lower
it, because a block smaller than the device's sector cannot be written at all.
`mke2fs` refuses a block below the logical sector for the same reason.

```rust
Params::new(Profile::Ext4).sector_size(4096)
```

The sector is also the smallest I/O a device has to accept, and a device that
enforces its logical block — a stormblock thin volume, an NVMe namespace
formatted at 4 KiB — answers `EINVAL` to anything smaller. A loop device hides
that behind a kernel read-modify-write; nothing else does. So **every read and
write this crate issues is a whole filesystem block at a block boundary**: the
formatter writes block 0 as boot area plus superblock in one piece and gathers
the reserved inodes into their inode-table blocks, and `Filesystem` reads and
writes the block an inode or the superblock lives in. A block is never smaller
than a sector, so alignment follows. `MemDevice::strict` refuses unaligned I/O
the way a real device does, and the `strict_sector` tests format, check and
write on it.

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
  `Params::concurrency(n)` bounds the groups in flight in one format. The
  default is twice the available parallelism, capped at 64.
- **Memory that does not grow with the filesystem.** A format streams: each
  group's bitmaps are built, written and dropped, and the descriptor table is
  written 256 descriptor blocks at a time. What a format holds is about the
  concurrency × two blocks plus 1 MiB of descriptors (at 4 KiB blocks), for
  1 GiB or 1 PiB alike ([#10](https://github.com/glennswest/mkfs.ext4.rs/issues/10)).
  `examples/formatscale.rs` measures it on a sparse in-memory device. Peak
  RSS less the metadata the device stores was 20 MiB at 256 TiB (it was
  17.6 GiB) and 71 MiB at 1 PiB, which now formats in 149 s. Most of the
  growth is the test device's own page index.
  `fsck` does not do this yet: its block map is one bit per block, 8 GiB at
  256 TiB ([#11](https://github.com/glennswest/mkfs.ext4.rs/issues/11)).
- **Inode counts that fit 32 bits.** From 256 TiB the size class's inode
  ratio asks for 2^32 inodes or more. As with `mke2fs`, the request is capped
  at 2^32 − 1 (with `64bit`; refused without it) and inodes per group come
  down until the total fits: 2032 per group at 256 TiB, 496 at 1 PiB
  ([#9](https://github.com/glennswest/mkfs.ext4.rs/issues/9)).
- **No device round trip.** The `BlockDevice` trait is the seam. A consumer
  formats its own in-memory or network-backed volume directly — no loopback,
  no `/dev` node, no shelling out to `mkfs.ext4`.

It is also *correct by reference*. The spec gives the shape; the differences a
comparison against real `mke2fs` output turns up are what give the values. Each
one is chased to a reason rather than adjusted until it disappears — which is
the difference between matching and merely resembling.

## Presentation

*Writing a Rust Library with AI* — how this crate came to exist, the three-day
analysis loop it broke out of, the method, and what each kind of verification
caught (36 slides):

- [PDF](docs/presentation/ext4-rust.pdf)
- [HTML deck](docs/presentation/ext4-rust.html) — open in a browser, arrow keys to navigate
- also in [glennswest/presentations](https://github.com/glennswest/presentations/blob/main/WritingARustLibraryWithAI.pdf)

## Status

The formatter works and is verified against a real kernel. `tests/verify-on-linux.sh`
builds images, ships them to a Linux host and runs each through
`e2fsck -fn` -> loop mount read-write -> write -> unmount -> `e2fsck -fn`.
All eight configurations pass: ext2, ext3 and ext4, with and without a journal,
at 1 KiB and 4 KiB blocks, from 16 MiB to 1 GiB.
The script loop-mounts, so it needs root on the Linux host it targets (by
default `root@dev.g8.lo`; pass `user@host` to choose another). It is not part of
`cargo test`. `cargo test` runs the golden (geometry and structural compare),
sector-size, journal-floor and strict-sector suites in `tests/` and needs no
privilege.

Geometry and feature masks are asserted against golden filesystems produced by
real `mke2fs` 1.47.3, which is byte-reproducible once the UUID, hash seed and
`SOURCE_DATE_EPOCH` are pinned.

See `CLAUDE.md` for the work plan and what is still outstanding.

## Consumers

- [`fio-ext4`](https://github.com/glennswest/fio.ext4.rs) — reads and writes
  files inside the filesystems this crate creates, in userspace
- [`stormblock`](https://github.com/glennswest/stormblock): filesystem
  templates ("mkfs once, clone forever"), formatted in place through the
  `BlockDevice` seam. It depends on `v3.0.0` with `features = ["std"]`; moving to `v4.0.0`
  brings the streaming formatter (#10, #14).

## Licence

`MIT OR Apache-2.0`, at your option — the Rust ecosystem's usual pair. The MIT
arm is GPLv2-compatible, so this imposes nothing on a kernel or RHEL consumer.
