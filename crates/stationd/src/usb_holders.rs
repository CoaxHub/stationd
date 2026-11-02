// SPDX-License-Identifier: GPL-3.0-only
//! Who holds an SDR: the programs with its USB device node
//! (`/dev/bus/usb/<bus>/<device>`) open, and the kernel driver bound to it.
//!
//! An RTL-SDR can be used by one program at a time. When another program
//! holds it (`rtl_tcp`, a GQRX instance, a TV application), radiod cannot
//! open it and exits at once; `stationd doctor` uses this to name the
//! program. It reads `/proc/<pid>/fd`, which Linux shows only for the
//! processes the caller may inspect (its own, or all of them for root), so
//! [`Holders::uninspected`] counts the ones it could not look at: an empty
//! answer is only as good as that count says.

use std::path::{Path, PathBuf};

/// Where Linux lists processes.
pub const PROC_DIR: &str = "/proc";

/// Where Linux has the USB device nodes.
pub const DEV_BUS_USB: &str = "/dev/bus/usb";

/// The longest process name shown.
const MAX_NAME_BYTES: usize = 32;

/// A program that has an SDR's device node open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// Its process id.
    pub pid: u32,
    /// Its name (`comm`), without control characters.
    pub name: String,
    /// The receiver whose radiod this is, when it runs in that receiver's
    /// unit (`coaxhub-radiod@<id>.service`): the station's own use of the
    /// SDR, not another program's.
    pub receiver: Option<u32>,
}

/// What a look at the processes found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Holders {
    /// The programs with the node open, by process id.
    pub holders: Vec<Holder>,
    /// Processes whose open files the caller may not see (another user's,
    /// without root).
    pub uninspected: usize,
}

impl Holders {
    /// The programs that are not one of this station's own radiods.
    pub fn foreign(&self) -> impl Iterator<Item = &Holder> {
        self.holders.iter().filter(|h| h.receiver.is_none())
    }
}

/// The device node of the USB device at `port` under `usb_devices` (a
/// directory laid out like `/sys/bus/usb/devices`), from its `busnum` and
/// `devnum`, under `dev_root` (`/dev/bus/usb`). `None` when sysfs doesn't
/// say.
pub fn device_node(usb_devices: &Path, port: &str, dev_root: &Path) -> Option<PathBuf> {
    let number = |file: &str| {
        std::fs::read_to_string(usb_devices.join(port).join(file))
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()
    };
    Some(
        dev_root
            .join(format!("{:03}", number("busnum")?))
            .join(format!("{:03}", number("devnum")?)),
    )
}

/// The kernel driver bound to the device at `port`'s first interface
/// (`dvb_usb_rtl28xxu` when the DVB-T driver holds an RTL-SDR); `None`
/// when none is.
pub fn bound_driver(usb_devices: &Path, port: &str) -> Option<String> {
    let link = usb_devices
        .join(port)
        .join(format!("{port}:1.0"))
        .join("driver");
    let target = std::fs::read_link(link).ok()?;
    Some(clean(&target.file_name()?.to_string_lossy()))
}

/// The programs under `proc_dir` (`/proc`) that have `node` open.
pub fn holders(proc_dir: &Path, node: &Path) -> Holders {
    let mut out = Holders::default();
    let Ok(entries) = std::fs::read_dir(proc_dir) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        let fds = match std::fs::read_dir(dir.join("fd")) {
            Ok(fds) => fds,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                out.uninspected += 1;
                continue;
            }
            // Gone since the listing, or not a process.
            Err(_) => continue,
        };
        let holds = fds
            .filter_map(Result::ok)
            .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|t| t == node));
        if !holds {
            continue;
        }
        let read = |file: &str| std::fs::read_to_string(dir.join(file)).unwrap_or_default();
        out.holders.push(Holder {
            pid,
            name: clean(read("comm").trim()),
            receiver: receiver_of_cgroup(&read("cgroup")),
        });
    }
    out.holders.sort_by_key(|h| h.pid);
    out
}

/// A name for the terminal: no control characters, short.
fn clean(raw: &str) -> String {
    let s: String = raw.chars().filter(|c| !c.is_control()).collect();
    crate::dispatch::truncate_utf8(&s, MAX_NAME_BYTES)
}

/// The receiver of the system unit `coaxhub-radiod@<id>.service` in a
/// `/proc/<pid>/cgroup`: the path is under `/system.slice/` and ends with
/// the unit, so a user's own unit of that name (`systemctl --user`, under
/// `/user.slice/`) is not taken for the station's.
fn receiver_of_cgroup(cgroup: &str) -> Option<u32> {
    cgroup.lines().find_map(|line| {
        let path = line.splitn(3, ':').nth(2)?;
        let unit = path.strip_prefix("/system.slice/")?.rsplit('/').next()?;
        let id = unit
            .strip_prefix("coaxhub-radiod@")?
            .strip_suffix(".service")?;
        id.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| id.parse().ok())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn process(proc_dir: &Path, pid: u32, comm: &str, cgroup: &str, open: &[&Path]) {
        let dir = proc_dir.join(pid.to_string());
        std::fs::create_dir_all(dir.join("fd")).expect("mkdir");
        std::fs::write(dir.join("comm"), format!("{comm}\n")).expect("write");
        std::fs::write(dir.join("cgroup"), cgroup).expect("write");
        for (i, target) in open.iter().enumerate() {
            std::os::unix::fs::symlink(target, dir.join("fd").join(i.to_string())).expect("link");
        }
    }

    #[test]
    fn the_node_comes_from_the_bus_and_device_numbers() {
        let tmp = tempfile::tempdir().expect("tmp");
        let port = tmp.path().join("1-1.3");
        std::fs::create_dir_all(&port).expect("mkdir");
        assert_eq!(
            device_node(tmp.path(), "1-1.3", Path::new("/dev/bus/usb")),
            None
        );
        std::fs::write(port.join("busnum"), "1\n").expect("write");
        std::fs::write(port.join("devnum"), "12\n").expect("write");
        assert_eq!(
            device_node(tmp.path(), "1-1.3", Path::new("/dev/bus/usb")),
            Some(PathBuf::from("/dev/bus/usb/001/012"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_kernel_driver_is_the_interfaces_link() {
        let tmp = tempfile::tempdir().expect("tmp");
        let interface = tmp.path().join("1-1.3").join("1-1.3:1.0");
        std::fs::create_dir_all(&interface).expect("mkdir");
        assert_eq!(bound_driver(tmp.path(), "1-1.3"), None);
        std::os::unix::fs::symlink(
            "../../../../../../bus/usb/drivers/dvb_usb_rtl28xxu",
            interface.join("driver"),
        )
        .expect("link");
        assert_eq!(
            bound_driver(tmp.path(), "1-1.3").as_deref(),
            Some("dvb_usb_rtl28xxu")
        );
    }

    #[cfg(unix)]
    #[test]
    fn processes_with_the_node_open_are_named() {
        let tmp = tempfile::tempdir().expect("tmp");
        let node = Path::new("/dev/bus/usb/001/012");
        let other = Path::new("/dev/bus/usb/001/013");
        process(
            tmp.path(),
            300,
            "rtl_tcp",
            "0::/user.slice/session-2.scope\n",
            &[node],
        );
        process(
            tmp.path(),
            120,
            "radiod",
            "0::/system.slice/system-coaxhub\\x2dradiod.slice/coaxhub-radiod@2.service\n",
            &[other, node],
        );
        process(tmp.path(), 400, "gqrx", "0::/user.slice\n", &[other]);
        // Not a process.
        std::fs::create_dir_all(tmp.path().join("sys")).expect("mkdir");
        let found = holders(tmp.path(), node);
        assert_eq!(found.uninspected, 0);
        let names: Vec<(u32, &str, Option<u32>)> = found
            .holders
            .iter()
            .map(|h| (h.pid, h.name.as_str(), h.receiver))
            .collect();
        assert_eq!(names, [(120, "radiod", Some(2)), (300, "rtl_tcp", None)]);
        let foreign: Vec<u32> = found.foreign().map(|h| h.pid).collect();
        assert_eq!(foreign, [300]);
    }

    #[cfg(unix)]
    #[test]
    fn processes_the_caller_may_not_inspect_are_counted() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().expect("tmp");
        let node = Path::new("/dev/bus/usb/001/012");
        process(tmp.path(), 500, "rtl_tcp", "0::/\n", &[node]);
        let fd = tmp.path().join("500").join("fd");
        std::fs::set_permissions(&fd, std::fs::Permissions::from_mode(0o000)).expect("chmod");
        let found = holders(tmp.path(), node);
        // Root reads it anyway; everyone else is told it could not be seen.
        if std::fs::read_dir(&fd).is_err() {
            assert_eq!(found.uninspected, 1);
            assert!(found.holders.is_empty());
        } else {
            assert_eq!(found.holders.len(), 1);
        }
        std::fs::set_permissions(&fd, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn cgroups_name_the_receiver() {
        assert_eq!(
            receiver_of_cgroup(
                "0::/system.slice/system-coaxhub\\x2dradiod.slice/coaxhub-radiod@17.service\n"
            ),
            Some(17)
        );
        assert_eq!(receiver_of_cgroup("0::/user.slice\n"), None);
        assert_eq!(
            receiver_of_cgroup("0::/system.slice/coaxhub-radiod@.service\n"),
            None
        );
        assert_eq!(
            receiver_of_cgroup("0::/system.slice/coaxhub-radiod@3.scope\n"),
            None
        );
        // A user's own unit of that name is not the station's.
        assert_eq!(
            receiver_of_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/coaxhub-radiod@3.service\n"
            ),
            None
        );
        assert_eq!(
            receiver_of_cgroup("0::/x/system.slice/coaxhub-radiod@3.service\n"),
            None
        );
    }

    #[test]
    fn names_are_short_and_plain() {
        assert_eq!(clean("rtl_\u{1b}[31mtcp"), "rtl_[31mtcp");
        assert_eq!(clean(&"x".repeat(100)).len(), MAX_NAME_BYTES);
    }
}
