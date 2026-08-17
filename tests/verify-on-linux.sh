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
    # No flex_bg, so no contiguous run long enough for an 8192-block journal.
    # The journal has to be allocated in pieces.
    "ext3-256m:256:ext3:"
    "ext2-256m:256:ext2:"
    # Multiple mount protection: the kernel should take the fence on mount.
    "ext4-64m-mmp:64:ext4:mmp"
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

hdr "repairs, judged by real e2fsck"
# Our fsck is only worth having if its repairs are correct, and the only
# authority on that is e2fsprogs. Break a filesystem in a known way, repair it
# with our fsck, and require e2fsck to call the result clean.
cargo build --quiet --example corrupt 2>/dev/null
cargo build --quiet --bin mkfs-ext4 --bin fsck-ext4 2>/dev/null
MKFS=target/debug/mkfs-ext4
FSCK=target/debug/fsck-ext4

ssh "$HOST" "rm -rf $REMOTE_DIR/repair && mkdir -p $REMOTE_DIR/repair" || exit 1

for mode in free-count block-bitmap inode-bitmap link-count dir-count; do
    img="$WORK/repair-$mode.img"
    dd if=/dev/zero of="$img" bs=1M count=64 status=none
    "$MKFS" -q -t ext4 "$img" || { bad "$mode: format"; continue; }
    cargo run --quiet --example corrupt -- "$img" "$mode" >/dev/null 2>&1 \
        || { bad "$mode: corrupt"; continue; }

    "$FSCK" -n "$img" >/dev/null 2>&1
    [ $? -eq 4 ] && ok "$mode: detected" || bad "$mode: not detected"

    "$FSCK" -y "$img" >/dev/null 2>&1
    [ $? -eq 1 ] && ok "$mode: repaired" || bad "$mode: not repaired"

    "$FSCK" -n "$img" >/dev/null 2>&1
    [ $? -eq 0 ] && ok "$mode: clean afterwards" || bad "$mode: still dirty"
done

scp -q "$WORK"/repair-*.img "$HOST:$REMOTE_DIR/repair/" || exit 1
verdicts=$(ssh "$HOST" "for f in $REMOTE_DIR/repair/*.img; do
    if e2fsck -fn \"\$f\" >/dev/null 2>&1; then echo \"CLEAN \$(basename \$f)\";
    else echo \"DIRTY \$(basename \$f)\"; fi
done")
while read -r verdict name; do
    if [ "$verdict" = "CLEAN" ]; then
        ok "e2fsck accepts our repair of ${name#repair-}"
    else
        bad "e2fsck rejects our repair of ${name#repair-}"
    fi
done <<< "$verdicts"

# ---------------------------------------------------------------------------
# Sector size. Real drives report 512 or 4096, and that number is a floor: a
# filesystem whose blocks are smaller than a sector cannot be written a block
# at a time. The kernel hides that behind a read-modify-write; lwext4 does not,
# and refuses every write instead (stormblock#39).
#
# tests/sector_size.rs asserts our choice against a table of what mke2fs
# chooses. A table is only worth what its source is, so this asks a real
# mke2fs, on real devices of each sector size, whether the table is still
# true. If e2fsprogs ever changes its size classes, this fails and the table
# gets updated — rather than the test quietly asserting yesterday's answer.
# ---------------------------------------------------------------------------
hdr "sector size — is the table in tests/sector_size.rs still what mke2fs does?"

sector_report=$(ssh "$HOST" "bash -s" <<'REMOTE'
for sec in 512 4096; do
    for sz in 64M 512M; do
        img=/tmp/sector-check.img
        rm -f "$img"; truncate -s "$sz" "$img"
        dev=$(losetup -f --show -b "$sec" "$img" 2>/dev/null) || { echo "$sec $sz SKIP"; continue; }
        mke2fs -qF -t ext4 "$dev" 2>/dev/null
        theirs=$(dumpe2fs -h "$dev" 2>/dev/null | awk -F: '/Block size/ {gsub(/ /,"",$2); print $2}')
        losetup -d "$dev"
        rm -f "$img"
        echo "$sec $sz ${theirs:-unknown}"
    done
done
REMOTE
)

# The same values tests/sector_size.rs asserts we produce.
expected_block_size() {
    case "$1 $2" in
        "512 64M")   echo 1024 ;;
        "512 512M")  echo 4096 ;;
        "4096 64M")  echo 4096 ;;
        "4096 512M") echo 4096 ;;
        *)           echo unknown ;;
    esac
}

while read -r sec sz theirs; do
    [ -z "$sec" ] && continue
    want=$(expected_block_size "$sec" "$sz")
    if [ "$theirs" = "SKIP" ]; then
        bad "$sz on ${sec}-byte sectors: could not create a loop device"
    elif [ "$theirs" = "$want" ]; then
        ok "$sz on ${sec}-byte sectors: mke2fs chooses ${theirs}, as the table says"
    else
        bad "$sz on ${sec}-byte sectors: mke2fs now chooses $theirs, table says $want — update tests/sector_size.rs"
    fi
done <<< "$sector_report"

hdr "result"
if [ "$FAILURES" -eq 0 ]; then
    echo -e "${GREEN}${BOLD}all checks passed${RESET}"
    exit 0
fi
echo -e "${RED}${BOLD}$FAILURES check(s) failed${RESET}"
exit 1
