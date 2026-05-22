#!/bin/bash

# Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

# This script generates a 6.6.x kernel config from the existing 5.4 config
# using `make olddefconfig` to preserve options while updating for new kernel.

set -e

SOURCE=$(readlink -f "$0")
TEST_RESOURCE_DIR="$(dirname "$SOURCE")"

KERNEL_VERSION="6.6.99"
arch=$(uname -m)

if [[ $arch = "x86_64" ]]; then
    OLD_CONFIG="microvm-kernel-5.4-x86_64.config"
    NEW_CONFIG="microvm-kernel-6.6-x86_64.config"
elif [[ $arch = "aarch64" ]]; then
    OLD_CONFIG="microvm-kernel-5.4-aarch64.config"
    NEW_CONFIG="microvm-kernel-6.6-aarch64.config"
else
    echo "Unsupported architecture: $arch"
    exit 1
fi

WORKDIR=$(mktemp -d)
echo "Working in: $WORKDIR"

cleanup() {
    echo "Cleaning up..."
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

# Download and extract kernel sources
KERNEL_ARCHIVE="linux-$KERNEL_VERSION.tar.xz"
KERNEL_URL="https://cdn.kernel.org/pub/linux/kernel/v6.x/$KERNEL_ARCHIVE"
KERNEL_DIR="$WORKDIR/linux-$KERNEL_VERSION"

echo "Downloading kernel $KERNEL_VERSION..."
cd "$WORKDIR"
[ -f "$KERNEL_ARCHIVE" ] || curl "$KERNEL_URL" > "$KERNEL_ARCHIVE"

echo "Extracting kernel sources..."
tar -xf "$KERNEL_ARCHIVE"

# Copy old config
echo "Copying old config from $OLD_CONFIG..."
cp "$TEST_RESOURCE_DIR/$OLD_CONFIG" "$KERNEL_DIR/.config"

# Update config for new kernel
echo "Running make olddefconfig to upgrade config..."
cd "$KERNEL_DIR"
make olddefconfig

# Save new config
echo "Saving new config to $NEW_CONFIG..."
cp "$KERNEL_DIR/.config" "$TEST_RESOURCE_DIR/$NEW_CONFIG"

echo "Config generated successfully: $TEST_RESOURCE_DIR/$NEW_CONFIG"
echo "You can now run make_kernel_image_deb_6x.sh"
