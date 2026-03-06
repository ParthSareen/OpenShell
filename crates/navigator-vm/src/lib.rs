// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `MicroVM` runtime using libkrun for hardware-isolated execution.
//!
//! This crate provides a thin wrapper around the libkrun C API to boot
//! lightweight VMs backed by virtio-fs root filesystems. On macOS ARM64,
//! it uses Apple's Hypervisor.framework; on Linux it uses KVM.
//!
//! # Codesigning (macOS)
//!
//! The calling binary must be codesigned with the
//! `com.apple.security.hypervisor` entitlement. See `entitlements.plist`.

#![allow(unsafe_code)]

mod ffi;

use std::ffi::CString;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Instant;

// ── Error type ─────────────────────────────────────────────────────────

/// Errors that can occur when configuring or launching a microVM.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum VmError {
    /// A libkrun FFI call returned a negative error code.
    #[error("{func} failed with error code {code}")]
    Krun { func: &'static str, code: i32 },

    /// The rootfs directory does not exist.
    #[error(
        "rootfs directory not found: {path}\nRun: ./crates/navigator-vm/scripts/build-rootfs.sh"
    )]
    RootfsNotFound { path: String },

    /// A path contained invalid UTF-8.
    #[error("path is not valid UTF-8: {0}")]
    InvalidPath(String),

    /// `CString::new` failed (embedded NUL byte).
    #[error("invalid C string: {0}")]
    CString(#[from] std::ffi::NulError),

    /// A required host binary was not found.
    #[error("required binary not found: {path}\n{hint}")]
    BinaryNotFound { path: String, hint: String },

    /// `fork()` failed.
    #[error("fork() failed: {0}")]
    Fork(String),

    /// Post-boot bootstrap failed.
    #[error("bootstrap failed: {0}")]
    Bootstrap(String),
}

/// Check a libkrun return code; negative values are errors.
fn check(ret: i32, func: &'static str) -> Result<(), VmError> {
    if ret < 0 {
        Err(VmError::Krun { func, code: ret })
    } else {
        Ok(())
    }
}

// ── Configuration ──────────────────────────────────────────────────────

/// Networking backend for the microVM.
#[derive(Debug, Clone)]
pub enum NetBackend {
    /// TSI (Transparent Socket Impersonation) — default libkrun networking.
    /// Simple but intercepts guest loopback connections, breaking k3s.
    Tsi,

    /// No networking — disable vsock/TSI entirely. For debugging only.
    None,

    /// gvproxy (vfkit mode) — real `eth0` interface via virtio-net.
    /// Requires gvproxy binary on the host. Port forwarding is done
    /// through gvproxy's HTTP API.
    Gvproxy {
        /// Path to the gvproxy binary.
        binary: PathBuf,
    },
}

/// Configuration for a libkrun microVM.
pub struct VmConfig {
    /// Path to the extracted rootfs directory (aarch64 Linux).
    pub rootfs: PathBuf,

    /// Number of virtual CPUs.
    pub vcpus: u8,

    /// RAM in MiB.
    pub mem_mib: u32,

    /// Executable path inside the VM.
    pub exec_path: String,

    /// Arguments to the executable (argv, excluding argv\[0\]).
    pub args: Vec<String>,

    /// Environment variables in `KEY=VALUE` form.
    /// If empty, a minimal default set is used.
    pub env: Vec<String>,

    /// Working directory inside the VM.
    pub workdir: String,

    /// TCP port mappings in `"host_port:guest_port"` form.
    /// Only used with TSI networking.
    pub port_map: Vec<String>,

    /// libkrun log level (0=Off .. 5=Trace).
    pub log_level: u32,

    /// Optional file path for VM console output. If `None`, console output
    /// goes to the parent directory of the rootfs as `console.log`.
    pub console_output: Option<PathBuf>,

    /// Networking backend.
    pub net: NetBackend,
}

impl VmConfig {
    /// Default gateway configuration: boots k3s server inside the VM.
    ///
    /// Runs `/srv/gateway-init.sh` which mounts essential filesystems,
    /// deploys the `NemoClaw` helm chart, and execs `k3s server`.
    /// Exposes the Kubernetes API on port 6443 and the `NemoClaw`
    /// gateway (navigator server `NodePort`) on port 30051.
    pub fn gateway(rootfs: PathBuf) -> Self {
        Self {
            rootfs,
            vcpus: 4,
            mem_mib: 8192,
            exec_path: "/srv/gateway-init.sh".to_string(),
            args: vec![],
            env: vec![
                "HOME=/root".to_string(),
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
                "TERM=xterm".to_string(),
            ],
            workdir: "/".to_string(),
            port_map: vec![
                // Map host 6443 -> guest 6444 (real kube-apiserver).
                // The k3s dynamiclistener on 6443 has TLS issues through
                // port forwarding, so we go directly to the apiserver.
                "6443:6444".to_string(),
                // Navigator server — with hostNetwork the server binds
                // directly to port 8080 on the VM's interface, bypassing
                // NodePort (which requires kube-proxy / iptables).
                // Map host 30051 -> guest 8080 so the external-facing
                // port stays the same for CLI clients.
                "30051:8080".to_string(),
            ],
            log_level: 3, // Info — for debugging
            console_output: None,
            net: NetBackend::Gvproxy {
                binary: find_gvproxy().unwrap_or_else(|| PathBuf::from("/opt/podman/bin/gvproxy")),
            },
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Build a null-terminated C string array from a slice of strings.
///
/// Returns both the `CString` owners (to keep them alive) and the pointer array.
fn c_string_array(strings: &[&str]) -> Result<(Vec<CString>, Vec<*const libc::c_char>), VmError> {
    let owned: Vec<CString> = strings
        .iter()
        .map(|s| CString::new(*s))
        .collect::<Result<Vec<_>, _>>()?;
    let mut ptrs: Vec<*const libc::c_char> = owned.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(ptr::null()); // null terminator
    Ok((owned, ptrs))
}

/// Discover the Homebrew lib directory.
fn homebrew_lib_dir() -> String {
    std::process::Command::new("brew")
        .args(["--prefix"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| format!("{}/lib", s.trim()))
            } else {
                None
            }
        })
        .unwrap_or_else(|| "/opt/homebrew/lib".to_string())
}

/// Ensure `DYLD_FALLBACK_LIBRARY_PATH` includes the Homebrew lib directory.
///
/// libkrun loads `libkrunfw.5.dylib` at runtime via `dlopen`. On macOS, dyld
/// only reads `DYLD_FALLBACK_LIBRARY_PATH` at process startup — setting it
/// programmatically after launch has no effect. If the variable isn't already
/// set, we re-exec the current process with it configured so dyld picks it up.
///
/// Returns `Ok(())` if the path is already set, or does not return (re-execs).
fn ensure_krunfw_path() -> Result<(), VmError> {
    let key = "DYLD_FALLBACK_LIBRARY_PATH";
    let homebrew_lib = homebrew_lib_dir();

    if let Ok(existing) = std::env::var(key)
        && existing.contains(&homebrew_lib)
    {
        return Ok(()); // Already set — nothing to do.
    }

    // Re-exec ourselves with the library path set. dyld will process it
    // at startup, making libkrunfw discoverable for libkrun's dlopen.
    let exe = std::env::current_exe().map_err(|e| VmError::Fork(e.to_string()))?;
    let args: Vec<String> = std::env::args().collect();

    let new_val = match std::env::var(key) {
        Ok(existing) => format!("{homebrew_lib}:{existing}"),
        Err(_) => homebrew_lib,
    };

    eprintln!("re-exec: setting {key} for libkrunfw discovery");
    // SAFETY: single-threaded at this point (before fork).
    unsafe {
        std::env::set_var(key, &new_val);
    }

    // exec replaces the process — if it returns, something went wrong.
    let err = std::process::Command::new(exe).args(&args[1..]).exec();
    Err(VmError::Fork(format!("re-exec failed: {err}")))
}

/// Try to find gvproxy in common locations.
fn find_gvproxy() -> Option<PathBuf> {
    // Check PATH first
    if let Ok(output) = std::process::Command::new("which").arg("gvproxy").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    // Common Podman installation paths
    for p in &[
        "/opt/podman/bin/gvproxy",
        "/opt/homebrew/bin/gvproxy",
        "/usr/local/bin/gvproxy",
    ] {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Issue a gvproxy expose call via its HTTP API (unix socket).
///
/// Sends a raw HTTP/1.1 POST request over the unix socket to avoid
/// depending on `curl` being installed on the host.
fn gvproxy_expose(api_sock: &Path, body: &str) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut stream =
        UnixStream::connect(api_sock).map_err(|e| format!("connect to gvproxy API socket: {e}"))?;

    let request = format!(
        "POST /services/forwarder/expose HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        body.len(),
        body,
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write to gvproxy API: {e}"))?;

    // Read just enough of the response to get the status line.
    let mut buf = [0u8; 1024];
    let n = stream
        .read(&mut buf)
        .map_err(|e| format!("read from gvproxy API: {e}"))?;
    let response = String::from_utf8_lossy(&buf[..n]);

    // Parse the HTTP status code from the first line (e.g. "HTTP/1.1 200 OK").
    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("0");

    match status {
        "200" | "204" => Ok(()),
        _ => {
            let first_line = response.lines().next().unwrap_or("<empty>");
            Err(format!("gvproxy API: {first_line}"))
        }
    }
}

/// Kill any stale gvproxy process from a previous gateway run.
///
/// If the CLI crashes or is killed before cleanup, gvproxy keeps running
/// and holds port 2222. A new gvproxy instance then fails with
/// "bind: address already in use".
fn kill_stale_gvproxy() {
    let output = std::process::Command::new("pkill")
        .args(["-x", "gvproxy"])
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            eprintln!("Killed stale gvproxy process");
            // Brief pause for the port to be released.
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}

fn path_to_cstring(path: &Path) -> Result<CString, VmError> {
    let s = path
        .to_str()
        .ok_or_else(|| VmError::InvalidPath(path.display().to_string()))?;
    Ok(CString::new(s)?)
}

// ── Launch ──────────────────────────────────────────────────────────────

/// Configure and launch a libkrun microVM.
///
/// This forks the process. The child enters the VM (never returns); the
/// parent blocks until the VM exits or a signal is received.
///
/// Returns the VM exit code (from `waitpid`).
#[allow(clippy::similar_names)]
pub fn launch(config: &VmConfig) -> Result<i32, VmError> {
    // Validate rootfs
    if !config.rootfs.is_dir() {
        return Err(VmError::RootfsNotFound {
            path: config.rootfs.display().to_string(),
        });
    }

    let launch_start = Instant::now();
    eprintln!("rootfs: {}", config.rootfs.display());
    eprintln!("vm: {} vCPU(s), {} MiB RAM", config.vcpus, config.mem_mib);

    // Ensure libkrunfw is discoverable. On macOS, dyld only reads
    // DYLD_FALLBACK_LIBRARY_PATH at startup, so if it's not set we
    // re-exec ourselves with it configured (this call won't return).
    ensure_krunfw_path()?;

    // ── Configure the microVM ──────────────────────────────────────

    unsafe {
        check(
            ffi::krun_set_log_level(config.log_level),
            "krun_set_log_level",
        )?;
    }

    let ctx_id = unsafe { ffi::krun_create_ctx() };
    if ctx_id < 0 {
        return Err(VmError::Krun {
            func: "krun_create_ctx",
            code: ctx_id,
        });
    }
    #[allow(clippy::cast_sign_loss)]
    let ctx_id = ctx_id as u32;

    unsafe {
        check(
            ffi::krun_set_vm_config(ctx_id, config.vcpus, config.mem_mib),
            "krun_set_vm_config",
        )?;
    }

    // Root filesystem (virtio-fs)
    let rootfs_c = path_to_cstring(&config.rootfs)?;
    unsafe {
        check(
            ffi::krun_set_root(ctx_id, rootfs_c.as_ptr()),
            "krun_set_root",
        )?;
    }

    // Working directory
    let workdir_c = CString::new(config.workdir.as_str())?;
    unsafe {
        check(
            ffi::krun_set_workdir(ctx_id, workdir_c.as_ptr()),
            "krun_set_workdir",
        )?;
    }

    // Networking setup
    let mut gvproxy_child: Option<std::process::Child> = None;
    let mut gvproxy_api_sock: Option<PathBuf> = None;

    match &config.net {
        NetBackend::Tsi => {
            // Default TSI — no special setup needed.
        }
        NetBackend::None => {
            unsafe {
                check(
                    ffi::krun_disable_implicit_vsock(ctx_id),
                    "krun_disable_implicit_vsock",
                )?;
                check(ffi::krun_add_vsock(ctx_id, 0), "krun_add_vsock")?;
            }
            eprintln!("Networking: disabled (no TSI, no virtio-net)");
        }
        NetBackend::Gvproxy { binary } => {
            if !binary.exists() {
                return Err(VmError::BinaryNotFound {
                    path: binary.display().to_string(),
                    hint: "Install Podman Desktop or place gvproxy in PATH".to_string(),
                });
            }

            // Create temp socket paths
            let run_dir = config
                .rootfs
                .parent()
                .unwrap_or(&config.rootfs)
                .to_path_buf();
            let vfkit_sock = run_dir.join("gvproxy-vfkit.sock");
            let api_sock = run_dir.join("gvproxy-api.sock");

            // Kill any stale gvproxy process from a previous run.
            // If gvproxy is still holding port 2222, the new instance
            // will fail with "bind: address already in use".
            kill_stale_gvproxy();

            // Clean stale sockets (including the -krun.sock file that
            // libkrun creates as its datagram endpoint).
            let _ = std::fs::remove_file(&vfkit_sock);
            let _ = std::fs::remove_file(&api_sock);
            let krun_sock = run_dir.join("gvproxy-vfkit.sock-krun.sock");
            let _ = std::fs::remove_file(&krun_sock);

            // Start gvproxy
            eprintln!("Starting gvproxy: {}", binary.display());
            let gvproxy_log = run_dir.join("gvproxy.log");
            let gvproxy_log_file = std::fs::File::create(&gvproxy_log)
                .map_err(|e| VmError::Fork(format!("failed to create gvproxy log: {e}")))?;
            let child = std::process::Command::new(binary)
                .arg("-listen-vfkit")
                .arg(format!("unixgram://{}", vfkit_sock.display()))
                .arg("-listen")
                .arg(format!("unix://{}", api_sock.display()))
                .stdout(std::process::Stdio::null())
                .stderr(gvproxy_log_file)
                .spawn()
                .map_err(|e| VmError::Fork(format!("failed to start gvproxy: {e}")))?;

            eprintln!(
                "gvproxy started (pid {}) [{:.1}s]",
                child.id(),
                launch_start.elapsed().as_secs_f64()
            );

            // Wait for the socket to appear (exponential backoff: 5ms → 100ms).
            {
                let deadline = Instant::now() + std::time::Duration::from_secs(5);
                let mut interval = std::time::Duration::from_millis(5);
                while !vfkit_sock.exists() {
                    if Instant::now() >= deadline {
                        return Err(VmError::Fork(
                            "gvproxy socket did not appear within 5s".to_string(),
                        ));
                    }
                    std::thread::sleep(interval);
                    interval = (interval * 2).min(std::time::Duration::from_millis(100));
                }
            }

            // Disable implicit TSI and add virtio-net via gvproxy
            unsafe {
                check(
                    ffi::krun_disable_implicit_vsock(ctx_id),
                    "krun_disable_implicit_vsock",
                )?;
                check(ffi::krun_add_vsock(ctx_id, 0), "krun_add_vsock")?;
            }

            let sock_c = path_to_cstring(&vfkit_sock)?;
            // This MAC matches gvproxy's default static DHCP lease for
            // 192.168.127.2. Using a different MAC can cause the gVisor
            // network stack to misroute or drop packets.
            let mac: [u8; 6] = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0xee];

            // COMPAT_NET_FEATURES from libkrun.h
            const NET_FEATURE_CSUM: u32 = 1 << 0;
            const NET_FEATURE_GUEST_CSUM: u32 = 1 << 1;
            const NET_FEATURE_GUEST_TSO4: u32 = 1 << 7;
            const NET_FEATURE_GUEST_UFO: u32 = 1 << 10;
            const NET_FEATURE_HOST_TSO4: u32 = 1 << 11;
            const NET_FEATURE_HOST_UFO: u32 = 1 << 14;
            const COMPAT_NET_FEATURES: u32 = NET_FEATURE_CSUM
                | NET_FEATURE_GUEST_CSUM
                | NET_FEATURE_GUEST_TSO4
                | NET_FEATURE_GUEST_UFO
                | NET_FEATURE_HOST_TSO4
                | NET_FEATURE_HOST_UFO;
            const NET_FLAG_VFKIT: u32 = 1 << 0;

            unsafe {
                check(
                    ffi::krun_add_net_unixgram(
                        ctx_id,
                        sock_c.as_ptr(),
                        -1,
                        mac.as_ptr(),
                        COMPAT_NET_FEATURES,
                        NET_FLAG_VFKIT,
                    ),
                    "krun_add_net_unixgram",
                )?;
            }

            eprintln!(
                "Networking: gvproxy (virtio-net) [{:.1}s]",
                launch_start.elapsed().as_secs_f64()
            );
            gvproxy_child = Some(child);
            gvproxy_api_sock = Some(api_sock);
        }
    }

    // Port mapping (TSI only)
    if !config.port_map.is_empty() && matches!(config.net, NetBackend::Tsi) {
        let port_strs: Vec<&str> = config.port_map.iter().map(String::as_str).collect();
        let (_port_owners, port_ptrs) = c_string_array(&port_strs)?;
        unsafe {
            check(
                ffi::krun_set_port_map(ctx_id, port_ptrs.as_ptr()),
                "krun_set_port_map",
            )?;
        }
    }

    // Console output
    let console_log = config.console_output.clone().unwrap_or_else(|| {
        config
            .rootfs
            .parent()
            .unwrap_or(&config.rootfs)
            .join("console.log")
    });
    let console_c = path_to_cstring(&console_log)?;
    unsafe {
        check(
            ffi::krun_set_console_output(ctx_id, console_c.as_ptr()),
            "krun_set_console_output",
        )?;
    }

    // Executable, argv, envp
    let exec_c = CString::new(config.exec_path.as_str())?;

    // argv: libkrun's init sets argv[0] from exec_path internally,
    // so we only pass the actual arguments here.
    let argv_strs: Vec<&str> = config.args.iter().map(String::as_str).collect();
    let (_argv_owners, argv_ptrs) = c_string_array(&argv_strs)?;

    // envp: use provided env or minimal defaults
    let env_strs: Vec<&str> = if config.env.is_empty() {
        vec![
            "HOME=/root",
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "TERM=xterm",
        ]
    } else {
        config.env.iter().map(String::as_str).collect()
    };
    let (_env_owners, env_ptrs) = c_string_array(&env_strs)?;

    unsafe {
        check(
            ffi::krun_set_exec(
                ctx_id,
                exec_c.as_ptr(),
                argv_ptrs.as_ptr(),
                env_ptrs.as_ptr(),
            ),
            "krun_set_exec",
        )?;
    }

    // ── Fork and enter the VM ──────────────────────────────────────
    //
    // krun_start_enter() never returns — it calls exit() when the guest
    // process exits. We fork so the parent can monitor and report.

    let boot_start = Instant::now();
    eprintln!("Booting microVM...");

    let pid = unsafe { libc::fork() };
    match pid {
        -1 => Err(VmError::Fork(std::io::Error::last_os_error().to_string())),
        0 => {
            // Child process: enter the VM (never returns on success)
            let ret = unsafe { ffi::krun_start_enter(ctx_id) };
            eprintln!("krun_start_enter failed: {ret}");
            std::process::exit(1);
        }
        _ => {
            // Parent: wait for child
            eprintln!(
                "VM started (child pid {pid}) [{:.1}s]",
                boot_start.elapsed().as_secs_f64()
            );
            for pm in &config.port_map {
                let host_port = pm.split(':').next().unwrap_or(pm);
                eprintln!("  port {pm} -> http://localhost:{host_port}");
            }
            eprintln!("Console output: {}", console_log.display());

            // Set up gvproxy port forwarding via its HTTP API.
            // The port_map entries use the same "host:guest" format
            // as TSI, but here we translate them into gvproxy expose
            // calls targeting the guest IP (192.168.127.2).
            //
            // Instead of a fixed 500ms sleep, poll the API socket with
            // exponential backoff (5ms → 200ms, ~1s total budget).
            if let Some(ref api_sock) = gvproxy_api_sock {
                let fwd_start = Instant::now();
                // Wait for the API socket to appear (it lags slightly
                // behind the vfkit data socket).
                {
                    let deadline = Instant::now() + std::time::Duration::from_secs(2);
                    let mut interval = std::time::Duration::from_millis(5);
                    while !api_sock.exists() {
                        if Instant::now() >= deadline {
                            eprintln!(
                                "warning: gvproxy API socket not ready after 2s, attempting anyway"
                            );
                            break;
                        }
                        std::thread::sleep(interval);
                        interval = (interval * 2).min(std::time::Duration::from_millis(200));
                    }
                }

                let guest_ip = "192.168.127.2";

                for pm in &config.port_map {
                    let parts: Vec<&str> = pm.split(':').collect();
                    let (host_port, guest_port) = match parts.len() {
                        2 => (parts[0], parts[1]),
                        1 => (parts[0], parts[0]),
                        _ => {
                            eprintln!("  skipping invalid port mapping: {pm}");
                            continue;
                        }
                    };

                    let expose_body = format!(
                        r#"{{"local":":{host_port}","remote":"{guest_ip}:{guest_port}","protocol":"tcp"}}"#
                    );

                    match gvproxy_expose(api_sock, &expose_body) {
                        Ok(()) => {
                            eprintln!("  port {host_port} -> {guest_ip}:{guest_port}");
                        }
                        Err(e) => {
                            eprintln!("  port {host_port}: {e}");
                        }
                    }
                }
                eprintln!(
                    "Port forwarding ready [{:.1}s]",
                    fwd_start.elapsed().as_secs_f64()
                );
            }

            // Wait for k3s kubeconfig to appear (virtio-fs makes it
            // visible on the host). Only do this for the gateway preset
            // (when exec_path is the default init script).
            if config.exec_path == "/srv/gateway-init.sh" {
                let kubeconfig_src = config.rootfs.join("etc/rancher/k3s/k3s.yaml");
                let kc_start = Instant::now();
                eprintln!("Waiting for kubeconfig...");

                // Aggressive polling initially (100ms) then back off to 1s.
                // Total budget: ~90s (enough for k3s cold start).
                let mut found = false;
                let deadline = Instant::now() + std::time::Duration::from_secs(90);
                let mut interval = std::time::Duration::from_millis(100);
                while Instant::now() < deadline {
                    if kubeconfig_src.is_file()
                        && std::fs::metadata(&kubeconfig_src)
                            .map(|m| m.len() > 0)
                            .unwrap_or(false)
                    {
                        found = true;
                        break;
                    }
                    std::thread::sleep(interval);
                    interval = (interval * 2).min(std::time::Duration::from_secs(1));
                }

                if found {
                    eprintln!(
                        "Kubeconfig appeared [{:.1}s]",
                        kc_start.elapsed().as_secs_f64()
                    );
                    // Copy kubeconfig to ~/.kube/gateway.yaml, rewriting
                    // the server URL to point at the forwarded host port.
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                    let kube_dir = PathBuf::from(&home).join(".kube");
                    let _ = std::fs::create_dir_all(&kube_dir);
                    let dest = kube_dir.join("gateway.yaml");

                    match std::fs::read_to_string(&kubeconfig_src) {
                        Ok(contents) => {
                            // The kubeconfig has server: https://127.0.0.1:6443
                            // which is correct since we forward host:6443 -> guest:6444.
                            if let Err(e) = std::fs::write(&dest, &contents) {
                                eprintln!("  failed to write kubeconfig: {e}");
                            } else {
                                eprintln!("Kubeconfig: {}", dest.display());
                                eprintln!("  export KUBECONFIG={}", dest.display());
                            }
                        }
                        Err(e) => {
                            eprintln!("  failed to read kubeconfig: {e}");
                        }
                    }

                    // Bootstrap the NemoClaw control plane: generate PKI,
                    // create TLS secrets, and store cluster metadata so CLI
                    // clients and e2e tests can connect.
                    //
                    // If the rootfs has pre-baked PKI (from build-rootfs.sh),
                    // this skips the namespace wait and kubectl apply entirely.
                    if let Err(e) = bootstrap_gateway(&dest, &config.rootfs) {
                        eprintln!("Bootstrap failed: {e}");
                        eprintln!("  The VM is running but NemoClaw may not be fully operational.");
                    }
                } else {
                    eprintln!("  kubeconfig not found after 90s (k3s may still be starting)");
                }

                // On warm reboots (rootfs persists via virtio-fs), the k3s
                // database may have stale pod records from the previous
                // session. containerd v2 doesn't always recover these
                // automatically. Force-delete any pods stuck in Unknown
                // or failed state so the StatefulSet controller recreates
                // them.
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                let kubeconfig_dest = PathBuf::from(&home).join(".kube/gateway.yaml");
                recover_stale_pods(&kubeconfig_dest);

                // Wait for the gRPC service to be reachable before
                // declaring "Ready". The navigator pod needs a few
                // seconds after k3s starts to bind its port.
                wait_for_gateway_service();
            }

            eprintln!("Ready [{:.1}s total]", boot_start.elapsed().as_secs_f64());
            eprintln!("Press Ctrl+C to stop.");

            // Forward signals to child
            unsafe {
                libc::signal(
                    libc::SIGINT,
                    forward_signal as *const () as libc::sighandler_t,
                );
                libc::signal(
                    libc::SIGTERM,
                    forward_signal as *const () as libc::sighandler_t,
                );
                CHILD_PID.store(pid, std::sync::atomic::Ordering::Relaxed);
            }

            let mut status: libc::c_int = 0;
            unsafe {
                libc::waitpid(pid, &raw mut status, 0);
            }

            // Clean up gvproxy
            if let Some(mut child) = gvproxy_child {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("gvproxy stopped");
            }

            if libc::WIFEXITED(status) {
                let code = libc::WEXITSTATUS(status);
                eprintln!("VM exited with code {code}");
                return Ok(code);
            } else if libc::WIFSIGNALED(status) {
                let sig = libc::WTERMSIG(status);
                eprintln!("VM killed by signal {sig}");
                return Ok(128 + sig);
            }

            Ok(status)
        }
    }
}

// ── Post-boot bootstrap ────────────────────────────────────────────────

/// Cluster name used for metadata and mTLS storage.
const GATEWAY_CLUSTER_NAME: &str = "gateway";

/// Gateway port: the host port mapped to the navigator `NodePort` (30051).
const GATEWAY_PORT: u16 = 30051;

/// Bootstrap the `NemoClaw` control plane after k3s is ready.
///
/// Three paths, fastest first:
///
/// 1. **Pre-baked PKI** (from `build-rootfs.sh`): reads PEM files directly
///    from the rootfs, stores creds + metadata on the host. No cluster
///    interaction at all. Completes in <50ms.
///
/// 2. **Warm boot**: host-side metadata + mTLS certs survive across VM
///    restarts. Waits for the navigator namespace, then returns.
///
/// 3. **Cold boot**: generates fresh PKI, waits for namespace, applies
///    secrets via kubectl, stores everything on the host.
fn bootstrap_gateway(kubeconfig: &Path, rootfs: &Path) -> Result<(), VmError> {
    let bootstrap_start = Instant::now();

    // Build cluster metadata early — it only depends on knowing the port and
    // cluster name, not on the cluster being ready.
    let metadata = navigator_bootstrap::ClusterMetadata {
        name: GATEWAY_CLUSTER_NAME.to_string(),
        gateway_endpoint: format!("https://127.0.0.1:{GATEWAY_PORT}"),
        is_remote: false,
        gateway_port: GATEWAY_PORT,
        kube_port: Some(6443),
        remote_host: None,
        resolved_host: None,
    };

    // ── Path 1: Pre-baked PKI from build-rootfs.sh ─────────────────
    //
    // If the rootfs was pre-initialized, PKI files are baked into
    // /opt/navigator/pki/. Read them directly — no cluster interaction
    // needed. The TLS secrets already exist inside the cluster from
    // the build-time k3s boot.
    let pki_dir = rootfs.join("opt/navigator/pki");
    if pki_dir.join("ca.crt").is_file() {
        eprintln!("Pre-baked PKI detected — fast bootstrap");

        let read = |name: &str| -> Result<String, VmError> {
            std::fs::read_to_string(pki_dir.join(name))
                .map_err(|e| VmError::Bootstrap(format!("failed to read {name}: {e}")))
        };

        let pki_bundle = navigator_bootstrap::pki::PkiBundle {
            ca_cert_pem: read("ca.crt")?,
            ca_key_pem: read("ca.key")?,
            server_cert_pem: read("server.crt")?,
            server_key_pem: read("server.key")?,
            client_cert_pem: read("client.crt")?,
            client_key_pem: read("client.key")?,
        };

        // Store metadata and credentials on the host.
        navigator_bootstrap::store_cluster_metadata(GATEWAY_CLUSTER_NAME, &metadata)
            .map_err(|e| VmError::Bootstrap(format!("failed to store metadata: {e}")))?;

        navigator_bootstrap::mtls::store_pki_bundle(GATEWAY_CLUSTER_NAME, &pki_bundle)
            .map_err(|e| VmError::Bootstrap(format!("failed to store mTLS creds: {e}")))?;

        navigator_bootstrap::save_active_cluster(GATEWAY_CLUSTER_NAME)
            .map_err(|e| VmError::Bootstrap(format!("failed to set active cluster: {e}")))?;

        eprintln!(
            "Bootstrap complete [{:.1}s]",
            bootstrap_start.elapsed().as_secs_f64()
        );
        eprintln!("  Cluster:  {GATEWAY_CLUSTER_NAME}");
        eprintln!("  Gateway:  https://127.0.0.1:{GATEWAY_PORT}");
        eprintln!("  mTLS:     ~/.config/nemoclaw/clusters/{GATEWAY_CLUSTER_NAME}/mtls/");
        return Ok(());
    }

    // ── Path 2: Warm boot ──────────────────────────────────────────
    //
    // Host-side metadata + mTLS certs survive from a previous boot.
    // Just wait for the namespace to confirm k3s is ready.
    let kc = kubeconfig
        .to_str()
        .ok_or_else(|| VmError::InvalidPath(kubeconfig.display().to_string()))?;

    if is_warm_boot() {
        eprintln!("Warm boot detected — reusing existing PKI and metadata.");
        eprintln!("Waiting for navigator namespace...");
        wait_for_namespace(kc)?;
        eprintln!(
            "Warm boot ready [{:.1}s]",
            bootstrap_start.elapsed().as_secs_f64()
        );
        eprintln!("  Cluster:  {GATEWAY_CLUSTER_NAME}");
        eprintln!("  Gateway:  https://127.0.0.1:{GATEWAY_PORT}");
        eprintln!("  mTLS:     ~/.config/nemoclaw/clusters/{GATEWAY_CLUSTER_NAME}/mtls/");
        return Ok(());
    }

    // ── Path 3: Cold boot (no pre-baked state) ─────────────────────
    eprintln!("Generating TLS certificates...");
    let pki_bundle = navigator_bootstrap::pki::generate_pki(&[])
        .map_err(|e| VmError::Bootstrap(format!("PKI generation failed: {e}")))?;

    navigator_bootstrap::store_cluster_metadata(GATEWAY_CLUSTER_NAME, &metadata)
        .map_err(|e| VmError::Bootstrap(format!("failed to store cluster metadata: {e}")))?;

    let ns_start = Instant::now();
    eprintln!("Waiting for navigator namespace...");
    wait_for_namespace(kc)?;
    eprintln!("Namespace ready [{:.1}s]", ns_start.elapsed().as_secs_f64());

    eprintln!("Creating TLS secrets...");
    apply_tls_secrets(kc, &pki_bundle)?;

    navigator_bootstrap::mtls::store_pki_bundle(GATEWAY_CLUSTER_NAME, &pki_bundle)
        .map_err(|e| VmError::Bootstrap(format!("failed to store mTLS credentials: {e}")))?;

    navigator_bootstrap::save_active_cluster(GATEWAY_CLUSTER_NAME)
        .map_err(|e| VmError::Bootstrap(format!("failed to set active cluster: {e}")))?;

    eprintln!(
        "Bootstrap complete [{:.1}s]",
        bootstrap_start.elapsed().as_secs_f64()
    );
    eprintln!("  Cluster:  {GATEWAY_CLUSTER_NAME}");
    eprintln!("  Gateway:  https://127.0.0.1:{GATEWAY_PORT}");
    eprintln!("  mTLS:     ~/.config/nemoclaw/clusters/{GATEWAY_CLUSTER_NAME}/mtls/");

    Ok(())
}

/// Check whether a previous bootstrap left valid state on disk.
///
/// A warm boot is detected when both:
/// - Cluster metadata exists: `$XDG_CONFIG_HOME/nemoclaw/clusters/gateway_metadata.json`
/// - mTLS certs exist: `$XDG_CONFIG_HOME/nemoclaw/clusters/gateway/mtls/{ca.crt,tls.crt,tls.key}`
///
/// When true, the host-side bootstrap (PKI generation, kubectl apply, metadata
/// storage) can be skipped because the virtio-fs rootfs persists k3s state
/// (TLS certs, kine/sqlite, containerd images, helm releases) across VM restarts.
fn is_warm_boot() -> bool {
    let Ok(home) = std::env::var("HOME") else {
        return false;
    };

    let config_base =
        std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{home}/.config"));

    let config_dir = PathBuf::from(&config_base)
        .join("nemoclaw")
        .join("clusters");

    // Check metadata file.
    let metadata_path = config_dir.join(format!("{GATEWAY_CLUSTER_NAME}_metadata.json"));
    if !metadata_path.is_file() {
        return false;
    }

    // Check mTLS cert files.
    let mtls_dir = config_dir.join(GATEWAY_CLUSTER_NAME).join("mtls");
    for name in &["ca.crt", "tls.crt", "tls.key"] {
        let path = mtls_dir.join(name);
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() && m.len() > 0 => {}
            _ => return false,
        }
    }

    true
}

/// Wait for the navigator pod to become Ready inside the k3s cluster
/// and verify the gRPC service is reachable from the host.
///
/// Stale pod/lease records are cleaned from the kine DB at build time
/// (see `build-rootfs.sh`). Containerd metadata (meta.db) is preserved
/// across boots so the native snapshotter doesn't re-extract image layers.
/// Runtime task state is cleaned by `gateway-init.sh` on each boot.
///
/// We poll kubectl for `Ready=True`, then verify with a host-side TCP
/// probe to `127.0.0.1:30051` to confirm the full gvproxy->VM->pod
/// path works. gvproxy accepts TCP connections even when nothing listens
/// in the guest, but those connections reset immediately. A connection
/// that stays open (server waiting for TLS `ClientHello`) proves the pod
/// is genuinely serving.
fn wait_for_gateway_service() {
    let start = Instant::now();
    let timeout = std::time::Duration::from_secs(90);
    let poll_interval = std::time::Duration::from_secs(1);

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let kubeconfig = PathBuf::from(&home).join(".kube/gateway.yaml");
    let kc = kubeconfig.to_string_lossy();

    eprintln!("Waiting for gateway service...");

    loop {
        // Check if the pod is Ready.
        let is_ready = std::process::Command::new("kubectl")
            .args(["--kubeconfig", &kc])
            .args([
                "-n",
                "navigator",
                "get",
                "pod",
                "navigator-0",
                "-o",
                "jsonpath={.status.conditions[?(@.type==\"Ready\")].status}",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .is_some_and(|s| s == "True");

        if is_ready {
            // Pod reports Ready — verify with a host-side TCP probe to
            // confirm the full gvproxy -> VM -> pod path works.
            if host_tcp_probe() {
                eprintln!("Service healthy [{:.1}s]", start.elapsed().as_secs_f64());
                return;
            }
            eprintln!("  pod Ready but host TCP probe failed, retrying...");
        }

        if start.elapsed() >= timeout {
            eprintln!(
                "  gateway service not ready after {:.0}s, continuing anyway",
                timeout.as_secs_f64()
            );
            return;
        }

        std::thread::sleep(poll_interval);
    }
}

/// Force-delete pods stuck in `Unknown` or failed states (safety net).
///
/// On warm reboots (virtio-fs persists rootfs across VM restarts), the
/// k3s database retains pod records from the previous session. Containerd
/// runtime task state is cleaned but metadata (meta.db) is preserved to
/// avoid re-extracting image layers. This function is a safety net for
/// edge cases where reconciliation fails — it force-deletes pods in
/// `Unknown` or `Failed` state so controllers can recreate them.
fn recover_stale_pods(kubeconfig: &Path) {
    let kc = kubeconfig.to_string_lossy();

    // Wait briefly for the API server to be responsive.
    let deadline = Instant::now() + std::time::Duration::from_secs(30);
    let mut interval = std::time::Duration::from_millis(500);
    loop {
        if let Ok(output) = std::process::Command::new("kubectl")
            .args(["--kubeconfig", &kc])
            .args(["get", "nodes", "-o", "name"])
            .output()
        {
            if output.status.success() {
                break;
            }
        }
        if Instant::now() >= deadline {
            eprintln!("  API server not ready after 30s, skipping pod recovery");
            return;
        }
        std::thread::sleep(interval);
        interval = (interval * 2).min(std::time::Duration::from_secs(2));
    }

    // Get all pods in a parseable format: namespace/name status
    let output = std::process::Command::new("kubectl")
        .args(["--kubeconfig", &kc])
        .args([
            "get", "pods", "-A",
            "-o", "jsonpath={range .items[*]}{.metadata.namespace}/{.metadata.name} {.status.phase}\\n{end}",
        ])
        .output();

    let Ok(output) = output else { return };
    if !output.status.success() {
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut stale_count = 0u32;

    for line in stdout.lines() {
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        if parts.len() != 2 {
            continue;
        }
        let (ns_name, phase) = (parts[0], parts[1]);
        // Delete pods in Unknown or Failed state — they can't recover
        // from stale containerd sandbox state.
        if phase == "Unknown" || phase == "Failed" {
            let ns_and_name: Vec<&str> = ns_name.splitn(2, '/').collect();
            if ns_and_name.len() != 2 {
                continue;
            }
            let (ns, name) = (ns_and_name[0], ns_and_name[1]);
            let result = std::process::Command::new("kubectl")
                .args(["--kubeconfig", &kc])
                .args([
                    "-n",
                    ns,
                    "delete",
                    "pod",
                    name,
                    "--force",
                    "--grace-period=0",
                ])
                .output();

            if let Ok(r) = result {
                if r.status.success() {
                    stale_count += 1;
                }
            }
        }
    }

    if stale_count > 0 {
        eprintln!("Recovered {stale_count} stale pod(s)");
    }
}

/// Probe `127.0.0.1:30051` from the host to verify the full
/// gvproxy → VM → pod path is working.
///
/// gvproxy accepts TCP connections even when the guest port is closed,
/// but those connections are immediately reset. A server that is truly
/// listening will hold the connection open (waiting for a TLS
/// ClientHello). We exploit this: connect, then try a short read. If
/// the read **times out** the server is alive; if it returns an error
/// (reset/EOF) the server is down.
fn host_tcp_probe() -> bool {
    use std::io::Read;
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let addr: SocketAddr = ([127, 0, 0, 1], GATEWAY_PORT).into();
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
        return false;
    };

    // A short read timeout: if the server is alive it will wait for us
    // to send a TLS ClientHello, so the read will time out (= good).
    // If the connection resets or closes, the server is dead.
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok();
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            true // Timeout = server alive, waiting for ClientHello.
        }
        _ => false, // Reset, EOF, or unexpected data = not healthy.
    }
}

/// Poll kubectl until the `navigator` namespace exists.
///
/// Uses exponential backoff (500ms → 3s) to minimize latency when the
/// namespace appears quickly while avoiding kubectl spam.
fn wait_for_namespace(kubeconfig: &str) -> Result<(), VmError> {
    let start = Instant::now();
    let timeout = std::time::Duration::from_secs(180);
    let mut interval = std::time::Duration::from_millis(500);
    let mut attempts = 0u32;

    loop {
        let output = std::process::Command::new("kubectl")
            .args(["--kubeconfig", kubeconfig])
            .args(["get", "namespace", "navigator", "-o", "name"])
            .output();

        if let Ok(output) = output
            && output.status.success()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.contains("navigator") {
                return Ok(());
            }
        }

        if start.elapsed() >= timeout {
            return Err(VmError::Bootstrap(
                "timed out waiting for navigator namespace (180s). \
                 Check console.log for k3s errors."
                    .to_string(),
            ));
        }

        attempts += 1;
        if attempts.is_multiple_of(10) {
            eprintln!(
                "  still waiting for navigator namespace ({:.0}s elapsed)",
                start.elapsed().as_secs_f64()
            );
        }

        std::thread::sleep(interval);
        interval = (interval * 2).min(std::time::Duration::from_secs(3));
    }
}

/// Apply the three TLS K8s secrets required by the `NemoClaw` server.
///
/// Uses `kubectl apply -f -` on the host, piping JSON manifests via stdin.
fn apply_tls_secrets(
    kubeconfig: &str,
    bundle: &navigator_bootstrap::pki::PkiBundle,
) -> Result<(), VmError> {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    let secrets = [
        // 1. navigator-server-tls (kubernetes.io/tls)
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": navigator_bootstrap::constants::SERVER_TLS_SECRET_NAME,
                "namespace": "navigator"
            },
            "type": "kubernetes.io/tls",
            "data": {
                "tls.crt": STANDARD.encode(&bundle.server_cert_pem),
                "tls.key": STANDARD.encode(&bundle.server_key_pem)
            }
        }),
        // 2. navigator-server-client-ca (Opaque)
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": navigator_bootstrap::constants::SERVER_CLIENT_CA_SECRET_NAME,
                "namespace": "navigator"
            },
            "type": "Opaque",
            "data": {
                "ca.crt": STANDARD.encode(&bundle.ca_cert_pem)
            }
        }),
        // 3. navigator-client-tls (Opaque) — shared by CLI and sandbox pods
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": navigator_bootstrap::constants::CLIENT_TLS_SECRET_NAME,
                "namespace": "navigator"
            },
            "type": "Opaque",
            "data": {
                "tls.crt": STANDARD.encode(&bundle.client_cert_pem),
                "tls.key": STANDARD.encode(&bundle.client_key_pem),
                "ca.crt": STANDARD.encode(&bundle.ca_cert_pem)
            }
        }),
    ];

    for secret in &secrets {
        let name = secret["metadata"]["name"].as_str().unwrap_or("unknown");
        kubectl_apply(kubeconfig, &secret.to_string())
            .map_err(|e| VmError::Bootstrap(format!("failed to create secret {name}: {e}")))?;
        eprintln!("  secret/{name} created");
    }

    Ok(())
}

/// Run `kubectl apply -f -` with the given manifest piped via stdin.
fn kubectl_apply(kubeconfig: &str, manifest: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("kubectl")
        .args(["--kubeconfig", kubeconfig, "apply", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn kubectl: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(manifest.as_bytes())
            .map_err(|e| format!("failed to write manifest to kubectl stdin: {e}"))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("failed to wait for kubectl: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("kubectl apply failed: {stderr}"));
    }

    Ok(())
}

static CHILD_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn forward_signal(_sig: libc::c_int) {
    let pid = CHILD_PID.load(std::sync::atomic::Ordering::Relaxed);
    if pid > 0 {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}
