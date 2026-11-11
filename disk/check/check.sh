#!/usr/bin/env bash
# Checks bottle-cli disk against GNU tar: makes ARCHIVE's disks, split into stages after each K
# layers, mounts them as the guest does (each read-only, stacked by overlayfs) and compares what's
# there, file by file, with ARCHIVE's layers extracted one by one, whiteouts applied as they come.
#
#     sudo BOTTLE_MKE2FS=... disk/check/check.sh ARCHIVE [K]...
#
# Prints SAME, or the difference. Linux only, as root, with loop devices; bottle-cli is
# $BOTTLE_CLI, or target/release's. mkoci.py makes ARCHIVEs to check: `mkoci.py synth OUT` one of
# the cases that matter, whiteouts, opaque directories and the rest.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
archive=$1; shift
# Short: overlayfs takes only so many characters of lowerdir.
work=$(mktemp -d /tmp/dc.XXXX)
cleanup() {
    umount "$work/merged" 2>/dev/null || true
    for m in "$work"/m*; do umount "$m" 2>/dev/null || true; done
    rm -rf "$work"
}
trap cleanup EXIT

# The reference: each layer's whiteouts applied to what's there, then the rest of it extracted.
mkdir "$work/ref"
python3 -I "$here/layers.py" "$archive" "$work/layers"
for l in $(ls "$work/layers" | sort -n); do
    python3 -I "$here/prepare.py" "$work/ref" "$work/layers/$l"
    tar -xpf "$work/layers/$l" -C "$work/ref" --numeric-owner --xattrs --xattrs-include='*' \
        --exclude='.wh.*' --overwrite
done

# The disks, stacked.
bases=()
for k in "$@"; do
    python3 -I "$here/mkoci.py" prefix "$archive" "$work/base$k.tar" "$k"
    bases+=("$work/base$k.tar")
done
mkdir "$work/disks"
mapfile -t disks < <(BOTTLE_MKE2FS=${BOTTLE_MKE2FS:?} "${BOTTLE_CLI:-$here/../../target/release/bottle-cli}" disk "$work/disks" "$archive" "${bases[@]}")
lower=
i=0
for d in "${disks[@]}"; do
    e2fsck -fn "$d" >/dev/null || { echo "e2fsck: $d"; exit 1; }
    mkdir "$work/m$i"
    mount -o ro,loop -t ext4 "$d" "$work/m$i"
    lower="$work/m$i/rootfs${lower:+:$lower}"
    i=$((i + 1))
done
mkdir "$work/merged"
if [ $i -eq 1 ]; then
    mount --bind -o ro "$work/m0/rootfs" "$work/merged"
else
    mount -t overlay overlay -o "ro,lowerdir=$lower" "$work/merged"
fi
python3 -I "$here/walk.py" "$work/ref" > "$work/ref.txt"
python3 -I "$here/walk.py" "$work/merged" > "$work/merged.txt"
echo "stages: ${#disks[@]}, paths: $(wc -l < "$work/ref.txt")"
diff -u "$work/ref.txt" "$work/merged.txt" && echo SAME
