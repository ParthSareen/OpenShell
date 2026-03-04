// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Minimal FFI bindings for the libkrun C API.
//!
//! libkrun is a `cdylib` — it cannot be consumed as a Rust dependency. We link
//! against the Homebrew-installed system library and declare `extern "C"` for
//! the subset of functions we need.
//!
//! See: <https://github.com/containers/libkrun/blob/main/include/libkrun.h>

use libc::c_char;

#[link(name = "krun")]
#[allow(dead_code)]
unsafe extern "C" {
    /// Sets the log level for the library (0=Off .. 5=Trace).
    pub fn krun_set_log_level(level: u32) -> i32;

    /// Creates a configuration context. Returns context ID (>= 0) or negative error.
    pub fn krun_create_ctx() -> i32;

    /// Frees a configuration context.
    pub fn krun_free_ctx(ctx_id: u32) -> i32;

    /// Sets vCPUs and RAM (MiB) for the microVM.
    pub fn krun_set_vm_config(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> i32;

    /// Sets the root filesystem path (virtio-fs backed directory).
    pub fn krun_set_root(ctx_id: u32, root_path: *const c_char) -> i32;

    /// Sets the working directory inside the VM.
    pub fn krun_set_workdir(ctx_id: u32, workdir_path: *const c_char) -> i32;

    /// Sets the executable path, argv, and envp for the process inside the VM.
    ///
    /// **Important:** If `envp` is NULL, libkrun serializes the entire host
    /// environment into the kernel command line, which can overflow its 4096-byte
    /// limit. Always pass an explicit minimal env.
    pub fn krun_set_exec(
        ctx_id: u32,
        exec_path: *const c_char,
        argv: *const *const c_char,
        envp: *const *const c_char,
    ) -> i32;

    /// Configures host-to-guest TCP port mapping.
    ///
    /// Format: null-terminated array of `"host_port:guest_port"` C strings.
    /// Passing NULL auto-exposes all listening guest ports.
    pub fn krun_set_port_map(ctx_id: u32, port_map: *const *const c_char) -> i32;

    /// Redirects console output to a file (ignores stdin).
    pub fn krun_set_console_output(ctx_id: u32, filepath: *const c_char) -> i32;

    /// Starts and enters the microVM. **Never returns** on success — calls
    /// `exit()` with the workload's exit code. Only returns on config error.
    pub fn krun_start_enter(ctx_id: u32) -> i32;

    /// Disables the implicit vsock device. Must be called before
    /// `krun_add_vsock` to manually configure TSI features.
    pub fn krun_disable_implicit_vsock(ctx_id: u32) -> i32;

    /// Adds a vsock device with specified TSI features.
    ///
    /// `tsi_features` is a bitmask:
    ///   - `KRUN_TSI_HIJACK_INET` (1 << 0): intercept AF_INET sockets
    ///   - `KRUN_TSI_HIJACK_UNIX` (1 << 1): intercept AF_UNIX sockets
    ///   - 0: vsock without any TSI hijacking
    pub fn krun_add_vsock(ctx_id: u32, tsi_features: u32) -> i32;

    /// Adds a virtio-net device connected to a unixgram-based backend
    /// (e.g., gvproxy in vfkit mode).
    ///
    /// `c_path` and `fd` are mutually exclusive: set one to NULL/-1.
    /// `c_mac` is 6 bytes. `features` is virtio-net feature bitmask.
    /// `flags` may include `NET_FLAG_VFKIT` (1 << 0) for gvproxy vfkit mode.
    pub fn krun_add_net_unixgram(
        ctx_id: u32,
        c_path: *const c_char,
        fd: i32,
        c_mac: *const u8,
        features: u32,
        flags: u32,
    ) -> i32;
}
