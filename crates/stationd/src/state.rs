// SPDX-License-Identifier: GPL-3.0-only
//! The state directory (station-protocol §7): what `stationd enroll`
//! writes and `stationd run` reads.
//!
//! ```text
//! /var/lib/coaxhub/stationd/     0700, owned by the service user
//!   key.pem                      0600  the station's private key (PKCS#8 PEM)
//!   cert.pem                     0644  the station certificate (leaf)
//!   chain.pem                    0644  intermediates, leaf issuer first (may be empty)
//!   gateway-ca.pem               0644  roots for the gateway and station-api
//!   assignment.json              0644  {"hub_id": …, "gateway_url": …}
//!   station-id                   0644  the station UUID, for display
//! ```
//!
//! Every file is written atomically: a temporary file in the same
//! directory, `fsync`, `rename`, then `fsync` of the directory. A station
//! is enrolled when all six files are present and parseable (checked by
//! [`crate::credentials`]).

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The private key.
pub const KEY_FILE: &str = "key.pem";
/// The station certificate.
pub const CERT_FILE: &str = "cert.pem";
/// Its intermediates.
pub const CHAIN_FILE: &str = "chain.pem";
/// The gateway and station-api roots.
pub const GATEWAY_CA_FILE: &str = "gateway-ca.pem";
/// The home hub.
pub const ASSIGNMENT_FILE: &str = "assignment.json";
/// The station ID.
pub const STATION_ID_FILE: &str = "station-id";
/// The daemon's runtime status (in the runtime directory).
pub const STATUS_FILE: &str = "status.json";

/// Every credential file, in the order enrollment writes them.
pub const CREDENTIAL_FILES: [&str; 6] = [
    KEY_FILE,
    CERT_FILE,
    CHAIN_FILE,
    GATEWAY_CA_FILE,
    ASSIGNMENT_FILE,
    STATION_ID_FILE,
];

/// Mode of the state directory.
pub const DIR_MODE: u32 = 0o700;
/// Mode of the private key.
pub const KEY_MODE: u32 = 0o600;
/// Mode of the other files.
pub const PUBLIC_MODE: u32 = 0o644;

/// A state-directory I/O failure.
#[derive(Debug, thiserror::Error)]
#[error("{}: {source}", path.display())]
pub struct StateError {
    /// The file or directory.
    pub path: PathBuf,
    /// The I/O error.
    pub source: std::io::Error,
}

fn err(path: &Path) -> impl FnOnce(std::io::Error) -> StateError + '_ {
    move |source| StateError {
        path: path.to_owned(),
        source,
    }
}

/// The last known home hub (`assignment.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    /// The hub's id.
    pub hub_id: String,
    /// Its gateway, `https://gw.<hub>.<domain>[:port]`.
    pub gateway_url: String,
}

/// What enrollment stores.
#[derive(Debug)]
pub struct Enrollment<'a> {
    /// The station ID.
    pub station_id: Uuid,
    /// The private key, PEM.
    pub key_pem: &'a str,
    /// The leaf certificate, PEM.
    pub cert_pem: &'a str,
    /// The intermediates, PEM (may be empty).
    pub chain_pem: &'a str,
    /// The gateway CA bundle, PEM.
    pub gateway_ca_pem: &'a str,
    /// The home hub.
    pub assignment: &'a Assignment,
}

/// The state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    path: PathBuf,
}

impl StateDir {
    /// The state directory at `path` (not created).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A file in the directory.
    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Creates the directory (and parents) if needed and sets it to 0700.
    pub fn ensure(&self) -> Result<(), StateError> {
        ensure_private_dir(&self.path)
    }

    /// The credential files that exist.
    pub fn present(&self) -> Vec<&'static str> {
        CREDENTIAL_FILES
            .into_iter()
            .filter(|f| self.file(f).exists())
            .collect()
    }

    /// Writes every enrollment file atomically (the key 0600, the rest
    /// 0644), after creating the directory.
    pub fn write_enrollment(&self, e: &Enrollment<'_>) -> Result<(), StateError> {
        self.ensure()?;
        let assignment = serde_json::to_string_pretty(e.assignment)
            .map_err(|x| err(&self.file(ASSIGNMENT_FILE))(std::io::Error::other(x)))?;
        let station_id = e.station_id.to_string();
        let files: [(&str, &[u8], u32); 6] = [
            (KEY_FILE, e.key_pem.as_bytes(), KEY_MODE),
            (CERT_FILE, e.cert_pem.as_bytes(), PUBLIC_MODE),
            (CHAIN_FILE, e.chain_pem.as_bytes(), PUBLIC_MODE),
            (GATEWAY_CA_FILE, e.gateway_ca_pem.as_bytes(), PUBLIC_MODE),
            (ASSIGNMENT_FILE, assignment.as_bytes(), PUBLIC_MODE),
            (STATION_ID_FILE, station_id.as_bytes(), PUBLIC_MODE),
        ];
        for (name, contents, mode) in files {
            write_atomic(&self.file(name), contents, mode)?;
        }
        Ok(())
    }

    /// Reads `assignment.json`.
    pub fn assignment(&self) -> Result<Assignment, StateError> {
        let path = self.file(ASSIGNMENT_FILE);
        let text = fs::read_to_string(&path).map_err(err(&path))?;
        serde_json::from_str(&text).map_err(|e| StateError {
            path,
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        })
    }

    /// Reads `station-id`.
    pub fn station_id(&self) -> Result<Uuid, StateError> {
        let path = self.file(STATION_ID_FILE);
        let text = fs::read_to_string(&path).map_err(err(&path))?;
        Uuid::try_parse(text.trim()).map_err(|e| StateError {
            path,
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        })
    }

    /// Deletes every credential file (keeps the directory and anything
    /// else in it). Returns the files that were removed.
    pub fn remove_credentials(&self) -> Result<Vec<&'static str>, StateError> {
        let mut removed = Vec::new();
        for name in CREDENTIAL_FILES {
            let path = self.file(name);
            match fs::remove_file(&path) {
                Ok(()) => removed.push(name),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(err(&path)(e)),
            }
        }
        if !removed.is_empty() {
            sync_dir(&self.path)?;
        }
        Ok(removed)
    }
}

/// Creates `dir` (and parents) if needed and sets it to 0700.
pub fn ensure_private_dir(dir: &Path) -> Result<(), StateError> {
    fs::create_dir_all(dir).map_err(err(dir))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE)).map_err(err(dir))
}

/// Writes `contents` to `path` atomically with `mode`: a new temporary file
/// in the same directory (created with `mode`, never wider), `fsync`,
/// `rename` over `path`, `fsync` of the directory. On failure the
/// temporary file is removed and `path` is unchanged.
///
/// The file gets the directory's owner and group when they differ from the
/// caller's (root running `stationd enroll` for the service user's
/// directory).
pub fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<(), StateError> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{name}.tmp-{:016x}", rand::random::<u64>()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
            .map_err(err(&tmp))?;
        // The umask may have narrowed the mode; set exactly what we want.
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(err(&tmp))?;
        let dir_meta = fs::metadata(dir).map_err(err(dir))?;
        let file_meta = file.metadata().map_err(err(&tmp))?;
        if (file_meta.uid(), file_meta.gid()) != (dir_meta.uid(), dir_meta.gid()) {
            std::os::unix::fs::fchown(&file, Some(dir_meta.uid()), Some(dir_meta.gid()))
                .map_err(err(&tmp))?;
        }
        file.write_all(contents).map_err(err(&tmp))?;
        file.sync_all().map_err(err(&tmp))?;
        drop(file);
        fs::rename(&tmp, path).map_err(err(path))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
        return result;
    }
    sync_dir(dir)
}

fn sync_dir(dir: &Path) -> Result<(), StateError> {
    File::open(dir).and_then(|d| d.sync_all()).map_err(err(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).expect("stat").permissions().mode() & 0o777
    }

    fn sample(dir: &StateDir) -> Assignment {
        let a = Assignment {
            hub_id: "sa1".into(),
            gateway_url: "https://gw.sa1.coaxhub.example".into(),
        };
        dir.write_enrollment(&Enrollment {
            station_id: Uuid::from_u128(7),
            key_pem: "KEY",
            cert_pem: "CERT",
            chain_pem: "",
            gateway_ca_pem: "CA",
            assignment: &a,
        })
        .expect("write");
        a
    }

    #[test]
    fn enrollment_files_and_permissions() {
        let tmp = tempfile::tempdir().expect("tmp");
        // A parent that does not exist yet, with a loose mode afterwards.
        let dir = StateDir::new(tmp.path().join("var/stationd"));
        assert!(dir.present().is_empty());
        let a = sample(&dir);
        assert_eq!(mode(dir.path()), 0o700);
        assert_eq!(mode(&dir.file(KEY_FILE)), 0o600);
        for f in [
            CERT_FILE,
            CHAIN_FILE,
            GATEWAY_CA_FILE,
            ASSIGNMENT_FILE,
            STATION_ID_FILE,
        ] {
            assert_eq!(mode(&dir.file(f)), 0o644, "{f}");
        }
        assert_eq!(dir.present(), CREDENTIAL_FILES.to_vec());
        assert_eq!(dir.assignment().expect("assignment"), a);
        assert_eq!(dir.station_id().expect("id"), Uuid::from_u128(7));
        assert_eq!(fs::read_to_string(dir.file(KEY_FILE)).expect("key"), "KEY");
        // No temporary files are left behind.
        let names: Vec<String> = fs::read_dir(dir.path())
            .expect("ls")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 6, "{names:?}");
        // A loose directory is tightened again.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("chmod");
        dir.ensure().expect("ensure");
        assert_eq!(mode(dir.path()), 0o700);
        // Removal keeps other files.
        fs::write(dir.file(STATUS_FILE), "{}").expect("status");
        assert_eq!(
            dir.remove_credentials().expect("rm"),
            CREDENTIAL_FILES.to_vec()
        );
        assert!(dir.present().is_empty());
        assert!(dir.file(STATUS_FILE).exists());
        assert!(dir.remove_credentials().expect("rm again").is_empty());
    }

    #[test]
    fn atomic_write_replaces_and_never_widens() {
        let tmp = tempfile::tempdir().expect("tmp");
        let p = tmp.path().join("key.pem");
        write_atomic(&p, b"one", 0o600).expect("write");
        write_atomic(&p, b"two", 0o600).expect("overwrite");
        assert_eq!(fs::read(&p).expect("read"), b"two");
        assert_eq!(mode(&p), 0o600);
        // A read-only directory: the write fails and the old file stays.
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o500)).expect("chmod");
        let e = write_atomic(&p, b"three", 0o600);
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).expect("chmod");
        if e.is_ok() {
            // Running as root: permissions are not enforced.
            return;
        }
        assert_eq!(fs::read(&p).expect("read"), b"two");
        assert_eq!(fs::read_dir(tmp.path()).expect("ls").count(), 1);
    }

    #[test]
    fn corrupt_files_are_errors() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = StateDir::new(tmp.path());
        sample(&dir);
        fs::write(dir.file(ASSIGNMENT_FILE), "{\"hub_id\": 3}").expect("write");
        let e = dir.assignment().expect_err("bad json");
        assert!(e.to_string().contains("assignment.json"), "{e}");
        fs::write(dir.file(STATION_ID_FILE), "nope").expect("write");
        assert!(dir.station_id().is_err());
        fs::remove_file(dir.file(STATION_ID_FILE)).expect("rm");
        assert_eq!(
            dir.station_id().expect_err("missing").source.kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn directory_that_is_a_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let file = tmp.path().join("f");
        fs::write(&file, "x").expect("write");
        assert!(StateDir::new(file.join("sub")).ensure().is_err());
        assert!(write_atomic(&file.join("x"), b"", 0o600).is_err());
    }
}
