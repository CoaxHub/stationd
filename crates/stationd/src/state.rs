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
//!
//! A certificate renewal replaces the key, the certificate, the chain and
//! (rarely) the CA bundle as **one** swap, so that no reader ever pairs a
//! new certificate with the old key or the reverse, even across a crash
//! ([`StateDir::swap_credentials`]):
//!
//! 1. the new files are written into a fresh `.next.tmp-*` directory and
//!    synced; a crash here leaves the old credentials in place and the
//!    temporary directory is removed by the next swap;
//! 2. that directory is renamed to `next/` (the commit point);
//! 3. each file is renamed from `next/` over the live one, then `next/` is
//!    removed.
//!
//! Readers take each swapped file from `next/` when it is there, else the
//! live one ([`StateDir::credential_file`]): before the commit they see
//! only old files, after it only new ones. An interrupted step 3 is
//! finished by the next swap or by [`StateDir::complete_swap`] at startup.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

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

/// The staging directory of a committed credential swap.
pub const NEXT_DIR: &str = "next";

/// Prefix of the uncommitted staging directories.
const STAGING_PREFIX: &str = ".next.tmp-";

/// The files a credential swap replaces, in the order they are moved into
/// place (the key last: until then, readers take it from `next/` too).
pub const SWAP_FILES: [&str; 4] = [CHAIN_FILE, CERT_FILE, GATEWAY_CA_FILE, KEY_FILE];

/// Serialises credential swaps and credential reads within this process, so
/// the daemon never loads a half-moved set (another process, such as
/// `stationd status`, may at worst see a transient key mismatch, which the
/// TLS loader refuses).
static SWAP_LOCK: Mutex<()> = Mutex::new(());

/// Holds off credential swaps in this process while the guard lives.
pub fn swap_guard() -> MutexGuard<'static, ()> {
    SWAP_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

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

/// What a certificate renewal stores ([`StateDir::swap_credentials`]).
#[derive(Debug)]
pub struct NewCredentials<'a> {
    /// The new private key, PEM.
    pub key_pem: &'a str,
    /// The new leaf certificate, PEM.
    pub cert_pem: &'a str,
    /// Its intermediates, PEM (may be empty).
    pub chain_pem: &'a str,
    /// A rotated gateway CA bundle, PEM; `None` keeps the current one.
    pub gateway_ca_pem: Option<&'a str>,
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
    ///
    /// A staged renewal of an earlier enrollment is discarded first.
    pub fn write_enrollment(&self, e: &Enrollment<'_>) -> Result<(), StateError> {
        self.ensure()?;
        let _guard = swap_guard();
        self.discard_staging(true)?;
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

    /// The file a reader should use for `name`: the staged one of a
    /// committed swap when there is one ([`SWAP_FILES`]), else the live one.
    pub fn credential_file(&self, name: &str) -> PathBuf {
        if SWAP_FILES.contains(&name) {
            let staged = self.path.join(NEXT_DIR).join(name);
            if staged.is_file() {
                return staged;
            }
        }
        self.file(name)
    }

    /// Replaces the key, certificate, chain and optionally the CA bundle as
    /// one atomic swap (see the [module docs](self)).
    pub fn swap_credentials(&self, c: &NewCredentials<'_>) -> Result<(), StateError> {
        let _guard = swap_guard();
        self.stage_locked(c)?;
        self.complete_locked().map(|_| ())
    }

    /// Steps 1 and 2 of a swap only: the new credentials are committed to
    /// `next/` but not moved into place (what a crash after the commit
    /// leaves behind). Readers already see the new set.
    pub fn stage_credentials(&self, c: &NewCredentials<'_>) -> Result<(), StateError> {
        let _guard = swap_guard();
        self.stage_locked(c)
    }

    /// Finishes an interrupted swap (moves what is left in `next/` into
    /// place) and removes uncommitted staging directories. Returns whether
    /// there was a swap to finish.
    pub fn complete_swap(&self) -> Result<bool, StateError> {
        let _guard = swap_guard();
        let finished = self.complete_locked()?;
        self.discard_staging(false)?;
        Ok(finished)
    }

    fn stage_locked(&self, c: &NewCredentials<'_>) -> Result<(), StateError> {
        self.complete_locked()?;
        self.discard_staging(false)?;
        let tmp = self
            .path
            .join(format!("{STAGING_PREFIX}{:016x}", rand::random::<u64>()));
        create_private_dir(&tmp)?;
        let mut files: Vec<(&str, &[u8], u32)> = vec![
            (KEY_FILE, c.key_pem.as_bytes(), KEY_MODE),
            (CERT_FILE, c.cert_pem.as_bytes(), PUBLIC_MODE),
            (CHAIN_FILE, c.chain_pem.as_bytes(), PUBLIC_MODE),
        ];
        if let Some(ca) = c.gateway_ca_pem {
            files.push((GATEWAY_CA_FILE, ca.as_bytes(), PUBLIC_MODE));
        }
        let written = files
            .into_iter()
            .try_for_each(|(name, contents, mode)| write_atomic(&tmp.join(name), contents, mode));
        if let Err(e) = written {
            let _ = fs::remove_dir_all(&tmp);
            return Err(e);
        }
        let next = self.path.join(NEXT_DIR);
        if let Err(e) = fs::rename(&tmp, &next) {
            let _ = fs::remove_dir_all(&tmp);
            return Err(err(&next)(e));
        }
        sync_dir(&self.path)
    }

    fn complete_locked(&self) -> Result<bool, StateError> {
        let next = self.path.join(NEXT_DIR);
        if !next.is_dir() {
            return Ok(false);
        }
        for name in SWAP_FILES {
            let staged = next.join(name);
            match fs::rename(&staged, self.file(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(err(&staged)(e)),
            }
        }
        sync_dir(&self.path)?;
        fs::remove_dir_all(&next).map_err(err(&next))?;
        sync_dir(&self.path)?;
        Ok(true)
    }

    /// Removes uncommitted staging directories, and `next/` too when
    /// `committed` (enrollment and unenrollment replace everything).
    fn discard_staging(&self, committed: bool) -> Result<(), StateError> {
        let entries = match fs::read_dir(&self.path) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(err(&self.path)(e)),
        };
        let mut removed = false;
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(STAGING_PREFIX) || (committed && name == NEXT_DIR) {
                let path = entry.path();
                fs::remove_dir_all(&path).map_err(err(&path))?;
                removed = true;
            }
        }
        if removed {
            sync_dir(&self.path)?;
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
        let _guard = swap_guard();
        self.discard_staging(true)?;
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

    /// Writes `assignment.json` atomically unless it already holds
    /// `assignment`; returns whether it changed.
    pub fn write_assignment(&self, assignment: &Assignment) -> Result<bool, StateError> {
        if self.assignment().ok().as_ref() == Some(assignment) {
            return Ok(false);
        }
        let path = self.file(ASSIGNMENT_FILE);
        let json = serde_json::to_string_pretty(assignment)
            .map_err(|x| err(&path)(std::io::Error::other(x)))?;
        write_atomic(&path, json.as_bytes(), PUBLIC_MODE)?;
        Ok(true)
    }
}

/// Creates the new directory `dir` with mode 0700, owned like its parent.
fn create_private_dir(dir: &Path) -> Result<(), StateError> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .mode(DIR_MODE)
        .create(dir)
        .map_err(err(dir))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE)).map_err(err(dir))?;
    if let Some(parent) = dir.parent() {
        let p = fs::metadata(parent).map_err(err(parent))?;
        let d = fs::metadata(dir).map_err(err(dir))?;
        if (p.uid(), p.gid()) != (d.uid(), d.gid()) {
            std::os::unix::fs::chown(dir, Some(p.uid()), Some(p.gid())).map_err(err(dir))?;
        }
    }
    Ok(())
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

    fn read(dir: &StateDir, name: &str) -> String {
        fs::read_to_string(dir.credential_file(name)).expect("read")
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut n: Vec<String> = fs::read_dir(dir)
            .expect("ls")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        n.sort();
        n
    }

    const NEW: NewCredentials<'static> = NewCredentials {
        key_pem: "KEY2",
        cert_pem: "CERT2",
        chain_pem: "CHAIN2",
        gateway_ca_pem: None,
    };

    #[test]
    fn swap_replaces_the_set_at_once() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = StateDir::new(tmp.path().join("state"));
        sample(&dir);
        dir.swap_credentials(&NEW).expect("swap");
        assert_eq!(read(&dir, KEY_FILE), "KEY2");
        assert_eq!(read(&dir, CERT_FILE), "CERT2");
        assert_eq!(read(&dir, CHAIN_FILE), "CHAIN2");
        assert_eq!(read(&dir, GATEWAY_CA_FILE), "CA", "kept");
        assert_eq!(mode(&dir.file(KEY_FILE)), 0o600);
        assert_eq!(mode(&dir.file(CERT_FILE)), 0o644);
        assert_eq!(names(dir.path()).len(), 6, "{:?}", names(dir.path()));
        // A rotated CA bundle.
        dir.swap_credentials(&NewCredentials {
            gateway_ca_pem: Some("CA3"),
            key_pem: "KEY3",
            ..NEW
        })
        .expect("swap");
        assert_eq!(read(&dir, GATEWAY_CA_FILE), "CA3");
        assert_eq!(read(&dir, KEY_FILE), "KEY3");
        assert!(!dir.complete_swap().expect("nothing to do"));
    }

    #[test]
    fn a_crash_after_the_commit_is_rolled_forward() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = StateDir::new(tmp.path());
        sample(&dir);
        dir.stage_credentials(&NEW).expect("stage");
        // Committed but not moved: readers already see the new set.
        assert_eq!(fs::read_to_string(dir.file(KEY_FILE)).expect("live"), "KEY");
        assert_eq!(read(&dir, KEY_FILE), "KEY2");
        assert_eq!(read(&dir, CERT_FILE), "CERT2");
        assert_eq!(read(&dir, GATEWAY_CA_FILE), "CA");
        assert_eq!(
            dir.credential_file(ASSIGNMENT_FILE),
            dir.file(ASSIGNMENT_FILE)
        );
        assert_eq!(mode(&dir.path().join(NEXT_DIR)), 0o700);
        assert_eq!(mode(&dir.path().join(NEXT_DIR).join(KEY_FILE)), 0o600);
        // A crash half-way through the moves: still the new set.
        fs::rename(
            dir.path().join(NEXT_DIR).join(CERT_FILE),
            dir.file(CERT_FILE),
        )
        .expect("move");
        assert_eq!(read(&dir, CERT_FILE), "CERT2");
        assert_eq!(read(&dir, KEY_FILE), "KEY2");
        // Startup finishes it.
        assert!(dir.complete_swap().expect("complete"));
        assert!(!dir.path().join(NEXT_DIR).exists());
        assert_eq!(
            fs::read_to_string(dir.file(KEY_FILE)).expect("live"),
            "KEY2"
        );
        assert_eq!(
            fs::read_to_string(dir.file(CHAIN_FILE)).expect("live"),
            "CHAIN2"
        );
        // A later swap also finishes an interrupted one first.
        dir.stage_credentials(&NEW).expect("stage");
        dir.swap_credentials(&NewCredentials {
            key_pem: "KEY4",
            ..NEW
        })
        .expect("swap");
        assert_eq!(read(&dir, KEY_FILE), "KEY4");
        assert!(!dir.path().join(NEXT_DIR).exists());
    }

    #[test]
    fn a_crash_before_the_commit_keeps_the_old_set() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = StateDir::new(tmp.path());
        sample(&dir);
        let partial = dir.path().join(".next.tmp-0000000000000001");
        fs::create_dir(&partial).expect("mkdir");
        fs::write(partial.join(KEY_FILE), "HALF").expect("write");
        assert_eq!(read(&dir, KEY_FILE), "KEY");
        assert!(!dir.complete_swap().expect("nothing committed"));
        assert!(!partial.exists(), "uncommitted staging removed");
        // A failed write leaves the old files and no staging behind.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).expect("chmod");
        let e = dir.swap_credentials(&NEW);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("chmod");
        if e.is_ok() {
            // Running as root: permissions are not enforced.
            return;
        }
        assert_eq!(read(&dir, KEY_FILE), "KEY");
        assert_eq!(names(dir.path()).len(), 6, "{:?}", names(dir.path()));
    }

    #[test]
    fn enrollment_and_removal_discard_staged_renewals() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = StateDir::new(tmp.path());
        sample(&dir);
        dir.stage_credentials(&NEW).expect("stage");
        sample(&dir);
        assert_eq!(read(&dir, KEY_FILE), "KEY", "a new enrollment wins");
        assert!(!dir.path().join(NEXT_DIR).exists());
        dir.stage_credentials(&NEW).expect("stage");
        fs::create_dir(dir.path().join(".next.tmp-2")).expect("mkdir");
        assert_eq!(dir.remove_credentials().expect("rm").len(), 6);
        assert!(names(dir.path()).is_empty(), "{:?}", names(dir.path()));
        // Nothing to discard in a directory that does not exist.
        let gone = StateDir::new(tmp.path().join("gone"));
        assert!(gone.remove_credentials().expect("rm").is_empty());
        assert!(!gone.complete_swap().expect("none"));
    }

    #[test]
    fn assignment_is_written_only_when_it_changes() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = StateDir::new(tmp.path());
        let a = sample(&dir);
        assert!(!dir.write_assignment(&a).expect("same"));
        let b = Assignment {
            hub_id: "eu1".into(),
            gateway_url: "https://gw.eu1.example".into(),
        };
        assert!(dir.write_assignment(&b).expect("new"));
        assert_eq!(dir.assignment().expect("read"), b);
        assert_eq!(mode(&dir.file(ASSIGNMENT_FILE)), 0o644);
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
