#!/bin/bash
# verify-on-linux.sh — put our filesystems in front of a real kernel.
#
# The unit tests prove we can read back our own superblock, which proves very
# little. This builds images with the `mkimage` example, ships them to a Linux
# host, and runs each one through e2fsck, a read-write mount, a write, an
# unmount and a second e2fsck.
#
# The second e2fsck is the one that matters: a filesystem the kernel mounts,
# writes to, and then leaves consistent is a working filesystem. "Mounts" and
# "is writable" are different claims (stormblock#39) and only the write proves
# the second.
#
#   ./tests/verify-on-linux.sh [user@host]
#
# Defaults to root@dev.g8.lo.

set -uo pipefail

HOST="${1:-root@dev.g8.lo}"
REMOTE_DIR=/root/mkfs-ext4-verify
FAILURES=0

GREEN='' RED='' CYAN='' BOLD='' RESET=''
if [ -t 1 ]; then
    GREEN='\033[0;32m'; RED='\033[0;31m'; CYAN='\033[0;36m'
    BOLD='\033[1m'; RESET='\033[0m'
fi
ok()  { echo -e "  ${GREEN}OK${RESET}: $1"; }
bad() { echo -e "  ${RED}FAIL${RESET}: $1"; FAILURES=$((FAILURES+1)); }
hdr() { echo; echo -e "${BOLD}${CYAN}-- $1 --${RESET}"; }

# name:size-mib:profile:extra-args
CASES=(
    "ext4-64m-nojournal:64:ext4:nojournal"
    "ext4-64m-journal:64:ext4:"
    "ext4-256m:256:ext4:"
    "ext4-16m-1k:16:ext4:nojournal block=1024"
    "ext4-512m-4k:512:ext4:block=4096"
    "ext3-64m:64:ext3:"
    "ext2-64m:64:ext2:"
    "ext4-1g:1024:ext4:"
)

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

hdr "building images"
cargo build --quiet --no-default-features --example mkimage || {
    echo "cargo build failed"; exit 1;
}

for case in "${CASES[@]}"; do
    IFS=: read -r name size profile extra <<< "$case"
    # shellcheck disable=SC2086
    if cargo run --quiet --no-default-features --example mkimage -- \
        "$WORK/$name.img" "$size" "$profile" $extra >/dev/null 2>&1; then
        ok "built $name"
    else
        bad "building $name"
    fi
done

hdr "shipping to $HOST"
ssh "$HOST" "rm -rf $REMOTE_DIR && mkdir -p $REMOTE_DIR" || exit 1
scp -q "$WORK"/*.img "$HOST:$REMOTE_DIR/" || exit 1
ok "copied $(ls "$WORK"/*.img | wc -l | tr -d ' ') images"

hdr "checking against the kernel and e2fsck"
for case in "${CASES[@]}"; do
    IFS=: read -r name size profile extra <<< "$case"
    echo
    echo "  == $name =="

    output=$(ssh "$HOST" "bash -s" <<EOF 2>&1
set -uo pipefail
IMG=$REMOTE_DIR/$name.img
MNT=\$(mktemp -d)
rc=0

# A filesystem we just wrote must be clean before anything touches it.
if ! e2fsck -fn "\$IMG" >/tmp/fsck1.log 2>&1; then
    echo "FSCK1_FAIL"; cat /tmp/fsck1.log; rc=1
else
    echo "FSCK1_OK"
fi

# blkid is what a boot path uses to find the filesystem at all.
blkid -o export "\$IMG" 2>/dev/null | grep -E '^(TYPE|UUID)=' | sed 's/^/BLKID_/'

LOOP=\$(losetup --find --show "\$IMG" 2>/dev/null)
if [ -z "\$LOOP" ]; then
    echo "LOSETUP_FAIL"; rc=1
else
    if mount "\$LOOP" "\$MNT" 2>/tmp/mount.log; then
        echo "MOUNT_OK"
        # The write is the point. Mounting read-write and refusing writes is
        # exactly the failure this harness exists to catch.
        if echo "hello from the kernel" > "\$MNT/probe.txt" 2>/tmp/write.log &&
           [ "\$(cat "\$MNT/probe.txt")" = "hello from the kernel" ]; then
            echo "WRITE_OK"
        else
            echo "WRITE_FAIL"; cat /tmp/write.log; rc=1
        fi
        mkdir -p "\$MNT/adir" 2>/dev/null && echo "MKDIR_OK" || { echo "MKDIR_FAIL"; rc=1; }
        # Enough data to force block allocation beyond the first extent.
        if dd if=/dev/urandom of="\$MNT/big.bin" bs=1M count=4 status=none 2>/dev/null; then
            sync
            echo "BIGWRITE_OK"
        else
            echo "BIGWRITE_FAIL"; rc=1
        fi
        [ -d "\$MNT/lost+found" ] && echo "LOSTFOUND_OK" || { echo "LOSTFOUND_FAIL"; rc=1; }
        umount "\$MNT" && echo "UMOUNT_OK" || { echo "UMOUNT_FAIL"; rc=1; }
    else
        echo "MOUNT_FAIL"; cat /tmp/mount.log; rc=1
    fi
    losetup -d "\$LOOP" 2>/dev/null
fi

# After the kernel has had its way with it, is it still consistent?
if ! e2fsck -fn "\$IMG" >/tmp/fsck2.log 2>&1; then
    echo "FSCK2_FAIL"; cat /tmp/fsck2.log; rc=1
else
    echo "FSCK2_OK"
fi

rmdir "\$MNT" 2>/dev/null
exit \$rc
EOF
)

    for check in FSCK1_OK MOUNT_OK WRITE_OK MKDIR_OK BIGWRITE_OK LOSTFOUND_OK UMOUNT_OK FSCK2_OK; do
        if grep -q "^$check\$" <<< "$output"; then
            ok "${check%_OK}"
        else
            bad "${check%_OK}"
        fi
    done
    grep '^BLKID_' <<< "$output" | sed 's/^BLKID_/    /'
    grep -E '_FAIL$' -A6 <<< "$output" | sed 's/^/    /' | head -20
done

hdr "result"
if [ "$FAILURES" -eq 0 ]; then
    echo -e "${GREEN}${BOLD}all checks passed${RESET}"
    exit 0
fi
echo -e "${RED}${BOLD}$FAILURES check(s) failed${RESET}"
exit 1
