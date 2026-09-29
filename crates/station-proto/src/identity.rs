// SPDX-License-Identifier: MIT
//! Station identity from the mTLS client certificate (station-protocol §8).
//!
//! The station ID is carried **only** in a URI SAN
//! `urn:coaxhub:station:<uuid>`; the subject and every protocol field are
//! ignored. Call this on the leaf certificate after the TLS stack has
//! verified the chain against the station CA: this module parses, it does not
//! verify signatures or validity.

use uuid::Uuid;
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::FromDer;

/// The URI SAN prefix that carries the station ID.
pub const STATION_ID_URI_PREFIX: &str = "urn:coaxhub:station:";

/// Why no station ID could be read from a certificate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdentityError {
    /// The bytes are not a DER X.509 certificate.
    #[error("not a DER X.509 certificate")]
    InvalidDer,
    /// Bytes follow the certificate.
    #[error("trailing bytes after the certificate")]
    TrailingData,
    /// The subjectAltName extension is malformed or duplicated.
    #[error("malformed subjectAltName extension")]
    InvalidSubjectAltName,
    /// No `urn:coaxhub:station:` URI SAN.
    #[error("no station URI SAN")]
    NoStationUri,
    /// More than one `urn:coaxhub:station:` URI SAN.
    #[error("{0} station URI SANs, expected exactly one")]
    MultipleStationUris(usize),
    /// The part after the prefix is not a canonical, non-nil UUID.
    #[error("station URI does not hold a canonical UUID")]
    InvalidUuid,
}

/// The station URI for `id`: `urn:coaxhub:station:<uuid>` (lowercase,
/// hyphenated), as the CA writes it into the SAN.
pub fn station_uri(id: Uuid) -> String {
    format!("{STATION_ID_URI_PREFIX}{}", id.hyphenated())
}

/// Parses a station URI. The UUID must be in canonical form: lowercase,
/// hyphenated, not nil. Other spellings of the same UUID (uppercase, braces,
/// no hyphens) are rejected so that each station has exactly one URI.
pub fn station_id_from_uri(uri: &str) -> Result<Uuid, IdentityError> {
    let rest = uri
        .strip_prefix(STATION_ID_URI_PREFIX)
        .ok_or(IdentityError::NoStationUri)?;
    let id = Uuid::try_parse(rest).map_err(|_| IdentityError::InvalidUuid)?;
    if id.is_nil() || id.hyphenated().to_string() != rest {
        return Err(IdentityError::InvalidUuid);
    }
    Ok(id)
}

/// The station ID from a client certificate (DER): requires exactly one URI
/// SAN starting with [`STATION_ID_URI_PREFIX`], holding a canonical UUID.
/// Other SANs (DNS names, other URIs) are ignored.
pub fn station_id_from_cert_der(der: &[u8]) -> Result<Uuid, IdentityError> {
    let (rest, cert) = X509Certificate::from_der(der).map_err(|_| IdentityError::InvalidDer)?;
    if !rest.is_empty() {
        return Err(IdentityError::TrailingData);
    }
    let san = cert
        .subject_alternative_name()
        .map_err(|_| IdentityError::InvalidSubjectAltName)?
        .ok_or(IdentityError::NoStationUri)?;
    let mut uris = san
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) if uri.starts_with(STATION_ID_URI_PREFIX) => Some(*uri),
            _ => None,
        });
    let first = uris.next().ok_or(IdentityError::NoStationUri)?;
    let others = uris.count();
    if others > 0 {
        return Err(IdentityError::MultipleStationUris(others + 1));
    }
    station_id_from_uri(first)
}

#[cfg(test)]
mod tests {
    use rcgen::{CertificateParams, KeyPair, SanType};

    use super::*;

    const ID: &str = "5f0c6b8e-3d7a-4c1e-9a52-0e6f1b2c3d4e";

    fn cert_with(sans: Vec<SanType>) -> Vec<u8> {
        let key = KeyPair::generate().expect("key");
        let mut params = CertificateParams::default();
        params.subject_alt_names = sans;
        params.self_signed(&key).expect("cert").der().to_vec()
    }

    fn uri(s: &str) -> SanType {
        SanType::URI(s.try_into().expect("ia5"))
    }

    #[test]
    fn exactly_one_station_uri() {
        let der = cert_with(vec![
            SanType::DnsName("station.example".try_into().expect("dns")),
            uri("https://example.org/other"),
            uri(&format!("{STATION_ID_URI_PREFIX}{ID}")),
        ]);
        let id = station_id_from_cert_der(&der).expect("id");
        assert_eq!(id.to_string(), ID);
        assert_eq!(station_uri(id), format!("urn:coaxhub:station:{ID}"));
    }

    #[test]
    fn no_san_extension() {
        assert_eq!(
            station_id_from_cert_der(&cert_with(vec![])),
            Err(IdentityError::NoStationUri)
        );
    }

    #[test]
    fn san_without_station_uri() {
        let der = cert_with(vec![
            SanType::DnsName("x.example".try_into().expect("dns")),
            uri("urn:other:1"),
        ]);
        assert_eq!(
            station_id_from_cert_der(&der),
            Err(IdentityError::NoStationUri)
        );
    }

    #[test]
    fn two_station_uris() {
        let der = cert_with(vec![
            uri(&format!("{STATION_ID_URI_PREFIX}{ID}")),
            uri(&format!(
                "{STATION_ID_URI_PREFIX}{}",
                Uuid::from_u128(7).hyphenated()
            )),
        ]);
        assert_eq!(
            station_id_from_cert_der(&der),
            Err(IdentityError::MultipleStationUris(2))
        );
        // Even the same id twice is refused.
        let der = cert_with(vec![
            uri(&format!("{STATION_ID_URI_PREFIX}{ID}")),
            uri(&format!("{STATION_ID_URI_PREFIX}{ID}")),
        ]);
        assert_eq!(
            station_id_from_cert_der(&der),
            Err(IdentityError::MultipleStationUris(2))
        );
    }

    #[test]
    fn wrong_prefix() {
        for bad in [
            format!("urn:coaxhub:stations:{ID}"),
            format!("URN:COAXHUB:STATION:{ID}"),
            format!("urn:coaxhub:hub:{ID}"),
        ] {
            let der = cert_with(vec![uri(&bad)]);
            assert_eq!(
                station_id_from_cert_der(&der),
                Err(IdentityError::NoStationUri),
                "{bad}"
            );
        }
    }

    #[test]
    fn bad_uuid() {
        for bad in [
            "not-a-uuid".to_owned(),
            String::new(),
            ID.to_uppercase(),
            ID.replace('-', ""),
            format!("{{{ID}}}"),
            format!("urn:uuid:{ID}"),
            format!("{ID}x"),
            Uuid::nil().hyphenated().to_string(),
        ] {
            let der = cert_with(vec![uri(&format!("{STATION_ID_URI_PREFIX}{bad}"))]);
            assert_eq!(
                station_id_from_cert_der(&der),
                Err(IdentityError::InvalidUuid),
                "{bad}"
            );
        }
    }

    #[test]
    fn garbage_der() {
        assert_eq!(
            station_id_from_cert_der(&[]),
            Err(IdentityError::InvalidDer)
        );
        assert_eq!(
            station_id_from_cert_der(b"\x30\x03\x02\x01\x00"),
            Err(IdentityError::InvalidDer)
        );
        assert_eq!(
            station_id_from_cert_der(&[0xff; 64]),
            Err(IdentityError::InvalidDer)
        );
        let mut der = cert_with(vec![uri(&format!("{STATION_ID_URI_PREFIX}{ID}"))]);
        der.push(0);
        assert_eq!(
            station_id_from_cert_der(&der),
            Err(IdentityError::TrailingData)
        );
        // Truncated certificate.
        der.truncate(der.len() / 2);
        assert_eq!(
            station_id_from_cert_der(&der),
            Err(IdentityError::InvalidDer)
        );
    }

    #[test]
    fn uri_parser_directly() {
        assert!(station_id_from_uri(&format!("{STATION_ID_URI_PREFIX}{ID}")).is_ok());
        assert_eq!(station_id_from_uri(ID), Err(IdentityError::NoStationUri));
    }

    #[test]
    fn error_messages() {
        assert_eq!(
            IdentityError::MultipleStationUris(3).to_string(),
            "3 station URI SANs, expected exactly one"
        );
        assert!(!IdentityError::InvalidSubjectAltName.to_string().is_empty());
    }
}
