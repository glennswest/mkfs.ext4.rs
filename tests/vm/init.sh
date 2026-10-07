#!/bin/busybox sh
# The init of the kernel-verification VM (#15), run as PID 1 from the
# initramfs tests/vm/build-image.sh makes. Everything here is the real
# kernel's: our mkfs-ext4 formats sparse image files in tmpfs, and the kernel
# loop-mounts them, writes, unmounts and remounts. Real e2fsck -fn (e2fsprogs,
# from the build box) and our fsck-ext4 -fn check each one as formatted and
# again after the kernel wrote to it.
#
# The check after the kernel's write is the one that counts: "mounts
# read-write" and "is writable" are different claims (stormblock#39), and only
# a completed write that leaves the filesystem consistent proves the second.
#
# Prints `VERIFY PASS` or `VERIFY FAIL <why>` on the serial console, then
# powers off. stormcentral's testhost boot watches for those lines.
/bin/busybox mount -t proc proc /proc
/bin/busybox --install -s /bin
export PATH=/bin:/sbin
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
mkdir -p /mnt /work
mount -t tmpfs -o size=90% tmpfs /work
echo 1 > /proc/sys/kernel/printk 2>/dev/null

say() { echo "EXT4-VERIFY: $*"; }
fail() {
    say "FAIL: $*"
    dmesg | grep -iE 'ext[234]|jbd2|loop|mmp' | tail -20 | sed 's/^/EXT4-VERIFY dmesg: /'
    [ -s /work/check.log ] && head -40 /work/check.log | sed 's/^/EXT4-VERIFY check: /'
    echo "VERIFY FAIL $*"
    sync; poweroff -f; sleep 30
}

say "kernel $(uname -r), $(cat /build-info 2>/dev/null)"
for m in $(cat /modules.order 2>/dev/null); do
    insmod "/modules/$m" || fail "insmod $m"
done
for t in ext2 ext3 ext4; do
    grep -qw $t /proc/filesystems || fail "the kernel has no $t"
done

check() { # image what — real e2fsck and ours, both read-only and forced
    e2fsck -fn "$1" >/work/check.log 2>&1 || fail "e2fsck -fn $2 (exit $?)"
    fsck-ext4 -fn "$1" >/work/check.log 2>&1 || fail "fsck-ext4 -fn $2 (exit $?)"
}

exercise() { # image type name — mount rw, write, unmount, check, remount ro
    local img=$1 type=$2 name=$3 i sum
    mount -t "$type" -o loop,rw "$img" /mnt 2>/work/err || fail "$name: mount: $(cat /work/err)"
    grep -q " /mnt $type rw" /proc/mounts || fail "$name: not mounted read-write"
    echo "hello from the kernel" > /mnt/probe.txt || fail "$name: write"
    mkdir -p /mnt/adir/sub && echo x > /mnt/adir/sub/f || fail "$name: mkdir"
    # Out of one directory block and into new inode-table blocks.
    i=0; while [ $i -lt 300 ]; do echo $i > /mnt/adir/file$i || break; i=$((i+1)); done
    [ $i -eq 300 ] || fail "$name: only $i of 300 files"
    rm /mnt/adir/file7 /mnt/adir/file150 || fail "$name: unlink"
    dd if=/dev/urandom of=/mnt/big.bin bs=1M count=4 2>/dev/null || fail "$name: 4 MiB write"
    sum=$(md5sum /mnt/big.bin | cut -d' ' -f1)
    sync
    umount /mnt || fail "$name: umount"
    check "$img" "$name: after the kernel wrote"
    mount -t "$type" -o loop,ro "$img" /mnt 2>/work/err || fail "$name: remount: $(cat /work/err)"
    [ "$(cat /mnt/probe.txt)" = "hello from the kernel" ] && [ -f /mnt/adir/file299 ] \
        && [ ! -e /mnt/adir/file7 ] && [ "$(md5sum /mnt/big.bin | cut -d' ' -f1)" = "$sum" ] \
        || fail "$name: read back"
    umount /mnt || fail "$name: umount after remount"
}

# name : size : kernel type : mkfs-ext4 options
for case in \
    "ext4-64m-nojournal:64M:ext4:-t ext4 --no-journal" \
    "ext4-256m:256M:ext4:-t ext4" \
    "ext4-16m-1k:16M:ext4:-t ext4 -b 1024 --no-journal" \
    "ext4-512m-4k-sector:512M:ext4:-t ext4 --sector-size 4096" \
    "ext3-256m:256M:ext3:-t ext3" \
    "ext2-256m:256M:ext2:-t ext2" \
    "ext3-64m-1k:64M:ext3:-t ext3 -b 1024" \
    "ext4-1g-mmp:1G:ext4:-t ext4 -O mmp" \
    "ext4-2g-meta_bg:2G:ext4:-t ext4 -O meta_bg,^resize_inode" \
    "ext4-1g-orphan_file:1G:ext4:-t ext4 -O orphan_file" \
    "ext4-1g-i128:1G:ext4:-t ext4 -I 128 -L kernel" \
    "ext4-64g-lazy:64G:ext4:-t ext4 --lazy-itable-init --zeroed-medium"
do
    name=${case%%:*}; rest=${case#*:}
    size=${rest%%:*}; rest=${rest#*:}
    type=${rest%%:*}; opts=${rest#*:}
    img=/work/$name.img
    rm -f "$img"; truncate -s "$size" "$img" || fail "$name: truncate"
    # shellcheck disable=SC2086
    mkfs-ext4 -q $opts "$img" >/work/err 2>&1 || fail "$name: mkfs-ext4: $(cat /work/err)"
    check "$img" "$name: as formatted"
    exercise "$img" "$type" "$name"
    say "$name ($size $opts): e2fsck and fsck-ext4 clean; mounted $type rw, written, both clean again, remounted and read back"
    rm -f "$img"
done

echo "VERIFY PASS"
sync; poweroff -f; sleep 30
