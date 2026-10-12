// SPDX-License-Identifier: MIT
//! The station under test: how the conformance scenarios start and stop a
//! station implementation, and the SDRs it sees.

use std::path::{Path, PathBuf};
use std::time::Duration;

use uuid::Uuid;

use crate::error::BoxError;

/// Where and how the station under test must connect.
#[derive(Debug, Clone)]
pub struct Target {
    /// The gateway URL, `https://localhost:<port>`.
    pub gateway_url: String,
    /// The CA bundle (PEM file) to trust for the gateway, and nothing else.
    pub gateway_ca: PathBuf,
    /// The station's client certificate chain (PEM file).
    pub cert: PathBuf,
    /// The station's private key (PEM file, mode 0600).
    pub key: PathBuf,
    /// The station ID in `cert`.
    pub station_id: Uuid,
    /// Where the station reads its SDRs from, instead of
    /// `/sys/bus/usb/devices`: a directory laid out like it, one
    /// subdirectory per USB device with the files `idVendor`, `idProduct`
    /// (hex), `manufacturer`, `product` and `serial`. The scenarios plug
    /// and unplug simulated SDRs here ([`SimulatedSdr`]); the station must
    /// notice within `ScenarioConfig::hardware_within`.
    pub usb_devices: PathBuf,
    /// A scratch directory for the station (e.g. its config file and
    /// state); removed after the scenario. It is the same directory when a
    /// scenario restarts the station, so what the station keeps (its
    /// receiver set, station protocol §7) survives the restart.
    pub work_dir: PathBuf,
}

/// A USB device the scenarios plug into [`Target::usb_devices`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulatedSdr {
    /// The device's directory name, its port (`1-1.2`).
    pub port: String,
    /// `idVendor`.
    pub vendor_id: u16,
    /// `idProduct`.
    pub product_id: u16,
    /// The `manufacturer` string.
    pub manufacturer: String,
    /// The `product` string.
    pub product: String,
    /// The `serial` string; empty writes no `serial` file.
    pub serial: String,
}

impl SimulatedSdr {
    /// A generic RTL-SDR (`0bda:2838`, Realtek `RTL2838UHIDIR`), such as a
    /// Blog V3.
    pub fn rtlsdr(port: &str, serial: &str) -> Self {
        Self {
            port: port.to_owned(),
            vendor_id: 0x0bda,
            product_id: 0x2838,
            manufacturer: "Realtek".to_owned(),
            product: "RTL2838UHIDIR".to_owned(),
            serial: serial.to_owned(),
        }
    }

    /// An RTL-SDR Blog V4 (`0bda:2838`, `RTLSDRBlog` `Blog V4`).
    pub fn blog_v4(port: &str, serial: &str) -> Self {
        Self {
            manufacturer: "RTLSDRBlog".to_owned(),
            product: "Blog V4".to_owned(),
            ..Self::rtlsdr(port, serial)
        }
    }

    /// An Airspy R2 (`1d50:60a1`): an SDR the station doesn't support.
    pub fn airspy(port: &str, serial: &str) -> Self {
        Self {
            port: port.to_owned(),
            vendor_id: 0x1d50,
            product_id: 0x60a1,
            manufacturer: "Airspy".to_owned(),
            product: "AIRSPY".to_owned(),
            serial: serial.to_owned(),
        }
    }

    /// A USB hub (`1d6b:0002`): not an SDR at all.
    pub fn hub(port: &str) -> Self {
        Self {
            port: port.to_owned(),
            vendor_id: 0x1d6b,
            product_id: 0x0002,
            manufacturer: "Linux Foundation".to_owned(),
            product: "2.0 root hub".to_owned(),
            serial: String::new(),
        }
    }

    /// Writes this device under `usb_devices`.
    pub fn plug(&self, usb_devices: &Path) -> std::io::Result<()> {
        let dir = usb_devices.join(&self.port);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("idVendor"), format!("{:04x}\n", self.vendor_id))?;
        std::fs::write(dir.join("idProduct"), format!("{:04x}\n", self.product_id))?;
        std::fs::write(dir.join("manufacturer"), format!("{}\n", self.manufacturer))?;
        std::fs::write(dir.join("product"), format!("{}\n", self.product))?;
        if !self.serial.is_empty() {
            std::fs::write(dir.join("serial"), format!("{}\n", self.serial))?;
        }
        Ok(())
    }

    /// Removes the device at `port` from `usb_devices`.
    pub fn unplug(usb_devices: &Path, port: &str) -> std::io::Result<()> {
        let dir = usb_devices.join(port);
        for file in ["idVendor", "idProduct", "manufacturer", "product", "serial"] {
            match std::fs::remove_file(dir.join(file)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        std::fs::remove_dir(dir)
    }
}

/// How a station process ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    /// Whether it exited successfully (exit status 0).
    pub success: bool,
    /// A description (exit code, error).
    pub detail: String,
}

/// A station implementation driven by the scenarios.
///
/// The station must reconnect by itself (station-protocol §2.2). Scenarios
/// restart it only to check what it keeps (§7): [`StationUnderTest::stop`],
/// then [`StationUnderTest::start`] with the same [`Target`]. The station
/// must run the receivers it is configured with on SDRs it reads from
/// [`Target::usb_devices`], without hardware (a simulated backend).
#[async_trait::async_trait]
pub trait StationUnderTest: Send {
    /// Starts the station against `target`. Returns once it is started, not
    /// necessarily connected.
    async fn start(&mut self, target: &Target) -> Result<(), BoxError>;

    /// Requests a graceful shutdown (SIGTERM) and waits for the exit. Must
    /// also work, and return the stored exit, if the station already exited.
    async fn stop(&mut self) -> Result<Exit, BoxError>;

    /// Waits up to `within` for the station to exit on its own; `None` if it
    /// is still running.
    async fn wait_exit(&mut self, within: Duration) -> Result<Option<Exit>, BoxError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulated_sdrs_are_written_like_sysfs() {
        let dir = tempfile::tempdir().expect("tmp");
        let v4 = SimulatedSdr::blog_v4("1-1.3", "00000001");
        v4.plug(dir.path()).expect("plug");
        let read = |f: &str| std::fs::read_to_string(dir.path().join("1-1.3").join(f));
        assert_eq!(read("idVendor").expect("vendor"), "0bda\n");
        assert_eq!(read("idProduct").expect("product id"), "2838\n");
        assert_eq!(read("product").expect("product"), "Blog V4\n");
        assert_eq!(read("serial").expect("serial"), "00000001\n");
        SimulatedSdr::hub("usb1").plug(dir.path()).expect("hub");
        assert!(read("serial").is_ok());
        assert!(!dir.path().join("usb1").join("serial").exists());
        SimulatedSdr::unplug(dir.path(), "1-1.3").expect("unplug");
        assert!(!dir.path().join("1-1.3").exists());
        SimulatedSdr::unplug(dir.path(), "usb1").expect("unplug without a serial");
        assert!(SimulatedSdr::unplug(dir.path(), "1-1.3").is_err());
        assert_eq!(SimulatedSdr::airspy("1-2", "a").vendor_id, 0x1d50);
        assert_eq!(SimulatedSdr::rtlsdr("1-2", "a").product, "RTL2838UHIDIR");
    }
}
