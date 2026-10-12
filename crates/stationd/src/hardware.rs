// SPDX-License-Identifier: GPL-3.0-only
//! The station's SDRs (station-protocol §4.2, §9): what is plugged in, and
//! what the package allows for each.
//!
//! [`scan`] reads the USB devices from sysfs (`/sys/bus/usb/devices/*`:
//! `idVendor`, `idProduct`, `manufacturer`, `product`, `serial`, and the
//! device name as the port); it needs no device access. The strings are the
//! devices' own claims, kept for display only.
//!
//! The SDR table ([`models_for`]), keyed by the USB identity:
//!
//! | The SDR reports | Models offered | Profiles |
//! |---|---|---|
//! | `0bda:2838` or `0bda:2832`, product containing `Blog V4` | `RTLSDR_BLOG_V4` | `RTLSDR_V4` |
//! | `0bda:2838` or `0bda:2832`, any other product | `RTLSDR_DIRECT_SAMPLING` | `RTLSDR`, `RTLSDR_HF` |
//! | | `RTLSDR_OTHER` | `RTLSDR` |
//! | another SDR ([`UNSUPPORTED_SDRS`]) | none | |
//!
//! A USB device that is not an SDR (a hub, a keyboard, a Pi's Ethernet
//! adapter) is not reported at all: the hardware report lists SDRs, and an
//! SDR the package doesn't support is one it knows to be an SDR.
//!
//! [`Hardware::report`] builds `v1::Hardware`: at most 16 SDRs in port order,
//! each with the receiver it serves now, and every profile of the package
//! once, so the hub can predict the range of a receiver whose SDR is not
//! plugged in.

use std::path::Path;

use station_proto::{limits, v1};

use crate::dispatch::truncate_utf8;
use crate::profile::ProfileId;

/// Where Linux lists USB devices.
pub const USB_DEVICES_DIR: &str = "/sys/bus/usb/devices";

/// RTL2832U dongles, the Blog V4 included: `0bda:2838` and `0bda:2832`.
pub const RTLSDR_IDS: [(u16, u16); 2] = [(0x0bda, 0x2838), (0x0bda, 0x2832)];

/// The `product` string of an RTL-SDR Blog V4 (its manufacturer string is
/// `RTLSDRBlog`; its IDs are the usual RTL-SDR ones).
pub const BLOG_V4_PRODUCT: &str = "Blog V4";

/// SDRs this package knows and doesn't support yet, by USB identity:
/// reported without models, so the owner sees them.
pub const UNSUPPORTED_SDRS: &[(u16, u16)] = &[
    (0x1d50, 0x60a1), // Airspy R2, Mini
    (0x03eb, 0x800c), // Airspy HF+
    (0x1d50, 0x6089), // HackRF One
    (0x1d50, 0xcc15), // HackRF One (rad1o)
    (0x1df7, 0x2500), // SDRplay RSP1
    (0x1df7, 0x3000), // SDRplay RSP1A
    (0x1df7, 0x3010), // SDRplay RSP2
    (0x1df7, 0x3020), // SDRplay RSPduo
    (0x1df7, 0x3030), // SDRplay RSPdx
    (0x1df7, 0x3050), // SDRplay RSP1B
    (0x1df7, 0x3060), // SDRplay RSPdx-R2
    (0x04b4, 0x00f1), // RX888 (Cypress FX3)
    (0x04b4, 0x00f3), // RX888 (FX3 boot loader)
    (0x04d8, 0xfb56), // FUNcube Dongle Pro
    (0x04d8, 0xfb31), // FUNcube Dongle Pro+
    (0x1d50, 0x6108), // LimeSDR-USB
    (0x0403, 0x601f), // LimeSDR Mini
    (0x2cf0, 0x5246), // bladeRF 2.0
    (0x1d50, 0x6066), // bladeRF
    (0x0456, 0xb673), // ADALM-Pluto
];

/// One USB device as sysfs lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    /// The device's directory name: its port (`1-1.3`).
    pub port: String,
    /// `idVendor`.
    pub vendor_id: u16,
    /// `idProduct`.
    pub product_id: u16,
    /// `manufacturer`, empty when absent.
    pub manufacturer: String,
    /// `product`, empty when absent.
    pub product: String,
    /// `serial`, empty when absent.
    pub serial: String,
}

impl UsbDevice {
    /// Whether it is an RTL-SDR (any model of the table).
    pub fn is_rtlsdr(&self) -> bool {
        RTLSDR_IDS.contains(&(self.vendor_id, self.product_id))
    }

    /// Whether it is an SDR this package knows: an RTL-SDR or one of
    /// [`UNSUPPORTED_SDRS`].
    pub fn is_sdr(&self) -> bool {
        self.is_rtlsdr() || UNSUPPORTED_SDRS.contains(&(self.vendor_id, self.product_id))
    }

    /// Whether it is an RTL-SDR Blog V4.
    pub fn is_blog_v4(&self) -> bool {
        self.is_rtlsdr() && self.product.contains(BLOG_V4_PRODUCT)
    }

    /// `vvvv:pppp`.
    pub fn usb_id(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor_id, self.product_id)
    }
}

/// The models and profiles the table offers for `dev` (§4.2): empty for an
/// SDR the package doesn't support.
pub fn models_for(dev: &UsbDevice) -> Vec<v1::ModelOption> {
    let option = |model: v1::SdrModel, profiles: &[v1::SdrProfile]| v1::ModelOption {
        model: model as i32,
        profiles: profiles.iter().map(|p| *p as i32).collect(),
    };
    if !dev.is_rtlsdr() {
        return Vec::new();
    }
    if dev.is_blog_v4() {
        return vec![option(
            v1::SdrModel::RtlsdrBlogV4,
            &[v1::SdrProfile::RtlsdrV4],
        )];
    }
    vec![
        option(
            v1::SdrModel::RtlsdrDirectSampling,
            &[v1::SdrProfile::Rtlsdr, v1::SdrProfile::RtlsdrHf],
        ),
        option(v1::SdrModel::RtlsdrOther, &[v1::SdrProfile::Rtlsdr]),
    ]
}

/// A sysfs string: trimmed, without control characters, at most `max`
/// bytes.
fn clean(raw: &str, max: usize) -> String {
    let s: String = raw.trim().chars().filter(|c| !c.is_control()).collect();
    truncate_utf8(&s, max)
}

/// The order of USB ports (`1-1.2` before `1-1.10` before `2-1`): numbers
/// compare as numbers.
fn port_key(port: &str) -> Vec<(u64, String)> {
    port.split(['-', '.'])
        .map(|part| (part.parse::<u64>().unwrap_or(u64::MAX), part.to_owned()))
        .collect()
}

/// Every USB device under `dir` (a directory laid out like
/// [`USB_DEVICES_DIR`]), in port order. Entries without readable hex
/// `idVendor` and `idProduct` (interfaces, unreadable ones) are left out;
/// a missing directory is no device.
pub fn scan(dir: &Path) -> Vec<UsbDevice> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let read = |d: &Path, f: &str| std::fs::read_to_string(d.join(f)).unwrap_or_default();
    let hex = |d: &Path, f: &str| u16::from_str_radix(read(d, f).trim(), 16).ok();
    let mut out: Vec<UsbDevice> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let d = e.path();
            Some(UsbDevice {
                port: clean(&e.file_name().to_string_lossy(), limits::MAX_SDR_PORT_BYTES),
                vendor_id: hex(&d, "idVendor")?,
                product_id: hex(&d, "idProduct")?,
                manufacturer: clean(&read(&d, "manufacturer"), limits::MAX_SDR_STRING_BYTES),
                product: clean(&read(&d, "product"), limits::MAX_SDR_STRING_BYTES),
                serial: clean(&read(&d, "serial"), limits::MAX_SDR_STRING_BYTES),
            })
        })
        .collect();
    out.sort_by_cached_key(|d| port_key(&d.port));
    out
}

/// The SDRs of a scan, at most [`limits::MAX_SDRS`], in port order.
pub fn sdrs(devices: &[UsbDevice]) -> Vec<&UsbDevice> {
    devices
        .iter()
        .filter(|d| d.is_sdr())
        .take(limits::MAX_SDRS)
        .collect()
}

/// The RTL-SDR a receiver with `serial` uses (§4.2, §9): the first one in
/// port order with that serial, or, for an empty serial, the only RTL-SDR
/// when there is exactly one.
pub fn device_for<'a>(devices: &'a [UsbDevice], serial: &str) -> Option<&'a UsbDevice> {
    let mut rtlsdrs = devices.iter().filter(|d| d.is_rtlsdr());
    if serial.is_empty() {
        let only = rtlsdrs.next()?;
        return rtlsdrs.next().is_none().then_some(only);
    }
    rtlsdrs.find(|d| d.serial == serial)
}

/// The SDRs plugged in, as the hub sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hardware {
    /// Every USB device, in port order.
    pub devices: Vec<UsbDevice>,
}

impl Hardware {
    /// Scans `dir`.
    pub fn scan(dir: &Path) -> Self {
        Self { devices: scan(dir) }
    }

    /// `GetHardware`'s answer: the SDRs (at most 16, in port order) with the
    /// table's models and, for the one a receiver of `serving` uses, its
    /// `receiver_id`; then every profile of the package, once, in the
    /// table's order, whether an SDR offers it or not (a receiver's SDR may
    /// not be plugged in, §4.2). `serving` is the receivers'
    /// (`receiver_id`, `sdr_serial`).
    pub fn report(&self, serving: &[(u32, String)]) -> v1::Hardware {
        let sdrs = sdrs(&self.devices);
        let receiver_of = |dev: &UsbDevice| {
            serving.iter().find_map(|(id, serial)| {
                device_for(&self.devices, serial)
                    .is_some_and(|d| d.port == dev.port)
                    .then_some(*id)
            })
        };
        let sdrs: Vec<v1::Sdr> = sdrs
            .into_iter()
            .map(|dev| {
                let models = models_for(dev);
                v1::Sdr {
                    usb_vendor_id: u32::from(dev.vendor_id),
                    usb_product_id: u32::from(dev.product_id),
                    manufacturer: dev.manufacturer.clone(),
                    product: dev.product.clone(),
                    serial: dev.serial.clone(),
                    port: dev.port.clone(),
                    models,
                    receiver_id: receiver_of(dev),
                }
            })
            .collect();
        let profiles = ProfileId::ALL
            .iter()
            .filter_map(|p| p.profile().info())
            .collect();
        v1::Hardware { sdrs, profiles }
    }

    /// The SDRs in one line each, for `stationd doctor`.
    pub fn describe(&self) -> Vec<String> {
        sdrs(&self.devices)
            .into_iter()
            .map(|d| {
                let what = if d.is_blog_v4() {
                    "RTL-SDR Blog V4"
                } else if d.is_rtlsdr() {
                    "RTL-SDR"
                } else {
                    "an SDR this package doesn't support"
                };
                let serial = if d.serial.is_empty() {
                    "no serial".to_owned()
                } else {
                    format!("serial {}", d.serial)
                };
                format!(
                    "{what} ({}, {:?}) at USB port {}, {serial}",
                    d.usb_id(),
                    d.product,
                    d.port
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use station_proto::validate::{check_message, check_receiver_set};

    /// A device directory as sysfs lists it.
    pub(crate) fn usb(dir: &Path, port: &str, ids: (u16, u16), product: &str, serial: &str) {
        let d = dir.join(port);
        std::fs::create_dir_all(&d).expect("mkdir");
        std::fs::write(d.join("idVendor"), format!("{:04x}\n", ids.0)).expect("write");
        std::fs::write(d.join("idProduct"), format!("{:04x}\n", ids.1)).expect("write");
        std::fs::write(d.join("product"), format!("{product}\n")).expect("write");
        std::fs::write(d.join("manufacturer"), "Maker\n").expect("write");
        if !serial.is_empty() {
            std::fs::write(d.join("serial"), format!("{serial}\n")).expect("write");
        }
    }

    const RTL: (u16, u16) = (0x0bda, 0x2838);

    #[test]
    fn scans_sysfs_in_port_order() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path();
        usb(dir, "usb1", (0x1d6b, 0x0002), "xHCI Host Controller", "");
        usb(dir, "1-1.10", RTL, "RTL2838UHIDIR", "00000003");
        usb(dir, "1-1.2", (0x0bda, 0x2832), "RTL2832U", "00000001");
        usb(dir, "1-1.3", RTL, "Blog V4", "00000002");
        usb(dir, "1-1.4", (0x1d50, 0x60a1), "AIRSPY", "a1");
        // An interface, and a device whose ids can't be read.
        std::fs::create_dir(dir.join("1-1.2:1.0")).expect("mkdir");
        std::fs::create_dir(dir.join("1-1.5")).expect("mkdir");
        std::fs::write(dir.join("1-1.5").join("idVendor"), "zz\n").expect("write");
        let devices = scan(dir);
        let ports: Vec<&str> = devices.iter().map(|d| d.port.as_str()).collect();
        assert_eq!(ports, ["1-1.2", "1-1.3", "1-1.4", "1-1.10", "usb1"]);
        assert_eq!(devices[0].serial, "00000001");
        assert_eq!(devices[0].manufacturer, "Maker");
        assert!(devices[1].is_blog_v4());
        assert!(!devices[2].is_rtlsdr() && devices[2].is_sdr());
        assert!(!devices[4].is_sdr());
        assert_eq!(devices[4].serial, "");
        assert_eq!(devices[0].usb_id(), "0bda:2832");
        assert!(scan(&dir.join("none")).is_empty());
    }

    #[test]
    fn strings_are_cleaned_and_capped() {
        let tmp = tempfile::tempdir().expect("tmp");
        let long = "é".repeat(100);
        usb(
            tmp.path(),
            "1-1",
            RTL,
            &format!("RTL\u{7}{long}"),
            "  ab\tc ",
        );
        let d = &scan(tmp.path())[0];
        assert!(d.product.starts_with("RTLé"));
        assert!(d.product.len() <= limits::MAX_SDR_STRING_BYTES);
        assert_eq!(d.serial, "abc");
    }

    #[test]
    fn the_sdr_table() {
        let dev = |product: &str, ids: (u16, u16)| UsbDevice {
            port: "1-1".into(),
            vendor_id: ids.0,
            product_id: ids.1,
            manufacturer: String::new(),
            product: product.into(),
            serial: String::new(),
        };
        let profiles = |m: &[v1::ModelOption]| -> Vec<(i32, Vec<i32>)> {
            m.iter().map(|o| (o.model, o.profiles.clone())).collect()
        };
        assert_eq!(
            profiles(&models_for(&dev("Blog V4", RTL))),
            vec![(
                v1::SdrModel::RtlsdrBlogV4 as i32,
                vec![v1::SdrProfile::RtlsdrV4 as i32]
            )]
        );
        let generic = vec![
            (
                v1::SdrModel::RtlsdrDirectSampling as i32,
                vec![
                    v1::SdrProfile::Rtlsdr as i32,
                    v1::SdrProfile::RtlsdrHf as i32,
                ],
            ),
            (
                v1::SdrModel::RtlsdrOther as i32,
                vec![v1::SdrProfile::Rtlsdr as i32],
            ),
        ];
        assert_eq!(profiles(&models_for(&dev("RTL2838UHIDIR", RTL))), generic);
        assert_eq!(
            profiles(&models_for(&dev("RTL2832U", (0x0bda, 0x2832)))),
            generic
        );
        // The V4's string on another device is not a V4.
        assert!(models_for(&dev("Blog V4", (0x1d50, 0x60a1))).is_empty());
        assert!(models_for(&dev("", (0x1234, 0x5678))).is_empty());
    }

    fn devices() -> Vec<UsbDevice> {
        let d = |port: &str, product: &str, serial: &str| UsbDevice {
            port: port.into(),
            vendor_id: 0x0bda,
            product_id: 0x2838,
            manufacturer: "Realtek".into(),
            product: product.into(),
            serial: serial.into(),
        };
        vec![
            d("1-1.2", "RTL2838UHIDIR", "00000001"),
            d("1-1.3", "Blog V4", "CH000002"),
            d("1-1.4", "RTL2838UHIDIR", "00000001"),
        ]
    }

    #[test]
    fn devices_for_serials() {
        let all = devices();
        assert_eq!(
            device_for(&all, "00000001").map(|d| d.port.as_str()),
            Some("1-1.2")
        );
        assert_eq!(
            device_for(&all, "CH000002").map(|d| d.port.as_str()),
            Some("1-1.3")
        );
        assert_eq!(device_for(&all, "nope"), None);
        assert_eq!(device_for(&all, ""), None, "three RTL-SDRs");
        assert_eq!(
            device_for(&all[1..2], "").map(|d| d.port.as_str()),
            Some("1-1.3")
        );
        assert_eq!(device_for(&[], ""), None);
    }

    #[test]
    fn the_report() {
        let mut devs = devices();
        devs.push(UsbDevice {
            port: "1-1.5".into(),
            vendor_id: 0x1d50,
            product_id: 0x60a1,
            manufacturer: "Airspy".into(),
            product: "AIRSPY".into(),
            serial: "a".into(),
        });
        devs.push(UsbDevice {
            port: "usb1".into(),
            vendor_id: 0x1d6b,
            product_id: 2,
            manufacturer: String::new(),
            product: String::new(),
            serial: String::new(),
        });
        let hw = Hardware { devices: devs };
        let report = hw.report(&[(7, "00000001".into()), (9, "CH000002".into())]);
        let ports: Vec<&str> = report.sdrs.iter().map(|s| s.port.as_str()).collect();
        assert_eq!(ports, ["1-1.2", "1-1.3", "1-1.4", "1-1.5"]);
        let serving: Vec<Option<u32>> = report.sdrs.iter().map(|s| s.receiver_id).collect();
        assert_eq!(serving, [Some(7), Some(9), None, None]);
        assert!(report.sdrs[3].models.is_empty());
        let profiles: Vec<i32> = report.profiles.iter().map(|p| p.profile).collect();
        assert_eq!(
            profiles,
            [
                v1::SdrProfile::Rtlsdr as i32,
                v1::SdrProfile::RtlsdrHf as i32,
                v1::SdrProfile::RtlsdrV4 as i32
            ]
        );
        // The hub's checks pass it.
        let msg = v1::StationMessage {
            body: Some(v1::station_message::Body::Response(v1::Response {
                request_id: 1,
                result: Some(v1::Result {
                    code: v1::ResultCode::Ok as i32,
                    message: String::new(),
                }),
                payload: Some(v1::response::Payload::Hardware(report.clone())),
            })),
        };
        assert_eq!(check_message(&msg, 0), Ok(()));
        // Without SDRs: no SDR, and still every profile of the package.
        let empty = Hardware {
            devices: Vec::new(),
        }
        .report(&[]);
        assert!(empty.sdrs.is_empty());
        assert_eq!(empty.profiles, report.profiles);
        // Only a V4: the direct-sampling profiles too.
        let v4 = Hardware {
            devices: devices()[1..2].to_vec(),
        }
        .report(&[]);
        assert_eq!(v4.profiles, report.profiles);
        let lines = hw.describe();
        assert_eq!(lines.len(), 4);
        assert!(
            lines[1].starts_with("RTL-SDR Blog V4 (0bda:2838"),
            "{}",
            lines[1]
        );
        assert!(lines[3].contains("doesn't support"), "{}", lines[3]);
    }

    #[test]
    fn the_report_meets_the_shared_rules() {
        let report = Hardware { devices: devices() }.report(&[]);
        // A set the table allows passes the shared rules; a V4 on HF by
        // direct sampling doesn't.
        let rx = |id, serial: &str, model: v1::SdrModel, profile: v1::SdrProfile, center| {
            v1::ReceiverConfig {
                receiver_id: id,
                sdr_serial: serial.into(),
                model: model as i32,
                profile: profile as i32,
                center_frequency_hz: center,
                ..Default::default()
            }
        };
        let good = v1::ConfigureReceivers {
            receivers: vec![
                rx(
                    1,
                    "00000001",
                    v1::SdrModel::RtlsdrDirectSampling,
                    v1::SdrProfile::RtlsdrHf,
                    7_100_000,
                ),
                rx(
                    2,
                    "CH000002",
                    v1::SdrModel::RtlsdrBlogV4,
                    v1::SdrProfile::RtlsdrV4,
                    145_000_000,
                ),
            ],
        };
        assert_eq!(check_receiver_set(&good, &report), Ok(()));
        let bad = v1::ConfigureReceivers {
            receivers: vec![rx(
                2,
                "CH000002",
                v1::SdrModel::RtlsdrBlogV4,
                v1::SdrProfile::RtlsdrHf,
                7_100_000,
            )],
        };
        assert_eq!(
            check_receiver_set(&bad, &report).map_err(|e| e.result_code()),
            Err(v1::ResultCode::PermissionDenied)
        );
    }

    #[test]
    fn at_most_sixteen_sdrs() {
        let devs: Vec<UsbDevice> = (0..20)
            .map(|i| UsbDevice {
                port: format!("1-{i}"),
                vendor_id: 0x0bda,
                product_id: 0x2838,
                manufacturer: String::new(),
                product: String::new(),
                serial: format!("{i}"),
            })
            .collect();
        assert_eq!(
            Hardware { devices: devs }.report(&[]).sdrs.len(),
            limits::MAX_SDRS
        );
    }
}
