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
    /// Runs `/srv/gateway-init.sh` which mounts essential filesystems and
    /// execs `k3s server`. Exposes the Kubernetes API on port 6443.
    pub fn gateway(rootfs: PathBuf) -> Self {
        Self {
            rootfs,
            vcpus: 2,
            mem_mib: 2048,
            exec_path: "/srv/gateway-init.sh".to_string(),
            args: vec![],
            env: vec![
                "HOME=/root".to_string(),
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
                "TERM=xterm".to_string(),
            ],
            workdir: "/".to_string(),
            // Map host 6443 -> guest 6444 (real kube-apiserver).
            // The k3s dynamiclistener on 6443 has TLS issues through
            // port forwarding, so we go directly to the apiserver.
            port_map: vec!["6443:6444".to_string()],
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

            // Clean stale sockets
            let _ = std::fs::remove_file(&vfkit_sock);
            let _ = std::fs::remove_file(&api_sock);

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

            eprintln!("gvproxy started (pid {})", child.id());

            // Wait for the socket to appear
            for _ in 0..50 {
                if vfkit_sock.exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if !vfkit_sock.exists() {
                return Err(VmError::Fork(
                    "gvproxy socket did not appear within 5s".to_string(),
                ));
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

            eprintln!("Networking: gvproxy (virtio-net via {vfkit_sock:?})");
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
            eprintln!("VM started (child pid {pid})");
            for pm in &config.port_map {
                let host_port = pm.split(':').next().unwrap_or(pm);
                eprintln!("  port {pm} -> http://localhost:{host_port}");
            }
            eprintln!("Console output: {}", console_log.display());

            // Set up gvproxy port forwarding via its HTTP API.
            // The port_map entries use the same "host:guest" format
            // as TSI, but here we translate them into gvproxy expose
            // calls targeting the guest IP (192.168.127.2).
            if let Some(ref api_sock) = gvproxy_api_sock {
                // Wait for gvproxy API socket to be ready
                std::thread::sleep(std::time::Duration::from_millis(500));
                eprintln!("Setting up gvproxy port forwarding...");

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
            }

            // Wait for k3s kubeconfig to appear (virtio-fs makes it
            // visible on the host). Only do this for the gateway preset
            // (when exec_path is the default init script).
            if config.exec_path == "/srv/gateway-init.sh" {
                let kubeconfig_src = config.rootfs.join("etc/rancher/k3s/k3s.yaml");
                eprintln!("Waiting for kubeconfig...");
                let mut found = false;
                for _ in 0..60 {
                    if kubeconfig_src.is_file()
                        && std::fs::metadata(&kubeconfig_src)
                            .map(|m| m.len() > 0)
                            .unwrap_or(false)
                    {
                        found = true;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }

                if found {
                    // Copy kubeconfig to ~/.kube/gateway.yaml, rewriting
                    // the server URL to point at the forwarded host port.
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                    let kube_dir = PathBuf::from(home).join(".kube");
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
                } else {
                    eprintln!("  kubeconfig not found after 60s (k3s may still be starting)");
                }
            }

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

static CHILD_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn forward_signal(_sig: libc::c_int) {
    let pid = CHILD_PID.load(std::sync::atomic::Ordering::Relaxed);
    if pid > 0 {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}
