// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Build script for navigator-gateway.
//!
//! Discovers the Homebrew library path for libkrun and emits the appropriate
//! cargo link-search directives. On macOS ARM64, libkrun is typically installed
//! via `brew tap slp/krun && brew install libkrun`.

fn main() {
    // Discover Homebrew prefix (handles both /opt/homebrew and /usr/local)
    let homebrew_prefix = std::process::Command::new("brew")
        .args(["--prefix"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "/opt/homebrew".to_string());

    let lib_dir = format!("{homebrew_prefix}/lib");

    println!("cargo:rustc-link-search=native={lib_dir}");
    println!("cargo:rustc-link-lib=dylib=krun");

    // Re-run if the library changes
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LIBRARY_PATH");
}
