// SPDX-License-Identifier: GPL-3.0-only
//! A packaged build (`--features packaged`): the domain
//! and root are built in, and every test-only setting is refused, by the
//! library and by the binary.
//!
//! ```sh
//! openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
//!     -keyout /dev/null -subj /CN=test-root -days 1 -out "$DIR/root.pem"
//! COAXHUB_DOMAIN=coaxhub.test COAXHUB_ROOT_CA="$DIR/root.pem" \
//!     cargo nextest run -p stationd --features packaged
//! ```
#![cfg(feature = "packaged")]

use std::path::Path;
use std::process::Command;

use stationd::config::{Config, ConfigError, TrustAnchor};
use stationd::flavour::{Flavour, PACKAGED_RUNTIME_DIR};
use stationd::tls;

const BIN: &str = env!("CARGO_BIN_EXE_stationd");
const DOMAIN: &str = env!("COAXHUB_DOMAIN");

#[test]
fn domain_and_root_are_built_in() {
    let f = Flavour::current();
    assert!(f.packaged);
    assert_eq!(f.name(), "packaged");
    let b = f.built_in.expect("built in");
    assert_eq!(b.domain, DOMAIN);
    let root = std::fs::read_to_string(env!("COAXHUB_ROOT_CA")).expect("root file");
    assert_eq!(b.root_ca_pem, root);
    assert!(!tls::parse_certs(b.root_ca_pem).expect("PEM").is_empty());
    assert_eq!(
        f.templates_dir(),
        Path::new("/usr/share/coaxhub/radiod-templates")
    );
}

#[test]
fn owner_config_uses_the_built_in_endpoints() {
    let dir = tempfile::tempdir().expect("tmp");
    let c = Config::parse("[sdr]\nprofile = \"rtlsdr-hf\"\n", dir.path()).expect("valid");
    let e = c.endpoints.clone().expect("endpoints");
    assert_eq!(e.enroll.url, format!("https://enroll.{DOMAIN}"));
    assert_eq!(e.station_api.url, format!("https://station-api.{DOMAIN}"));
    assert!(matches!(e.root, TrustAnchor::BuiltIn(_)));
    assert_eq!(c.state_dir, Path::new("/var/lib/coaxhub/stationd"));
    assert_eq!(c.runtime_dir, Path::new(PACKAGED_RUNTIME_DIR));
}

#[test]
fn test_only_settings_are_refused() {
    let dir = tempfile::tempdir().expect("tmp");
    for f in ["c.pem", "k.pem", "ca.pem"] {
        std::fs::write(dir.path().join(f), "x").expect("write");
    }
    let cases = [
        (
            "state_dir = \"/tmp/x\"\n[sdr]\nprofile = \"rtlsdr\"\n",
            "state_dir",
        ),
        (
            "[sdr]\nprofile = \"rtlsdr\"\n[endpoints]\nenroll_url = \"https://localhost:1\"\n\
             station_api_url = \"https://localhost:2\"\nroot_ca = \"ca.pem\"\n",
            "endpoints",
        ),
        (
            "[credentials]\ngateway_url = \"https://localhost:1\"\ncert = \"c.pem\"\n\
             key = \"k.pem\"\ngateway_ca = \"ca.pem\"\n[sdr]\nprofile = \"rtlsdr\"\n",
            "credentials",
        ),
        ("[backend]\nkind = \"fake\"\n", "backend"),
    ];
    for (text, field) in cases {
        match Config::parse(text, dir.path()) {
            Err(ConfigError::DevelopmentOnly { field: f }) => assert_eq!(f, field),
            other => panic!("{field}: {other:?}"),
        }
        // The same file is fine for a development build.
        assert!(
            Config::parse_as(text, dir.path(), Flavour::development()).is_ok(),
            "{field}"
        );
    }
    let e = Config::parse("[sdr]\nprofile = \"siggen\"\n", dir.path()).expect_err("dev only");
    assert!(
        e.to_string().contains("supported: rtlsdr, rtlsdr-hf"),
        "{e}"
    );
}

#[test]
fn the_binary_refuses_them_too() {
    let dir = tempfile::tempdir().expect("tmp");
    let config = dir.path().join("stationd.toml");
    std::fs::write(&config, "[backend]\nkind = \"fake\"\n").expect("write");
    let out = Command::new(BIN)
        .args(["check-config", "--config"])
        .arg(&config)
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("packaged builds of stationd do not accept it"),
        "{err}"
    );

    // An owner's config renders radiod's config from the templates.
    std::fs::write(&config, "[sdr]\nprofile = \"rtlsdr\"\n").expect("write");
    let templates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/radiod-templates");
    let out = Command::new(BIN)
        .args(["backend-config", "--out", "-", "--templates"])
        .arg(&templates)
        .arg("--config")
        .arg(&config)
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("frequency = 98000000"));
    // Not enrolled: run exits 3.
    let out = Command::new(BIN)
        .args(["check-config", "--config"])
        .arg(&config)
        .output()
        .expect("run");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("config OK (packaged build): not enrolled yet"),
        "{text}"
    );
}
