#!/bin/bash

# Build a 6.x kernel image and an Ubuntu noble rootfs in one command.

set -e

SOURCE=$(readlink -f "$0")
SCRIPT_DIR="$(dirname "$SOURCE")"

KERNEL_SCRIPT="$SCRIPT_DIR/kernel/make_kernel_image_deb_6x.sh"
ROOTFS_SCRIPT="$SCRIPT_DIR/disk/make_rootfs_6x.sh"
KERNEL_VERSION="6.6.99"

OPTIND=1
CLEAN=
KERNEL_FMT=
MAKEPROCS=4
WORKDIR=$(mktemp -d)
KERNEL_BINARY_NAME=
ROOTFS_FILE="rootfs.ext4"
ROOTFS_SIZE="1G"

USAGE="
Usage: $(basename "$SOURCE") -f (elf|bzimage) [-j nprocs] [-k kernel] [-w workdir] [-o rootfs] [-s size] [-c]

Options:
  -f elf|bzimage    Kernel image format (either elf or bzimage).
  -j nprocs         Number of CPUs to use for the kernel build. Default: 4.
  -k kernel         Name of the resulting kernel image.
  -w workdir        Working directory for all generated artifacts.
  -o rootfs         Name of the resulting rootfs image. Default: rootfs.ext4.
  -s size           Size of the resulting rootfs image. Default: 1G.
  -c                Clean up the working directory after the build.
"

while getopts ":cf:j:k:o:s:w:" opt; do
    case "$opt" in
    c)  CLEAN=1
        ;;
    f)  KERNEL_FMT="$OPTARG"
        ;;
    j)  MAKEPROCS="$OPTARG"
        ;;
    k)  KERNEL_BINARY_NAME="$OPTARG"
        ;;
    o)  ROOTFS_FILE="$OPTARG"
        ;;
    s)  ROOTFS_SIZE="$OPTARG"
        ;;
    w)  rm -rf "$WORKDIR"
        WORKDIR="$OPTARG"
        ;;
    *)  echo "$USAGE"
        exit 1
    esac
done
shift $((OPTIND-1))

die() {
    echo "[ERROR] $1" >&2
    echo "$USAGE" >&2
    exit 1
}

cleanup() {
    if [ -n "$CLEAN" ]; then
        echo "Cleaning $WORKDIR..."
        rm -rf "$WORKDIR"
    fi
}

[ -z "$KERNEL_FMT" ] && die "Kernel format not specified."
[ "$KERNEL_FMT" != "elf" ] && [ "$KERNEL_FMT" != "bzimage" ] && die "Invalid kernel format: $KERNEL_FMT"
[ ! -x "$KERNEL_SCRIPT" ] && die "Kernel build script is not executable: $KERNEL_SCRIPT"
[ ! -x "$ROOTFS_SCRIPT" ] && die "Rootfs build script is not executable: $ROOTFS_SCRIPT"

cleanup

KERNEL_WORKDIR="$WORKDIR/kernel"
ROOTFS_WORKDIR="$WORKDIR/rootfs"
KERNEL_DIR="$KERNEL_WORKDIR/linux-$KERNEL_VERSION"
DEBDIR="$KERNEL_DIR/deb"

mkdir -p "$KERNEL_WORKDIR" "$ROOTFS_WORKDIR"

kernel_args=(-f "$KERNEL_FMT" -j "$MAKEPROCS" -w "$KERNEL_WORKDIR")
[ -n "$KERNEL_BINARY_NAME" ] && kernel_args+=(-k "$KERNEL_BINARY_NAME")

echo "Building kernel and deb packages..."
"$KERNEL_SCRIPT" "${kernel_args[@]}"

[ ! -d "$DEBDIR" ] && die "Kernel deb directory was not created: $DEBDIR"

echo "Building rootfs..."
"$ROOTFS_SCRIPT" -d "$DEBDIR" -w "$ROOTFS_WORKDIR" -o "$ROOTFS_FILE" -s "$ROOTFS_SIZE"

echo "Done!"
echo "Kernel workdir: $KERNEL_DIR"
echo "Deb packages:   $DEBDIR"
echo "Rootfs image:   $ROOTFS_WORKDIR/$ROOTFS_FILE"

cleanup
exit 0
