// SPDX-License-Identifier: GPL-3.0-only
//! Build flavours of stationd.
//!
//! With the `packaged` feature, the `CoaxHub` domain and root certificate
//! are compiled into the binary:
//!
//! - `COAXHUB_DOMAIN`: the service domain; enrollment goes to
//!   `https://enroll.<domain>` and `StationControl` to
//!   `https://station-api.<domain>`.
//! - `COAXHUB_ROOT_CA`: the path of a PEM file holding the `CoaxHub` root
//!   certificate(s), the only trust anchor for enrollment.
//!
//! The build fails if either is missing or malformed. Development builds
//! (no `packaged` feature) embed nothing and ignore both variables.

use std::path::{Path, PathBuf};

const DOMAIN_ENV: &str = "COAXHUB_DOMAIN";
const ROOT_CA_ENV: &str = "COAXHUB_ROOT_CA";

fn main() -> Result<(), String> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={DOMAIN_ENV}");
    println!("cargo:rerun-if-env-changed={ROOT_CA_ENV}");
    if std::env::var_os("CARGO_FEATURE_PACKAGED").is_none() {
        return Ok(());
    }
    let domain = std::env::var(DOMAIN_ENV).map_err(|_| {
        format!("the `packaged` feature needs {DOMAIN_ENV} (the CoaxHub domain) at build time")
    })?;
    check_domain(&domain)?;
    let root_path = std::env::var_os(ROOT_CA_ENV).map(PathBuf::from).ok_or_else(|| {
        format!("the `packaged` feature needs {ROOT_CA_ENV} (path to the CoaxHub root CA PEM) at build time")
    })?;
    println!("cargo:rerun-if-changed={}", root_path.display());
    let pem = std::fs::read_to_string(&root_path)
        .map_err(|e| format!("{ROOT_CA_ENV}={}: {e}", root_path.display()))?;
    check_root_pem(&pem, &root_path)?;
    let out = PathBuf::from(std::env::var("OUT_DIR").map_err(|e| e.to_string())?);
    std::fs::write(out.join("coaxhub-root-ca.pem"), pem).map_err(|e| e.to_string())?;
    println!("cargo:rustc-env=STATIOND_BUILT_IN_DOMAIN={domain}");
    Ok(())
}

/// A lowercase DNS name with at least two labels.
fn check_domain(domain: &str) -> Result<(), String> {
    let labels: Vec<&str> = domain.split('.').collect();
    let label_ok = |l: &&str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if domain.len() > 200 || labels.len() < 2 || !labels.iter().all(label_ok) {
        return Err(format!(
            "{DOMAIN_ENV}={domain:?} is not a lowercase DNS name such as coaxhub.example"
        ));
    }
    Ok(())
}

/// At least one certificate and no private key.
fn check_root_pem(pem: &str, path: &Path) -> Result<(), String> {
    if !pem.contains("-----BEGIN CERTIFICATE-----") {
        return Err(format!(
            "{ROOT_CA_ENV}={}: no PEM certificate found",
            path.display()
        ));
    }
    if pem.contains("PRIVATE KEY") {
        return Err(format!(
            "{ROOT_CA_ENV}={}: the file holds a private key; embed the certificate only",
            path.display()
        ));
    }
    Ok(())
}
