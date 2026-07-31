#!/bin/sh
# flipperos-install-kernel: run `kernel-install` inside a deployed profile root
# for every installed kernel, writing images + BLS entries into the shared
# /boot subvolume.
#
# Usage: flipperos-install-kernel <profile-root> <shared-boot> [profile-name]
#
# The profile's own kernel-install(8), its plugins and BLS config (the pinned
# /etc/kernel/entry-token) live inside <profile-root>; this script only sets up
# the chroot (API filesystems + the shared /boot bind) and drives kernel-install
# for each kernel version found under the profile's module tree.

set -eu

ROOT="${1:?profile root required}"
BOOT="${2:?shared boot dir required}"
PROFILE="${3:-?}"

log() { printf 'install-kernel[%s]: %s\n' "$PROFILE" "$*" >&2; }

# Ensure chroot mount targets exist (they normally do in a real rootfs).
mkdir -p "$ROOT/dev" "$ROOT/dev/pts" "$ROOT/proc" "$ROOT/sys" "$ROOT/run" "$ROOT/boot"

cleanup() {
    # Best-effort unmount in reverse order. Every mountpoint is listed
    # explicitly rather than relying on `umount -R`: that is a util-linux
    # extension BusyBox's umount does not implement, and the installer runs in a
    # BusyBox initramfs — so `-R` silently left /dev, /dev/pts and /boot mounted,
    # keeping the target filesystem busy after the install.
    umount "$ROOT/boot"    2>/dev/null || true
    umount "$ROOT/run"     2>/dev/null || true
    umount "$ROOT/sys"     2>/dev/null || true
    umount "$ROOT/proc"    2>/dev/null || true
    umount "$ROOT/dev/pts" 2>/dev/null || true
    umount "$ROOT/dev"     2>/dev/null || true
}
trap cleanup EXIT INT TERM

mount --bind /dev      "$ROOT/dev"
mount --bind /dev/pts  "$ROOT/dev/pts" 2>/dev/null || true
mount -t proc  proc    "$ROOT/proc"
mount -t sysfs sysfs   "$ROOT/sys"
mount -t tmpfs tmpfs   "$ROOT/run"
# The shared /boot subvolume is where kernel-install writes the kernel image,
# initrd and the per-profile BLS entry (keyed by this profile's entry-token).
mount --bind "$BOOT"   "$ROOT/boot"

log "running kernel-install for all kernels under $ROOT"

# Drive kernel-install from inside the profile for every installed kernel.
chroot "$ROOT" /bin/sh -eu <<'CHROOT'
# Re-point the deployed image at the filesystem we just wrote it onto, BEFORE
# kernel-install bakes the BLS entry. The prebuilt profile carries the build
# host's root UUID in /etc/kernel/cmdline (which kernel-install copies into the
# entry) and in /etc/fstab. Read that old UUID from the cmdline, then swap every
# occurrence of it — first in /etc/fstab, then in the cmdline itself — for this
# filesystem's actual UUID. All subvolumes share the one btrfs filesystem UUID,
# which findmnt reports for `/` (a real `-o subvol=` mount, same as flipper-bls
# relies on). Uses the profile's own util-linux, not the initramfs's.
new_uuid=$(findmnt -no UUID / 2>/dev/null || true)
if [ -z "$new_uuid" ]; then
    echo "install-kernel: could not determine root UUID; skipping UUID rewrite" >&2
elif [ ! -f /etc/kernel/cmdline ]; then
    echo "install-kernel: no /etc/kernel/cmdline; skipping UUID rewrite" >&2
else
    old_uuid=$(sed -n 's/.*root=UUID=\([0-9A-Fa-f-]*\).*/\1/p' /etc/kernel/cmdline | head -n1)
    if [ -z "$old_uuid" ]; then
        echo "install-kernel: no root=UUID= in cmdline; skipping UUID rewrite" >&2
    elif [ "$old_uuid" = "$new_uuid" ]; then
        echo "install-kernel: root UUID already $new_uuid; nothing to rewrite" >&2
    else
        echo "install-kernel: rewriting root UUID $old_uuid -> $new_uuid" >&2
        [ -f /etc/fstab ] && sed -i "s/$old_uuid/$new_uuid/g" /etc/fstab
        sed -i "s/$old_uuid/$new_uuid/g" /etc/kernel/cmdline
    fi
fi

if [ -d /usr/lib/modules ]; then
    MODBASE=/usr/lib/modules
else
    MODBASE=/lib/modules
fi

found=0
for moddir in "$MODBASE"/*/; do
    [ -d "$moddir" ] || continue
    kver=${moddir%/}
    kver=${kver##*/}

    # Locate the kernel image for this version.
    if   [ -f "$moddir/vmlinuz" ];                 then kimg="$moddir/vmlinuz"
    elif [ -f "/boot/vmlinuz-$kver" ];             then kimg="/boot/vmlinuz-$kver"
    elif [ -f "/usr/lib/modules/$kver/vmlinuz" ];  then kimg="/usr/lib/modules/$kver/vmlinuz"
    else
        echo "install-kernel: no kernel image for $kver, skipping" >&2
        continue
    fi

    echo "install-kernel: kernel-install add $kver $kimg" >&2
    kernel-install add "$kver" "$kimg"
    found=$((found + 1))
done

if [ "$found" -eq 0 ]; then
    echo "install-kernel: no kernels found under $MODBASE" >&2
    exit 1
fi
CHROOT

log "done"
