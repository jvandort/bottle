#!/usr/bin/env bash
# Measures bottles booted by bottle-cli boot, on the Mac: how long a boot takes until the agent
# answers, each VM's memory (Activity Monitor's Memory column) as it goes, and whether two VMs on
# the same stage disk share the host's cache of it.
#
#     cli/check/measure.sh DISK...
#
# DISKs are stage disks, bottom first. Needs what boot needs: BOTTLE_LIBKRUN (libkrunfw beside it),
# BOTTLE_GUEST_AGENT, BOTTLE_MKE2FS, and target/release/bottle-cli signed with
# cli/hypervisor.entitlements. Takes a few minutes; stops its VMs when it's done.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cli=$here/../../target/release/bottle-cli
work=$(mktemp -d /tmp/bm.XXXX)
pids=()
cleanup() {
    for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }
since() { perl -e "printf '%.2f s', $(now) - $1"; }
# Activity Monitor's Memory (footprint) and the resident size, of a process.
mem() { echo "memory $(top -l 1 -pid "$1" -stats mem | tail -1), resident $(ps -o rss= -p "$1" | awk '{printf "%dM", $1 / 1024}')"; }
# The Mac's file cache, as vm_stat counts it.
filecache() { vm_stat | awk '/page size of/ {ps = $8} /File-backed pages/ {gsub("\\.", "", $3); printf "%dM\n", $3 * ps / 1048576}'; }
x() { local sock=$1; shift; "$cli" exec "$sock" -- "$@"; }

# Boots VM $1, prints how long until the agent ran a command, and leaves its pid in $vm_pid.
boot() {
    local name=$1 start
    start=$(now)
    "$cli" boot "$work/$name.sock" "$work/$name.ext4" "${disks[@]}" --cpus 2 --mem 2048 --size 16 \
        --console "$work/$name.console" &
    vm_pid=$!
    pids+=("$vm_pid")
    until x "$work/$name.sock" true 2>/dev/null; do
        kill -0 "$vm_pid" 2>/dev/null || { echo "$name died:"; tail -20 "$work/$name.console"; exit 1; }
        sleep 0.02
    done
    echo "$name: agent answering $(since "$start") after launch"
}

disks=("$@")
echo "disks: $(du -ch "${disks[@]}" | tail -1 | cut -f1) on disk"
echo "file cache at the start: $(filecache)"

echo "== boot times (the first makes a's disk too)"
boot a; a=$vm_pid
kill "$a"; wait "$a" 2>/dev/null || true
boot a; a=$vm_pid
kill "$a"; wait "$a" 2>/dev/null || true
boot a; a=$vm_pid
sleep 5
echo "a idle: $(mem "$a")"

echo "== a reads every file in the image"
before=$(filecache); start=$(now)
x "$work/a.sock" sh -c 'find / -xdev -type f -print0 2>/dev/null | xargs -0 cat > /dev/null 2>&1; true'
echo "a read it all in $(since "$start"); file cache $before -> $(filecache)"
echo "a after reading: $(mem "$a")"
x "$work/a.sock" sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'
sleep 15
echo "a after dropping its caches (+15 s): $(mem "$a")"

echo "== b, on the same disks, while a runs"
boot b; b=$vm_pid
before=$(filecache); start=$(now)
x "$work/b.sock" sh -c 'find / -xdev -type f -print0 2>/dev/null | xargs -0 cat > /dev/null 2>&1; true'
echo "b read it all in $(since "$start"); file cache $before -> $(filecache)"
echo "b after reading: $(mem "$b")"
echo "a meanwhile: $(mem "$a")"

echo "== a fills and frees 1 GiB of tmpfs (Replace mode)"
x "$work/a.sock" sh -c 'mkdir -p /run/t && mount -t tmpfs -o size=1200m tmpfs /run/t && dd if=/dev/urandom of=/run/t/x bs=1M count=1024 2>/dev/null'
echo "a holding 1 GiB: $(mem "$a")"
x "$work/a.sock" rm /run/t/x
sleep 15
echo "a after freeing (+15 s): $(mem "$a")"

if [ -n "${COMPARE_PID:-}" ]; then
    echo "== the process at COMPARE_PID ($COMPARE_PID), for comparison: $(mem "$COMPARE_PID")"
fi
