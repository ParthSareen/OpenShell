#!/bin/bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Init script for the gateway microVM. Runs as PID 1 inside the libkrun VM.
#
# Mounts essential virtual filesystems, deploys bundled manifests (helm chart,
# agent-sandbox controller), then execs k3s server.

set -e

# ── Mount essential filesystems ─────────────────────────────────────────

mount -t proc     proc     /proc     2>/dev/null || true
mount -t sysfs    sysfs    /sys      2>/dev/null || true
mount -t tmpfs    tmpfs    /tmp      2>/dev/null || true
mount -t tmpfs    tmpfs    /run      2>/dev/null || true

# devtmpfs is usually auto-mounted by the kernel, but ensure it's there.
mount -t devtmpfs devtmpfs /dev      2>/dev/null || true
mkdir -p /dev/pts /dev/shm
mount -t devpts   devpts   /dev/pts  2>/dev/null || true
mount -t tmpfs    tmpfs    /dev/shm  2>/dev/null || true

# cgroup2 (unified hierarchy) — required by k3s/containerd.
mkdir -p /sys/fs/cgroup
mount -t cgroup2 cgroup2 /sys/fs/cgroup 2>/dev/null || true

# ── Networking ──────────────────────────────────────────────────────────

hostname gateway 2>/dev/null || true

# Ensure loopback is up (k3s binds to 127.0.0.1).
ip link set lo up 2>/dev/null || true

# Detect whether we have a real network interface (gvproxy) or need a
# dummy interface (TSI / no networking).
if ip link show eth0 >/dev/null 2>&1; then
    # gvproxy networking — bring up eth0 and get an IP via DHCP.
    # gvproxy has a built-in DHCP server that assigns 192.168.127.2/24
    # with gateway 192.168.127.1 and configures ARP properly.
    echo "[gateway-init] detected eth0 (gvproxy networking)"
    ip link set eth0 up 2>/dev/null || true

    # Use DHCP to get IP and configure routes. gvproxy's DHCP server
    # handles ARP resolution which static config does not.
    if command -v udhcpc >/dev/null 2>&1; then
        echo "[gateway-init] running DHCP (udhcpc)..."
        # udhcpc needs a script to apply the lease. Use the busybox
        # default script if available, otherwise write a minimal one.
        UDHCPC_SCRIPT="/usr/share/udhcpc/default.script"
        if [ ! -f "$UDHCPC_SCRIPT" ]; then
            mkdir -p /usr/share/udhcpc
            cat > "$UDHCPC_SCRIPT" << 'DHCP_SCRIPT'
#!/bin/sh
case "$1" in
    bound|renew)
        ip addr flush dev "$interface"
        ip addr add "$ip/$mask" dev "$interface"
        if [ -n "$router" ]; then
            ip route add default via $router dev "$interface"
        fi
        if [ -n "$dns" ]; then
            echo -n > /etc/resolv.conf
            for d in $dns; do
                echo "nameserver $d" >> /etc/resolv.conf
            done
        fi
        ;;
esac
DHCP_SCRIPT
            chmod +x "$UDHCPC_SCRIPT"
        fi
        # -f: stay in foreground, -q: quit after obtaining lease,
        # -n: exit if no lease, -T 2: 2s between retries, -t 5: 5 retries
        udhcpc -i eth0 -f -q -n -T 2 -t 5 -s "$UDHCPC_SCRIPT" 2>&1 || true
    else
        # Fallback to static config if no DHCP client available.
        echo "[gateway-init] no DHCP client, using static config"
        ip addr add 192.168.127.2/24 dev eth0 2>/dev/null || true
        ip route add default via 192.168.127.1 2>/dev/null || true
    fi

    # Ensure DNS is configured. DHCP should have set /etc/resolv.conf,
    # but if it didn't (or static fallback was used), provide a default.
    if [ ! -s /etc/resolv.conf ]; then
        echo "[gateway-init] no DNS configured, using public DNS"
        echo "nameserver 8.8.8.8" > /etc/resolv.conf
        echo "nameserver 8.8.4.4" >> /etc/resolv.conf
    fi

    # Read back the IP we got (from DHCP or static).
    NODE_IP=$(ip -4 addr show eth0 | grep -oP 'inet \K[^/]+' || echo "192.168.127.2")
    echo "[gateway-init] eth0 IP: $NODE_IP"
else
    # TSI or no networking — create a dummy interface for k3s.
    echo "[gateway-init] no eth0 found, using dummy interface (TSI mode)"
    ip link add dummy0 type dummy  2>/dev/null || true
    ip addr add 10.0.2.15/24 dev dummy0  2>/dev/null || true
    ip link set dummy0 up  2>/dev/null || true
    ip route add default dev dummy0  2>/dev/null || true

    NODE_IP="10.0.2.15"
fi

echo "[gateway-init] node IP: $NODE_IP"

# ── k3s data directories ───────────────────────────────────────────────

mkdir -p /var/lib/rancher/k3s
mkdir -p /etc/rancher/k3s

# Clean stale runtime artifacts from previous boots (virtio-fs persists
# the rootfs between VM restarts).
echo "[gateway-init] cleaning stale runtime artifacts..."
rm -rf /var/lib/rancher/k3s/server/tls/temporary-certs 2>/dev/null || true
rm -f  /var/lib/rancher/k3s/server/kine.sock           2>/dev/null || true
# Also clean any stale pid files and unix sockets
find /var/lib/rancher/k3s -name '*.sock' -delete 2>/dev/null || true
find /run -name '*.sock' -delete 2>/dev/null || true

# ── Deploy bundled manifests ────────────────────────────────────────────
# Copy manifests from the staging directory to the k3s auto-deploy path.
# This mirrors the approach in cluster-entrypoint.sh for the Docker path.

K3S_MANIFESTS="/var/lib/rancher/k3s/server/manifests"
BUNDLED_MANIFESTS="/opt/navigator/manifests"

mkdir -p "$K3S_MANIFESTS"

if [ -d "$BUNDLED_MANIFESTS" ]; then
    echo "[gateway-init] deploying bundled manifests..."
    for manifest in "$BUNDLED_MANIFESTS"/*.yaml; do
        [ ! -f "$manifest" ] && continue
        cp "$manifest" "$K3S_MANIFESTS/"
        echo "  $(basename "$manifest")"
    done

    # Remove stale navigator-managed manifests from previous boots.
    for existing in "$K3S_MANIFESTS"/navigator-*.yaml \
                    "$K3S_MANIFESTS"/agent-*.yaml; do
        [ ! -f "$existing" ] && continue
        basename=$(basename "$existing")
        if [ ! -f "$BUNDLED_MANIFESTS/$basename" ]; then
            echo "  removing stale: $basename"
            rm -f "$existing"
        fi
    done
fi

# Patch the HelmChart manifest for VM deployment.
HELMCHART="$K3S_MANIFESTS/navigator-helmchart.yaml"
if [ -f "$HELMCHART" ]; then
    echo "[gateway-init] patching HelmChart manifest..."
    # Use pre-loaded images — don't pull from registry.
    sed -i 's|pullPolicy: Always|pullPolicy: IfNotPresent|' "$HELMCHART"
    # Clear SSH gateway placeholders (default 127.0.0.1 is correct for local VM).
    sed -i 's|sshGatewayHost: __SSH_GATEWAY_HOST__|sshGatewayHost: ""|g' "$HELMCHART"
    sed -i 's|sshGatewayPort: __SSH_GATEWAY_PORT__|sshGatewayPort: 0|g' "$HELMCHART"
fi

# ── Start k3s ──────────────────────────────────────────────────────────

echo "[gateway-init] starting k3s server..."
exec /usr/local/bin/k3s server \
    --disable=traefik \
    --write-kubeconfig-mode=644 \
    --node-ip="$NODE_IP" \
    --kube-apiserver-arg=bind-address=0.0.0.0 \
    --resolv-conf=/etc/resolv.conf \
    --tls-san=localhost,127.0.0.1,10.0.2.15,192.168.127.2
