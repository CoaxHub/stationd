// SPDX-License-Identifier: MIT
//! Throwaway test PKI generated in memory with `rcgen` (ECDSA P-256): a root
//! CA, a hub server certificate and a station client certificate carrying
//! exactly one `urn:coaxhub:station:<uuid>` URI SAN.
//!
//! For tests only: keys live in memory and in the harness's temporary
//! directory, never in the repository.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use rcgen::string::Ia5String;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use station_proto::identity;
use uuid::Uuid;

use crate::error::{ConformanceError, Result};
use crate::hub::{HubTls, TlsVersions};

fn pki_err(e: impl fmt::Display) -> ConformanceError {
    ConformanceError::Pki(e.to_string())
}

/// A CA plus one hub and one station certificate issued by it.
pub struct TestPki {
    /// The CA certificate (PEM): the trust bundle for both sides.
    pub ca_pem: String,
    /// The hub's server certificate (PEM), SANs `localhost`, `127.0.0.1`,
    /// `::1`.
    pub hub_cert_pem: String,
    /// The hub's private key (PEM).
    pub hub_key_pem: String,
    /// The station ID in the station certificate.
    pub station_id: Uuid,
    /// The station's client certificate (PEM).
    pub station_cert_pem: String,
    /// The station's private key (PEM).
    pub station_key_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl fmt::Debug for TestPki {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print keys.
        f.debug_struct("TestPki")
            .field("station_id", &self.station_id)
            .finish_non_exhaustive()
    }
}

impl TestPki {
    /// A fresh PKI with a random station ID.
    pub fn generate() -> Result<Self> {
        Self::generate_for(Uuid::new_v4())
    }

    /// A fresh PKI whose station certificate carries `station_id`.
    pub fn generate_for(station_id: Uuid) -> Result<Self> {
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(pki_err)?;
        params
            .distinguished_name
            .push(DnType::CommonName, "CoaxHub conformance test CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_key = KeyPair::generate().map_err(pki_err)?;
        let ca_cert = params.self_signed(&ca_key).map_err(pki_err)?;
        let issuer = Issuer::new(params, ca_key);
        let mut pki = Self {
            ca_pem: ca_cert.pem(),
            hub_cert_pem: String::new(),
            hub_key_pem: String::new(),
            station_id,
            station_cert_pem: String::new(),
            station_key_pem: String::new(),
            issuer,
        };
        let (hub_cert, hub_key) = pki.issue_hub(
            &["localhost"],
            &[
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ],
        )?;
        let (station_cert, station_key) = pki.issue_station(station_id)?;
        pki.hub_cert_pem = hub_cert;
        pki.hub_key_pem = hub_key;
        pki.station_cert_pem = station_cert;
        pki.station_key_pem = station_key;
        Ok(pki)
    }

    /// Issues a hub server certificate `(cert_pem, key_pem)`.
    pub fn issue_hub(&self, dns: &[&str], ips: &[IpAddr]) -> Result<(String, String)> {
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(pki_err)?;
        params
            .distinguished_name
            .push(DnType::CommonName, "coaxhub-conformance-hub");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        for name in dns {
            params.subject_alt_names.push(SanType::DnsName(
                Ia5String::try_from(*name).map_err(pki_err)?,
            ));
        }
        params
            .subject_alt_names
            .extend(ips.iter().copied().map(SanType::IpAddress));
        self.sign(&params)
    }

    /// Issues a station client certificate `(cert_pem, key_pem)` for `id`.
    pub fn issue_station(&self, id: Uuid) -> Result<(String, String)> {
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(pki_err)?;
        params
            .distinguished_name
            .push(DnType::CommonName, format!("coaxhub-station-{id}"));
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let uri = identity::station_uri(id);
        params.subject_alt_names = vec![SanType::URI(
            Ia5String::try_from(uri.as_str()).map_err(pki_err)?,
        )];
        self.sign(&params)
    }

    fn sign(&self, params: &CertificateParams) -> Result<(String, String)> {
        let key = KeyPair::generate().map_err(pki_err)?;
        let cert = params.signed_by(&key, &self.issuer).map_err(pki_err)?;
        Ok((cert.pem(), key.serialize_pem()))
    }

    /// The hub's TLS settings: its certificate, requiring station
    /// certificates from this CA, TLS 1.3 only.
    pub fn hub_tls(&self) -> HubTls {
        HubTls {
            cert_chain_pem: self.hub_cert_pem.clone(),
            key_pem: self.hub_key_pem.clone(),
            client_ca_pem: self.ca_pem.clone(),
            versions: TlsVersions::Tls13Only,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject as _;

    #[test]
    fn station_certificate_identifies_the_station() {
        let pki = TestPki::generate().expect("pki");
        let der = CertificateDer::from_pem_slice(pki.station_cert_pem.as_bytes()).expect("pem");
        assert_eq!(identity::station_id_from_cert_der(&der), Ok(pki.station_id));
        let hub = CertificateDer::from_pem_slice(pki.hub_cert_pem.as_bytes()).expect("pem");
        assert!(identity::station_id_from_cert_der(&hub).is_err());
        assert!(!format!("{pki:?}").contains("PRIVATE"));
        let id = Uuid::new_v4();
        assert_eq!(TestPki::generate_for(id).expect("pki").station_id, id);
        let tls = pki.hub_tls();
        assert_eq!(tls.versions, TlsVersions::Tls13Only);
        assert!(tls.server_config().is_ok());
    }
}
