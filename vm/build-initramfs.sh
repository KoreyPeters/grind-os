#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Build the test VM's boot files from the packages unpacked by fetch.sh:
#   vm/build/vmlinuz        the Debian kernel
#   vm/build/initramfs.gz   busybox, the kernel modules the VM needs, and vm/init
#
# Any failure prints one line of the form
#   BUILD step=<step> expected="..." got="..."
# and exits non-zero.

set -euo pipefail

VM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE="$VM_DIR/.cache"
BUILD="$VM_DIR/build"

# Modules loaded by vm/init. Their dependencies are added automatically.
MODULES=(e1000e vfio-pci vfio_iommu_type1)

fail() { # step expected got
	printf 'BUILD step=%s expected="%s" got="%s"\n' "$1" "$2" "$3" >&2
	exit 1
}

for tool in depmod modprobe cpio gzip; do
	command -v "$tool" >/dev/null || fail tool "$tool on PATH" "not found"
done

for name in kernel busybox; do
	[ -f "$CACHE/$name/.sha256" ] || fail inputs "$CACHE/$name unpacked (run vm/fetch.sh)" "missing"
done

kvers=("$CACHE"/kernel/lib/modules/*/)
[ "${#kvers[@]}" -eq 1 ] && [ -d "${kvers[0]}" ] ||
	fail kernel-version "exactly one directory in $CACHE/kernel/lib/modules" "${kvers[*]}"
KVER="$(basename "${kvers[0]}")"
VMLINUZ="$CACHE/kernel/boot/vmlinuz-$KVER"
BUSYBOX="$CACHE/busybox/bin/busybox"
[ -f "$VMLINUZ" ] || fail kernel-image "$VMLINUZ" "missing"
[ -x "$BUSYBOX" ] || fail busybox "$BUSYBOX executable" "missing or not executable"

rm -rf "$BUILD/modtree" "$BUILD/root"
mkdir -p "$BUILD/modtree/lib/modules" "$BUILD/root"

# Resolve module dependencies. depmod writes into a hardlinked copy so the
# unpacked cache stays exactly as fetched.
cp -al "$CACHE/kernel/lib/modules/$KVER" "$BUILD/modtree/lib/modules/"
depmod -b "$BUILD/modtree" "$KVER" || fail depmod "depmod succeeds on the full module tree" "exit $?"

ROOT="$BUILD/root"
MODDIR="lib/modules/$KVER"
mkdir -p "$ROOT/$MODDIR"
for mod in "${MODULES[@]}"; do
	deps="$(modprobe -d "$BUILD/modtree" -S "$KVER" --show-depends "$mod" 2>&1)" ||
		fail module-deps "modprobe --show-depends $mod succeeds" "$deps"
	while read -r kind path _; do
		[ "$kind" = insmod ] || continue
		rel="${path#"$BUILD/modtree/"}"
		[ "$rel" != "$path" ] || fail module-path "path under $BUILD/modtree" "$path"
		mkdir -p "$ROOT/$(dirname "$rel")"
		cp "$path" "$ROOT/$rel"
	done <<<"$deps"
done
for f in modules.order modules.builtin modules.builtin.modinfo; do
	cp "$CACHE/kernel/$MODDIR/$f" "$ROOT/$MODDIR/"
done
depmod -b "$ROOT" "$KVER" || fail depmod "depmod succeeds on the initramfs module tree" "exit $?"

# Busybox and its applet links.
mkdir -p "$ROOT"/{bin,sbin,usr/bin,usr/sbin,dev,proc,sys,tmp,root}
cp "$BUSYBOX" "$ROOT/bin/busybox"
while read -r applet; do
	[ "$applet" = bin/busybox ] && continue
	target="$(realpath -m --relative-to="$ROOT/$(dirname "$applet")" "$ROOT/bin/busybox")"
	ln -sf "$target" "$ROOT/$applet"
done < <("$ROOT/bin/busybox" --list-full)

install -m 0755 "$VM_DIR/init" "$ROOT/init"

# Pack. Fixed timestamps, ownership and order keep the archive reproducible.
find "$ROOT" -exec touch -h -d @0 {} +
(cd "$ROOT" && find . -print0 | LC_ALL=C sort -z |
	cpio --null -o -H newc --owner=0:0 --reproducible --quiet) |
	gzip -9 -n >"$BUILD/initramfs.gz.part"
mv "$BUILD/initramfs.gz.part" "$BUILD/initramfs.gz"
cp "$VMLINUZ" "$BUILD/vmlinuz"
rm -rf "$BUILD/modtree"

echo "build: kernel $KVER"
echo "build: modules: $(cd "$ROOT" && find lib/modules -name '*.ko' -printf '%f ' )"
echo "build: $BUILD/vmlinuz"
echo "build: $BUILD/initramfs.gz ($(du -h "$BUILD/initramfs.gz" | cut -f1))"
echo "build: ok"
