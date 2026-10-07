#!/bin/bash
# build-image.sh — the disk image for the kernel verification VM (#15).
#
# A GPT disk whose ESP boots the UEFI Shell; its startup.nsh starts the
# build box's own kernel (EFI stub) with an initramfs of busybox, the ext4
# (and, if modular, loop) kernel modules, e2fsprogs' e2fsck, and this
# checkout's mkfs-ext4 and fsck-ext4. tests/vm/init.sh runs as PID 1 and
# prints `VERIFY PASS` or `VERIFY FAIL <why>` on serial.
#
# No root, here or anywhere: the image is built from files, and only
# stormcentral's throwaway VM ever runs it. The recipe is mkfs.xfs.rs's
# (its #11), which passed on the same boot machine.
#
#   SC_BUILD_OUT=tmp/ext4-verify.img SC_BUILD_OUT_TO=tmp/ext4-verify.img \
#     sc-build 'tests/vm/build-image.sh tmp/ext4-verify.img'
#   stormcentral testhost boot nanatest1 --image tmp/ext4-verify.img \
#     --expect 'VERIFY PASS' --fail 'VERIFY FAIL' --timeout 900 \
#     --url http://stormcentral.g8.lo
#
# Needs cargo, busybox, e2fsck, modinfo, mkfs.fat, mtools, sfdisk, gzip,
# python3 and an OVMF Shell.efi (edk2-ovmf).
set -euo pipefail
OUT=${1:?usage: $0 OUT.img}
HERE=$(cd "$(dirname "$0")" && pwd)
need() { command -v "$1" >/dev/null || { echo "missing $1" >&2; exit 2; }; }
for t in cargo busybox e2fsck modinfo mkfs.fat mcopy mmd sfdisk gzip python3 ldd; do need "$t"; done
SHELL_EFI=${SHELL_EFI:-$(find /usr/share/edk2 -name 'Shell*.efi' 2>/dev/null | head -1)}
[[ -n "$SHELL_EFI" ]] || { echo "no Shell.efi under /usr/share/edk2" >&2; exit 2; }
KVER=$(uname -r)
KERNEL=""
for k in "/lib/modules/$KVER/vmlinuz" "/boot/vmlinuz-$KVER"; do
    [[ -r "$k" ]] && { KERNEL=$k; break; }
done
[[ -n "$KERNEL" ]] || { echo "no readable kernel image for $KVER" >&2; exit 2; }

W=$(mktemp -d "${TMPDIR:-/tmp}/ext4-vm.XXXXXX")
trap 'rm -rf "$W"' EXIT
ROOT=$W/root
mkdir -p "$ROOT"/{bin,sbin,dev,etc,proc,sys,mnt,tmp,modules}
ln -s /proc/mounts "$ROOT/etc/mtab"   # e2fsck asks whether the image is mounted

echo "== building mkfs-ext4 and fsck-ext4"
cargo build --release --quiet --bin mkfs-ext4 --bin fsck-ext4
TARGET=${CARGO_TARGET_DIR:-target}/release
install -m 755 "$TARGET/mkfs-ext4" "$TARGET/fsck-ext4" "$ROOT/bin/"
strip "$ROOT/bin/mkfs-ext4" "$ROOT/bin/fsck-ext4" 2>/dev/null || true
cp "$(command -v busybox)" "$ROOT/bin/busybox"
cp -L "$(command -v e2fsck)" "$ROOT/sbin/e2fsck"

# Every dynamic library (and the loader) the binaries need.
for b in "$ROOT"/bin/* "$ROOT"/sbin/*; do
    ldd "$b" >/dev/null 2>&1 || continue
    for lib in $(ldd "$b" | grep -o '/[^ ]*'); do
        [[ -e "$ROOT$lib" ]] && continue
        mkdir -p "$ROOT$(dirname "$lib")"; cp -L "$lib" "$ROOT$lib"
    done
done

# Modules, dependencies first; a built-in one needs nothing.
ORDER=()
addmod() {
    local m=$1 path dep
    for done_m in "${ORDER[@]:-}"; do [[ "$done_m" == "$m.ko" ]] && return; done
    path=$(modinfo -k "$KVER" -n "$m" 2>/dev/null) || return 0
    [[ -z "$path" || "$path" == "(builtin)" ]] && return 0
    for dep in $(modinfo -k "$KVER" -F depends "$m" | tr ',' ' '); do addmod "$dep"; done
    case "$path" in
        *.xz)  xz -dc "$path" > "$ROOT/modules/$m.ko" ;;
        *.zst) zstd -qdc "$path" > "$ROOT/modules/$m.ko" ;;
        *.gz)  gzip -dc "$path" > "$ROOT/modules/$m.ko" ;;
        *)     cp "$path" "$ROOT/modules/$m.ko" ;;
    esac
    ORDER+=("$m.ko")
}
addmod loop
# metadata_csum's crc32c, where it is a module the kernel only requests at
# mount time (not a modinfo dependency of ext4).
addmod crc32c_generic
addmod crc32c-intel
addmod ext4
printf '%s\n' "${ORDER[@]:-}" > "$ROOT/modules.order"
echo "== modules: ${ORDER[*]:-(all built in)}"
echo "mkfs.ext4.rs $(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo '?'), e2fsck $(e2fsck -V 2>&1 | awk 'NR==1{print $2}')" > "$ROOT/build-info"
install -m 755 "$HERE/init.sh" "$ROOT/init"

# A newc initramfs, written without root (no mknod: headers are bytes).
python3 - "$ROOT" "$W/initrd" <<'PY'
import os, stat, sys
root, out = sys.argv[1], sys.argv[2]
ino = [1]
def hdr(name, mode, size, nlink=1, rmaj=0, rmin=0):
    ino[0] += 1
    fields = [ino[0], mode, 0, 0, nlink, 0, size, 0, 0, rmaj, rmin, len(name) + 1, 0]
    h = b"070701" + b"".join(b"%08x" % f for f in fields)
    h += name.encode() + b"\0"
    return h + b"\0" * ((4 - len(h) % 4) % 4)
def pad(d):
    return d + b"\0" * ((4 - len(d) % 4) % 4)
with open(out, "wb") as f:
    for dirpath, dirs, files in os.walk(root):
        rel = os.path.relpath(dirpath, root)
        if rel != ".":
            f.write(hdr(rel, stat.S_IFDIR | 0o755, 0, 2))
        for n in files:
            p = os.path.join(dirpath, n)
            r = os.path.normpath(os.path.join(rel, n))
            st = os.lstat(p)
            if stat.S_ISLNK(st.st_mode):
                t = os.readlink(p).encode()
                f.write(hdr(r, stat.S_IFLNK | 0o777, len(t)) + pad(t))
            else:
                d = open(p, "rb").read()
                f.write(hdr(r, stat.S_IFREG | (st.st_mode & 0o777), len(d)) + pad(d))
    f.write(hdr("dev/console", stat.S_IFCHR | 0o600, 0, 1, 5, 1))
    f.write(hdr("TRAILER!!!", 0, 0))
PY
gzip -9n "$W/initrd"

# The ESP: the Shell as the default loader, the kernel and initramfs, and a
# startup.nsh that starts the kernel with them.
ESP_MB=160
truncate -s ${ESP_MB}M "$W/esp.img"
mkfs.fat -F 32 -n EXT4VERIFY "$W/esp.img" >/dev/null
mmd -i "$W/esp.img" ::/EFI ::/EFI/BOOT ::/EFI/linux
mcopy -i "$W/esp.img" "$SHELL_EFI" ::/EFI/BOOT/BOOTX64.EFI
mcopy -i "$W/esp.img" "$KERNEL" ::/EFI/linux/vmlinuz.efi
mcopy -i "$W/esp.img" "$W/initrd.gz" ::/EFI/linux/initrd.gz
printf '%s\r\n' '@echo -off' 'echo EXT4-VERIFY: starting the kernel' 'fs0:' \
    '\EFI\linux\vmlinuz.efi initrd=\EFI\linux\initrd.gz console=ttyS0,115200 rdinit=/init panic=-1 loglevel=4' \
    'echo VERIFY FAIL the kernel did not start' 'reset -s' > "$W/startup.nsh"
mcopy -i "$W/esp.img" "$W/startup.nsh" ::/startup.nsh

mkdir -p "$(dirname "$OUT")"
truncate -s 0 "$OUT"
truncate -s $((ESP_MB + 2))M "$OUT"
printf 'label: gpt\nstart=2048, size=%d, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="esp"\n' \
    $((ESP_MB * 2048)) | sfdisk -q "$OUT"
dd if="$W/esp.img" of="$OUT" bs=1M seek=1 conv=notrunc status=none
echo "== $OUT: $(stat -c %s "$OUT") bytes; kernel $KVER ($(du -h "$KERNEL" | cut -f1)), initramfs $(du -h "$W/initrd.gz" | cut -f1)"
