#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Build an aarch64 Ubuntu rootfs for the gateway microVM.
#
# Produces a rootfs with k3s pre-installed, the NemoClaw helm chart and
# manifests baked in, and container images pre-loaded for airgap boot.
#
# Usage:
#   ./crates/navigator-vm/scripts/build-rootfs.sh [output_dir]
#
# Requires: Docker (or compatible container runtime), curl, helm

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

# Project root (two levels up from crates/navigator-vm/scripts/)
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

# Container images to pre-load into k3s (arm64).
IMAGE_REPO_BASE="${IMAGE_REPO_BASE:-d1i0nduu2f6qxk.cloudfront.net/navigator}"
IMAGE_TAG="${IMAGE_TAG:-latest}"
SERVER_IMAGE="${IMAGE_REPO_BASE}/server:${IMAGE_TAG}"
SANDBOX_IMAGE="${IMAGE_REPO_BASE}/sandbox:${IMAGE_TAG}"
AGENT_SANDBOX_IMAGE="registry.k8s.io/agent-sandbox/agent-sandbox-controller:v0.1.0"

echo "==> Building gateway rootfs"
echo "    k3s version: ${K3S_VERSION}"
echo "    Images:      ${SERVER_IMAGE}, ${SANDBOX_IMAGE}"
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

# ── Package and inject helm chart ────────────────────────────────────

HELM_CHART_DIR="${PROJECT_ROOT}/deploy/helm/navigator"
CHART_DEST="${ROOTFS_DIR}/var/lib/rancher/k3s/server/static/charts"

if [ -d "${HELM_CHART_DIR}" ]; then
    echo "==> Packaging helm chart..."
    mkdir -p "${CHART_DEST}"
    helm package "${HELM_CHART_DIR}" -d "${CHART_DEST}"
    echo "    $(ls "${CHART_DEST}"/*.tgz 2>/dev/null | xargs -I{} basename {})"
else
    echo "WARNING: Helm chart not found at ${HELM_CHART_DIR}, skipping"
fi

# ── Inject Kubernetes manifests ──────────────────────────────────────
# These are copied to /opt/navigator/manifests/ (staging). gateway-init.sh
# moves them to /var/lib/rancher/k3s/server/manifests/ at boot so the
# k3s Helm Controller auto-deploys them.

MANIFEST_SRC="${PROJECT_ROOT}/deploy/kube/manifests"
MANIFEST_DEST="${ROOTFS_DIR}/opt/navigator/manifests"

echo "==> Injecting Kubernetes manifests..."
mkdir -p "${MANIFEST_DEST}"

for manifest in navigator-helmchart.yaml agent-sandbox.yaml; do
    if [ -f "${MANIFEST_SRC}/${manifest}" ]; then
        cp "${MANIFEST_SRC}/${manifest}" "${MANIFEST_DEST}/"
        echo "    ${manifest}"
    else
        echo "WARNING: ${manifest} not found in ${MANIFEST_SRC}"
    fi
done

# ── Pre-load container images ────────────────────────────────────────
# Pull arm64 images and save as tarballs in the k3s airgap images
# directory. k3s auto-imports from /var/lib/rancher/k3s/agent/images/
# on startup, so no internet access is needed at boot time.

IMAGES_DIR="${ROOTFS_DIR}/var/lib/rancher/k3s/agent/images"
mkdir -p "${IMAGES_DIR}"

echo "==> Pre-loading container images (arm64)..."

pull_and_save() {
    local image="$1"
    local output="$2"

    if [ -f "${output}" ]; then
        echo "    cached: $(basename "${output}")"
        return 0
    fi

    echo "    pulling: ${image}..."
    docker pull --platform linux/arm64 "${image}" --quiet
    echo "    saving:  $(basename "${output}")..."
    docker save "${image}" -o "${output}"
}

pull_and_save "${SERVER_IMAGE}" "${IMAGES_DIR}/navigator-server.tar"
pull_and_save "${SANDBOX_IMAGE}" "${IMAGES_DIR}/navigator-sandbox.tar"
pull_and_save "${AGENT_SANDBOX_IMAGE}" "${IMAGES_DIR}/agent-sandbox-controller.tar"

# ── Verify ────────────────────────────────────────────────────────────

if [ ! -f "${ROOTFS_DIR}/usr/local/bin/k3s" ]; then
    echo "ERROR: k3s binary not found in rootfs. Something went wrong."
    exit 1
fi

echo ""
echo "==> Rootfs ready at: ${ROOTFS_DIR}"
echo "    Size: $(du -sh "${ROOTFS_DIR}" | cut -f1)"

# Show image sizes
echo "    Images:"
for img in "${IMAGES_DIR}"/*.tar; do
    [ -f "$img" ] || continue
    echo "      $(basename "$img"): $(du -sh "$img" | cut -f1)"
done

echo ""
echo "Next steps:"
echo "  1. Run:  ncl gateway"
