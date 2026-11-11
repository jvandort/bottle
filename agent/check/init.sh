#!/usr/bin/env bash
# Checks `bottle-agent init` out of a VM: stacks DISK... (stage disks, bottom first, as
# `bottle-cli disk` prints them) under a fresh writable disk on loop devices, in a mount and UTS
# namespace of its own, runs commands through the agent there with bottle-cli exec, then stops it
# and stacks the writable disk on the stages as one more: what a build's upper would be.
#
#     sudo BOTTLE_MKE2FS=... agent/check/init.sh DISK...
#
# Linux only, as root, with loop devices; the binaries are target/release's, the agent $BOTTLE_AGENT
# if set (the static build a VM gets, say).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin=$here/../../target/release
# Short: overlayfs takes only so many characters of lowerdir.
work=$(mktemp -d /tmp/ai.XXXX)
loops=()
agent=
cleanup() {
    [ -z "$agent" ] || kill "$agent" 2>/dev/null || true
    wait 2>/dev/null || true
    umount "$work/st" 2>/dev/null || true
    for m in "$work"/m*; do umount "$m" 2>/dev/null || true; done
    for l in "${loops[@]}"; do losetup -d "$l" 2>/dev/null || true; done
    rm -rf "$work"
}
trap cleanup EXIT

lowers=()
for disk in "$@"; do
    l=$(losetup -f -r --show "$disk"); loops+=("$l"); lowers+=(--lower "$l")
done
MKE2FS_CONFIG=/nonexistent "${BOTTLE_MKE2FS:?}" -q -t ext4 -E root_owner=0:0 "$work/rw.ext4" 8G
rw=$(losetup -f --show "$work/rw.ext4"); loops+=("$rw")

unshare -m -u --propagation private "${BOTTLE_AGENT:-$bin/bottle-agent}" init "${lowers[@]}" --upper "$rw" \
    --hostname checked --unix /run/agent.sock &
agent=$!
sock=/proc/$agent/root/run/agent.sock
for _ in $(seq 50); do [ -S "$sock" ] && break; sleep 0.1; done
exec_() { "$bin/bottle-cli" exec "$sock" "$@"; }
[ "$(exec_ cat /proc/sys/kernel/hostname)" = checked ]
exec_ sh -c 'test "$(stat -f -c %T /)" = overlayfs'
exec_ -u nobody id -u | grep -qx 65534
exec_ sh -c 'echo made > /made && rm -rf /etc && test ! -e /etc'
kill "$agent"; wait "$agent" || true; agent=
losetup -d "$rw"; loops=("${loops[@]:0:${#loops[@]}-1}")
e2fsck -fn "$work/rw.ext4" >/dev/null

# The writable disk, as a stage on top of the others.
lower=
i=0
for disk in "$@" "$work/rw.ext4"; do
    mkdir "$work/m$i"; mount -o ro,loop "$disk" "$work/m$i"
    lower="$work/m$i/rootfs${lower:+:$lower}"; i=$((i + 1))
done
mkdir "$work/st"
mount -t overlay overlay -o "ro,lowerdir=$lower" "$work/st"
[ "$(cat "$work/st/made")" = made ]
[ ! -e "$work/st/etc" ]
echo OK
