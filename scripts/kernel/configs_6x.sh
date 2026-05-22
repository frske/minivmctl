#!/bin/bash

# Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

# Configuration for Linux kernel 6.6.x builds

arch=$(uname -m)
KERNEL_VERSION="6.6.99"

if [[ $arch = "x86_64" ]]; then
	KERNEL_CFG="microvm-kernel-6.6-x86_64.config"
elif [[ $arch = "aarch64" ]]; then
	KERNEL_CFG="microvm-kernel-6.6-aarch64.config"
fi

BUSYBOX_CFG="busybox_1_32_1_static_config"
BUSYBOX_VERSION="1.32.1"

echo "Kernel Version: $KERNEL_VERSION"
echo "Config: $KERNEL_CFG"
echo "Busybox Version: $BUSYBOX_VERSION"
echo "Config: $BUSYBOX_CFG"
