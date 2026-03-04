#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Build an aarch64 Ubuntu rootfs for the gateway microVM.
#
# Produces a rootfs with k3s pre-installed, plus the gateway-init.sh script
# that runs as PID 1 inside the libkrun VM.
#
# Usage:
#   ./crates/navigator-vm/scripts/build-rootfs.sh [output_dir]
#
# Requires: Docker (or compatible container runtime), curl

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_ROOTFS="${XDG_DATA_HOME:-${HOME}/.local/share}/nemoclaw/gateway/rootfs"
ROOTFS_DIR="${1:-${DEFAULT_ROOTFS}}"
CONTAINER_NAME="krun-rootfs-builder"
IMAGE_TAG="krun-rootfs:gateway"
# K3S_VERSION uses the semver "+" form for GitHub releases.
# The mise env may provide the Docker-tag form with "-" instead of "+";
# normalise to "+" so the GitHub download URL works.
K3S_VERSION="${K3S_VERSION:-v1.29.8+k3s1}"
K3S_VERSION="${K3S_VERSION//-k3s/+k3s}"

echo "==> Building gateway rootfs"
echo "    k3s version: ${K3S_VERSION}"
echo "    Output:      ${ROOTFS_DIR}"

# ── Download k3s binary (outside Docker — much faster) ─────────────────

K3S_BIN="/tmp/k3s-arm64-${K3S_VERSION}"
if [ -f "${K3S_BIN}" ]; then
    echo "==> Using cached k3s binary: ${K3S_BIN}"
else
    echo "==> Downloading k3s ${K3S_VERSION} for arm64..."
    curl -fSL "https://github.com/k3s-io/k3s/releases/download/${K3S_VERSION}/k3s-arm64" \
        -o "${K3S_BIN}"
    chmod +x "${K3S_BIN}"
fi

# ── Build base image with dependencies ─────────────────────────────────

# Clean up any previous run
docker rm -f "${CONTAINER_NAME}" 2>/dev/null || true

echo "==> Building base image..."
docker build --platform linux/arm64 -t "${IMAGE_TAG}" -f - . <<'DOCKERFILE'
FROM ubuntu:22.04
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        iptables \
        iproute2 \
        python3 \
        busybox-static \
    && rm -rf /var/lib/apt/lists/*
# busybox-static provides udhcpc for DHCP inside the VM.
RUN mkdir -p /usr/share/udhcpc && \
    ln -sf /bin/busybox /sbin/udhcpc
RUN mkdir -p /var/lib/rancher/k3s /etc/rancher/k3s
DOCKERFILE

# Create a container and export the filesystem
echo "==> Creating container..."
docker create --platform linux/arm64 --name "${CONTAINER_NAME}" "${IMAGE_TAG}" /bin/true

echo "==> Exporting filesystem..."
rm -rf "${ROOTFS_DIR}"
mkdir -p "${ROOTFS_DIR}"
docker export "${CONTAINER_NAME}" | tar -C "${ROOTFS_DIR}" -xf -

docker rm "${CONTAINER_NAME}"

# ── Inject k3s binary ────────────────────────────────────────────────

echo "==> Injecting k3s binary..."
cp "${K3S_BIN}" "${ROOTFS_DIR}/usr/local/bin/k3s"
chmod +x "${ROOTFS_DIR}/usr/local/bin/k3s"
ln -sf /usr/local/bin/k3s "${ROOTFS_DIR}/usr/local/bin/kubectl"

# ── Inject scripts ────────────────────────────────────────────────────

echo "==> Injecting gateway-init.sh..."
mkdir -p "${ROOTFS_DIR}/srv"
cp "${SCRIPT_DIR}/gateway-init.sh" "${ROOTFS_DIR}/srv/gateway-init.sh"
chmod +x "${ROOTFS_DIR}/srv/gateway-init.sh"

# Keep the hello server around for debugging
cp "${SCRIPT_DIR}/hello-server.py" "${ROOTFS_DIR}/srv/hello-server.py"
chmod +x "${ROOTFS_DIR}/srv/hello-server.py"

# ── Verify ────────────────────────────────────────────────────────────

if [ ! -f "${ROOTFS_DIR}/usr/local/bin/k3s" ]; then
    echo "ERROR: k3s binary not found in rootfs. Something went wrong."
    exit 1
fi

echo ""
echo "==> Rootfs ready at: ${ROOTFS_DIR}"
echo "    Size: $(du -sh "${ROOTFS_DIR}" | cut -f1)"
echo ""
echo "Next steps:"
echo "  1. Run:  ncl gateway"
