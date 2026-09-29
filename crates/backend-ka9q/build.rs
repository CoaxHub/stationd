// SPDX-License-Identifier: GPL-3.0-only
//! Generates the ka9q-radio tag enums from the pinned, vendored headers in
//! `vendor/ka9q-radio/` into `$OUT_DIR/tags.rs` (included by `src/tags.rs`).

#[path = "src/codegen.rs"]
#[allow(dead_code)]
mod codegen;

use std::path::{Path, PathBuf};

const VENDOR_DIR: &str = "vendor/ka9q-radio";

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

fn run() -> Result<(), String> {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set")?;
    let out_dir = std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?;
    let vendor = PathBuf::from(manifest).join(VENDOR_DIR);

    println!("cargo:rerun-if-changed=src/codegen.rs");
    for file in ["status.h", "rtp.h", "VERSION"] {
        println!("cargo:rerun-if-changed={VENDOR_DIR}/{file}");
    }

    let status = read(&vendor.join("status.h"))?;
    let rtp = read(&vendor.join("rtp.h"))?;
    let version = codegen::parse_version_file(&read(&vendor.join("VERSION"))?)?;
    let generated = codegen::generate_all(&[("status.h", &status), ("rtp.h", &rtp)], &version)?;

    let dest = PathBuf::from(out_dir).join("tags.rs");
    std::fs::write(&dest, generated).map_err(|e| format!("cannot write {}: {e}", dest.display()))
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: backend-ka9q tag codegen failed: {e}");
        eprintln!(
            "hint: the vendored ka9q-radio headers changed shape; see scripts/update-ka9q-headers.sh"
        );
        std::process::exit(1);
    }
}
