// SPDX-License-Identifier: GPL-3.0-only
//! A packaged build (`--features packaged`): the domain and root are built
//! in, and it reads no settings file: no `--config`, no `check-config`.
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

use stationd::config::{BackendConfig, Config, TrustAnchor};
use stationd::flavour::{Flavour, PACKAGED_RUNTIME_DIR, RECEIVERS_DIR};
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
fn the_built_in_settings() {
    let c = Config::built_in(Flavour::current().built_in.expect("built in")).expect("config");
    let e = c.endpoints.clone().expect("endpoints");
    assert_eq!(e.enroll.url, format!("https://enroll.{DOMAIN}"));
    assert_eq!(e.station_api.url, format!("https://station-api.{DOMAIN}"));
    assert!(matches!(e.root, TrustAnchor::BuiltIn(_)));
    assert_eq!(c.state_dir, Path::new("/var/lib/coaxhub/stationd"));
    assert_eq!(c.runtime_dir, Path::new(PACKAGED_RUNTIME_DIR));
    assert_eq!(c.receivers_dir, Path::new(RECEIVERS_DIR));
    assert_eq!(c.backend, BackendConfig::Bundled);
    assert_eq!(c.usb_devices, Path::new("/sys/bus/usb/devices"));
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(BIN).args(args).output().expect("run")
}

fn text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn the_binary_takes_no_settings_file() {
    // No check-config, no --config: usage errors.
    let out = run(&["check-config"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let out = run(&["--config", "/nonexistent", "status"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let out = run(&["status", "--config", "/nonexistent"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    // backend-config refuses receiver 0, and one the applied set lacks.
    for id in ["0", "99"] {
        let out = run(&["backend-config", "--receiver", id, "--out", "-"]);
        assert_eq!(out.status.code(), Some(2), "{id}: {}", text(&out));
    }
    // Doctor explains a station that isn't enrolled.
    let out = run(&["doctor"]);
    let t = text(&out).to_lowercase();
    assert!(t.contains("packaged build: built-in settings"), "{t}");
    assert!(t.contains("not enrolled"), "{t}");
}
