// SPDX-License-Identifier: GPL-3.0-only
//! `stationd enroll`: exchanges a one-time token and a CSR for the
//! station's credentials (station-protocol §8).
//!
//! 1. Generate an ECDSA P-256 key on the station; build a PKCS#10 CSR (the
//!    CA ignores its subject).
//! 2. Call `Enrollment.Enroll` over TLS 1.3 (ALPN h2, no client
//!    certificate), verifying the server against the `CoaxHub` root **only**:
//!    the built-in one, or `[endpoints] root_ca` in development builds.
//! 3. Check the response (the certificate carries our key and the station
//!    ID; the CA bundle and the assignment are usable) and write the state
//!    directory atomically.
//!
//! The token is never logged, echoed or put in an error message, and its
//! buffers are zeroed when dropped.

use std::fmt;
use std::io::{BufRead as _, IsTerminal as _, Read as _, Write as _};
use std::sync::Arc;
use std::time::Duration;

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
use station_proto::identity;
use station_proto::v1::enrollment_client::EnrollmentClient;
use station_proto::v1::{EnrollRequest, EnrollResponse};
use tonic::transport::Uri;
use tracing::info;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::config::{Config, CredentialsMode, Endpoint, Endpoints, TrustAnchor};
use crate::credentials::{CredentialSource as _, EnrolledSource};
use crate::state::{Assignment, Enrollment, StateDir, StateError};
use crate::tls::{self, ConnectError, Connector, TlsError, Validity};

/// The token prefix (station-protocol §8).
pub const TOKEN_PREFIX: &str = "chs_enr_";
/// Crockford base32 characters after the prefix.
pub const TOKEN_BODY_LEN: usize = 32;
/// The Crockford base32 alphabet (no I, L, O, U).
const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Largest accepted `gateway_ca_bundle_pem`.
const MAX_CA_BUNDLE_BYTES: usize = 64 * 1024;
/// Most intermediates accepted in the chain.
const MAX_CHAIN_LEN: usize = 8;
/// Longest accepted hub id.
const MAX_HUB_ID_LEN: usize = 64;
/// How much of a server message is shown.
const MAX_SERVER_MESSAGE: usize = 200;

/// An enrollment token. Its text is zeroed on drop and never printed.
pub struct Token(Zeroizing<String>);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// Checks the format (`chs_enr_` + 32 Crockford base32 characters,
    /// surrounding whitespace ignored) and normalises the body to upper
    /// case. The error never contains the input.
    pub fn parse(input: &str) -> Result<Self, EnrollError> {
        let t = input.trim();
        let body = t
            .strip_prefix(TOKEN_PREFIX)
            .ok_or(EnrollError::MalformedToken)?;
        if body.len() != TOKEN_BODY_LEN
            || !body
                .bytes()
                .all(|b| CROCKFORD.contains(&b.to_ascii_uppercase()))
        {
            return Err(EnrollError::MalformedToken);
        }
        let mut s = Zeroizing::new(String::with_capacity(TOKEN_PREFIX.len() + TOKEN_BODY_LEN));
        s.push_str(TOKEN_PREFIX);
        s.extend(body.chars().map(|c| c.to_ascii_uppercase()));
        Ok(Self(s))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

/// Why enrollment failed. Messages never contain the token.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnrollError {
    /// The config uses static `[credentials]`.
    #[error("stationd.toml has static [credentials]; remove them to enroll this station")]
    StaticCredentials,
    /// No enrollment endpoint (development build without `[endpoints]`).
    #[error(
        "no enrollment endpoint: this development build needs an [endpoints] section in stationd.toml"
    )]
    NoEndpoints,
    /// Already enrolled and `--force` not given.
    #[error(
        "this station is already enrolled (station {station_id}); use --force to enroll it again"
    )]
    AlreadyEnrolled {
        /// The enrolled station.
        station_id: Uuid,
    },
    /// No token was given.
    #[error("no enrollment token given")]
    NoToken,
    /// The token does not have the `chs_enr_…` format.
    #[error(
        "that is not an enrollment token: expected chs_enr_ followed by 32 letters and digits \
         (copy it again from the web app)"
    )]
    MalformedToken,
    /// The token could not be read.
    #[error("cannot read the enrollment token: {0}")]
    ReadToken(std::io::Error),
    /// The control plane refused the token.
    #[error("{0}")]
    Token(TokenProblem),
    /// The trust anchor cannot be loaded.
    #[error("cannot load the CoaxHub root certificate ({anchor}): {reason}")]
    TrustAnchor {
        /// Which anchor.
        anchor: String,
        /// What is wrong.
        reason: String,
    },
    /// The control plane could not be reached.
    #[error("cannot reach the enrollment service at {endpoint}: {detail}")]
    Network {
        /// The endpoint.
        endpoint: String,
        /// What happened.
        detail: String,
    },
    /// The TLS handshake failed (for example, a server certificate that
    /// does not chain to the `CoaxHub` root or is for another name).
    #[error(
        "TLS with the enrollment service at {endpoint} failed: {detail}; stationd trusts only \
         the CoaxHub root"
    )]
    Tls {
        /// The endpoint.
        endpoint: String,
        /// What happened.
        detail: String,
    },
    /// No answer in time.
    #[error("the enrollment service at {endpoint} did not answer in time")]
    Timeout {
        /// The endpoint.
        endpoint: String,
    },
    /// The control plane refused the request for another reason.
    #[error("the enrollment service refused the request ({code}): {message}")]
    Refused {
        /// The gRPC status code.
        code: String,
        /// The server's message (truncated, escaped).
        message: String,
    },
    /// The response is unusable.
    #[error("the enrollment service sent an unusable response: {0}")]
    BadResponse(String),
    /// Key or CSR generation failed.
    #[error("cannot generate the station key: {0}")]
    Key(String),
    /// The state directory cannot be written.
    #[error("cannot store the credentials: {0}")]
    State(#[from] StateError),
}

/// Why the control plane refused a token (gRPC `UNAUTHENTICATED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenProblem {
    /// Unknown or wrong.
    Invalid,
    /// Older than 15 minutes.
    Expired,
    /// Used before.
    AlreadyUsed,
}

impl fmt::Display for TokenProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Invalid => {
                "the enrollment token was not accepted; create a new one in the web app"
            }
            Self::Expired => {
                "the enrollment token has expired (tokens last 15 minutes); create a new one in the web app"
            }
            Self::AlreadyUsed => {
                "the enrollment token was already used; create a new one in the web app"
            }
        })
    }
}

impl TokenProblem {
    /// Classifies an `UNAUTHENTICATED` message: `expired`, `used`,
    /// otherwise invalid.
    pub fn from_message(message: &str) -> Self {
        let m = message.to_ascii_lowercase();
        if m.contains("expired") {
            Self::Expired
        } else if m.contains("used") {
            Self::AlreadyUsed
        } else {
            Self::Invalid
        }
    }
}

/// Timeouts for [`enroll`].
#[derive(Debug, Clone, Copy)]
pub struct EnrollOptions {
    /// Enroll even if the station is already enrolled.
    pub force: bool,
    /// TCP + TLS connect timeout.
    pub connect_timeout: Duration,
    /// Timeout for the `Enroll` call.
    pub request_timeout: Duration,
}

impl Default for EnrollOptions {
    fn default() -> Self {
        Self {
            force: false,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
        }
    }
}

/// A successful enrollment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolled {
    /// The station ID.
    pub station_id: Uuid,
    /// The home hub.
    pub assignment: Assignment,
    /// The certificate's validity.
    pub validity: Validity,
}

/// Reads the token: `arg` if given, else a hidden prompt on a terminal,
/// else the first line of standard input.
pub fn read_token(arg: Option<String>) -> Result<Token, EnrollError> {
    if let Some(a) = arg {
        let a = Zeroizing::new(a);
        return Token::parse(&a);
    }
    let stdin = std::io::stdin();
    let line = if stdin.is_terminal() {
        prompt_hidden(&stdin).map_err(EnrollError::ReadToken)?
    } else {
        let mut line = Zeroizing::new(String::new());
        stdin
            .lock()
            .take(1024)
            .read_line(&mut line)
            .map_err(EnrollError::ReadToken)?;
        line
    };
    if line.trim().is_empty() {
        return Err(EnrollError::NoToken);
    }
    Token::parse(&line)
}

/// Prompts on stderr and reads a line from the terminal without echo.
fn prompt_hidden(stdin: &std::io::Stdin) -> std::io::Result<Zeroizing<String>> {
    use rustix::termios::{self, LocalModes, OptionalActions};
    let mut stderr = std::io::stderr();
    write!(stderr, "Enrollment token (input hidden): ")?;
    stderr.flush()?;
    let saved = termios::tcgetattr(stdin)?;
    let mut quiet = saved.clone();
    quiet.local_modes.remove(LocalModes::ECHO);
    termios::tcsetattr(stdin, OptionalActions::Now, &quiet)?;
    let mut line = Zeroizing::new(String::new());
    let read = stdin.lock().take(1024).read_line(&mut line);
    let restored = termios::tcsetattr(stdin, OptionalActions::Now, &saved);
    writeln!(stderr)?;
    read?;
    restored?;
    Ok(line)
}

/// A fresh station key and a CSR for it.
pub struct KeyAndCsr {
    /// The private key, PKCS#8 PEM (zeroed on drop).
    pub key_pem: Zeroizing<String>,
    /// The raw public key (uncompressed P-256 point).
    pub public_key: Vec<u8>,
    /// The CSR, DER.
    pub csr_der: Vec<u8>,
}

impl fmt::Debug for KeyAndCsr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyAndCsr").finish_non_exhaustive()
    }
}

/// Generates an ECDSA P-256 key and a CSR signed with it. The
/// subject is a placeholder: the CA ignores it.
pub fn generate_key() -> Result<KeyAndCsr, EnrollError> {
    use rcgen::PublicKeyData as _;
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map_err(|e| EnrollError::Key(e.to_string()))?;
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "coaxhub-station");
    let csr = params
        .serialize_request(&key)
        .map_err(|e| EnrollError::Key(e.to_string()))?;
    Ok(KeyAndCsr {
        key_pem: Zeroizing::new(key.serialize_pem()),
        public_key: key.der_bytes().to_vec(),
        csr_der: csr.der().to_vec(),
    })
}

/// The root store for `anchor`.
pub fn trust_anchor_roots(anchor: &TrustAnchor) -> Result<rustls::RootCertStore, EnrollError> {
    let fail = |reason: String| EnrollError::TrustAnchor {
        anchor: anchor.to_string(),
        reason,
    };
    let pem_text = match anchor {
        TrustAnchor::BuiltIn(pem) => (*pem).to_owned(),
        TrustAnchor::File(path) => {
            std::fs::read_to_string(path).map_err(|e| fail(e.to_string()))?
        }
    };
    tls::root_store(tls::parse_certs(&pem_text).map_err(fail)?).map_err(fail)
}

/// Checks that `config` can enroll (no static credentials, an enrollment
/// endpoint, not enrolled yet unless `force`), before asking for a token.
pub fn preflight(config: &Config, force: bool) -> Result<&Endpoints, EnrollError> {
    if matches!(config.credentials, CredentialsMode::Static(_)) {
        return Err(EnrollError::StaticCredentials);
    }
    let endpoints = config.endpoints.as_ref().ok_or(EnrollError::NoEndpoints)?;
    if !force {
        if let Ok(c) = EnrolledSource(StateDir::new(&config.state_dir)).load() {
            return Err(EnrollError::AlreadyEnrolled {
                station_id: c.tls.station_id,
            });
        }
    }
    Ok(endpoints)
}

/// Enrolls the station `config` describes with `token` and writes the
/// state directory.
pub async fn enroll(
    config: &Config,
    token: Token,
    options: EnrollOptions,
) -> Result<Enrolled, EnrollError> {
    let endpoints = preflight(config, options.force)?;
    let state = StateDir::new(&config.state_dir);
    let key = generate_key()?;
    info!(endpoint = %endpoints.enroll.url, root = %endpoints.root, "enrolling this station");
    let response = call_enroll(endpoints, token, key.csr_der.clone(), options).await?;
    let checked = check_response(response, &key.public_key)?;
    state.write_enrollment(&Enrollment {
        station_id: checked.station_id,
        key_pem: &key.key_pem,
        cert_pem: &checked.cert_pem,
        chain_pem: &checked.chain_pem,
        gateway_ca_pem: &checked.gateway_ca_pem,
        assignment: &checked.assignment,
    })?;
    info!(
        station_id = %checked.station_id,
        hub = %checked.assignment.hub_id,
        gateway = %checked.assignment.gateway_url,
        "enrolled"
    );
    Ok(Enrolled {
        station_id: checked.station_id,
        assignment: checked.assignment,
        validity: checked.validity,
    })
}

/// Sends `Enroll` to `endpoints.enroll`, trusting only `endpoints.root`.
/// `token` is dropped (and zeroed) as soon as the request is encoded.
pub async fn call_enroll(
    endpoints: &Endpoints,
    token: Token,
    csr_der: Vec<u8>,
    options: EnrollOptions,
) -> Result<EnrollResponse, EnrollError> {
    let endpoint = &endpoints.enroll;
    let roots = trust_anchor_roots(&endpoints.root)?;
    let tls_config = tls::client_config(roots, None).map_err(|e| EnrollError::Tls {
        endpoint: endpoint.url.clone(),
        detail: e.to_string(),
    })?;
    let channel = channel(endpoint, Arc::new(tls_config), options).await?;
    let mut client = EnrollmentClient::new(channel);
    let request = EnrollRequest {
        token: token.expose().to_owned(),
        csr_der,
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    drop(token);
    let mut request = tonic::Request::new(request);
    request.set_timeout(options.request_timeout);
    let call = client.enroll(request);
    match tokio::time::timeout(options.request_timeout, call).await {
        Err(_) => Err(EnrollError::Timeout {
            endpoint: endpoint.url.clone(),
        }),
        Ok(Ok(r)) => Ok(r.into_inner()),
        Ok(Err(status)) => Err(status_error(endpoint, &status)),
    }
}

async fn channel(
    endpoint: &Endpoint,
    config: Arc<rustls::ClientConfig>,
    options: EnrollOptions,
) -> Result<tonic::transport::Channel, EnrollError> {
    let authority = endpoint.authority();
    let bad = |detail: String| EnrollError::Network {
        endpoint: endpoint.url.clone(),
        detail,
    };
    let connector = Connector::new(config, endpoint).map_err(|e: TlsError| bad(e.to_string()))?;
    let origin: Uri = format!("https://{authority}")
        .parse()
        .map_err(|e| bad(format!("{e}")))?;
    let ep = tonic::transport::Endpoint::from_shared(format!("http://{authority}"))
        .map_err(|e| bad(e.to_string()))?
        .origin(origin)
        .connect_timeout(options.connect_timeout)
        .timeout(options.request_timeout);
    match tokio::time::timeout(
        options.connect_timeout,
        ep.connect_with_connector(connector),
    )
    .await
    {
        Err(_) => Err(EnrollError::Timeout {
            endpoint: endpoint.url.clone(),
        }),
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => Err(connect_error(endpoint, &e)),
    }
}

/// Maps a transport error to [`EnrollError::Tls`] or
/// [`EnrollError::Network`] by its cause.
fn connect_error(endpoint: &Endpoint, e: &(dyn std::error::Error + 'static)) -> EnrollError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(s) = source {
        if let Some(c) = s.downcast_ref::<ConnectError>() {
            return match c {
                ConnectError::Tcp { .. } => EnrollError::Network {
                    endpoint: endpoint.url.clone(),
                    detail: c.to_string(),
                },
                _ => EnrollError::Tls {
                    endpoint: endpoint.url.clone(),
                    detail: c.to_string(),
                },
            };
        }
        source = s.source();
    }
    EnrollError::Network {
        endpoint: endpoint.url.clone(),
        detail: e.to_string(),
    }
}

/// A server message fit for a terminal: control characters escaped,
/// truncated.
fn server_message(message: &str) -> String {
    let escaped: String = message.escape_debug().collect();
    let mut cut = escaped.len().min(MAX_SERVER_MESSAGE);
    while !escaped.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = escaped[..cut].to_owned();
    if cut < escaped.len() {
        out.push('…');
    }
    out
}

fn status_error(endpoint: &Endpoint, status: &tonic::Status) -> EnrollError {
    use tonic::Code;
    match status.code() {
        Code::Unauthenticated => EnrollError::Token(TokenProblem::from_message(status.message())),
        Code::DeadlineExceeded => EnrollError::Timeout {
            endpoint: endpoint.url.clone(),
        },
        Code::Unavailable | Code::Unknown | Code::Cancelled => {
            if let Some(source) = std::error::Error::source(status) {
                return connect_error(endpoint, source);
            }
            if status.code() == Code::Unavailable {
                return EnrollError::Network {
                    endpoint: endpoint.url.clone(),
                    detail: server_message(status.message()),
                };
            }
            EnrollError::Refused {
                code: format!("{:?}", status.code()),
                message: server_message(status.message()),
            }
        }
        code => EnrollError::Refused {
            code: format!("{code:?}"),
            message: server_message(status.message()),
        },
    }
}

/// A checked `EnrollResponse`, as PEM.
#[derive(Debug)]
struct Checked {
    station_id: Uuid,
    cert_pem: String,
    chain_pem: String,
    gateway_ca_pem: String,
    assignment: Assignment,
    validity: Validity,
}

fn pem_cert(der: &[u8]) -> String {
    pem::encode_config(
        &pem::Pem::new("CERTIFICATE", der),
        pem::EncodeConfig::new().set_line_ending(pem::LineEnding::LF),
    )
}

fn hub_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_HUB_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Checks that the certificate identifies `station_id`, carries our key and
/// is valid now, and that the CA bundle and assignment are usable.
fn check_response(r: EnrollResponse, public_key: &[u8]) -> Result<Checked, EnrollError> {
    use x509_parser::prelude::FromDer as _;
    let bad = |m: &str| EnrollError::BadResponse(m.to_owned());
    let station_id = Uuid::try_parse(&r.station_id)
        .ok()
        .filter(|u| !u.is_nil())
        .ok_or_else(|| bad("station_id is not a UUID"))?;
    let cert = r.certificate.ok_or_else(|| bad("no certificate"))?;
    let (rest, leaf) = x509_parser::certificate::X509Certificate::from_der(&cert.certificate_der)
        .map_err(|_| bad("the certificate is not DER X.509"))?;
    if !rest.is_empty() {
        return Err(bad("trailing bytes after the certificate"));
    }
    let san_id = identity::station_id_from_cert_der(&cert.certificate_der)
        .map_err(|e| EnrollError::BadResponse(format!("the certificate: {e}")))?;
    if san_id != station_id {
        return Err(bad("the certificate is for another station"));
    }
    if leaf.public_key().subject_public_key.data.as_ref() != public_key {
        return Err(bad("the certificate is not for this station's key"));
    }
    let validity = Validity::of_der(&cert.certificate_der).ok_or_else(|| bad("no validity"))?;
    let now = crate::local_status::unix_now();
    if validity.not_after_unix_s <= now {
        return Err(bad(
            "the certificate has already expired (is this station's clock right?)",
        ));
    }
    if cert.chain_der.len() > MAX_CHAIN_LEN {
        return Err(bad("the certificate chain is too long"));
    }
    let mut chain_pem = String::new();
    for der in &cert.chain_der {
        if x509_parser::certificate::X509Certificate::from_der(der).is_err() {
            return Err(bad("a chain certificate is not DER X.509"));
        }
        chain_pem.push_str(&pem_cert(der));
    }
    if r.gateway_ca_bundle_pem.len() > MAX_CA_BUNDLE_BYTES {
        return Err(bad("the gateway CA bundle is too large"));
    }
    let roots = tls::parse_certs(&r.gateway_ca_bundle_pem)
        .map_err(|e| EnrollError::BadResponse(format!("the gateway CA bundle: {e}")))?;
    tls::root_store(roots.clone())
        .map_err(|e| EnrollError::BadResponse(format!("the gateway CA bundle: {e}")))?;
    let gateway_ca_pem: String = roots.iter().map(|c| pem_cert(c)).collect();
    let a = r.assignment.ok_or_else(|| bad("no assignment"))?;
    if !hub_id_ok(&a.hub_id) {
        return Err(bad("the assignment's hub_id is malformed"));
    }
    Endpoint::parse("assignment.gateway_url", &a.gateway_url)
        .map_err(|e| EnrollError::BadResponse(e.to_string()))?;
    Ok(Checked {
        station_id,
        cert_pem: pem_cert(&cert.certificate_der),
        chain_pem,
        gateway_ca_pem,
        assignment: Assignment {
            hub_id: a.hub_id,
            gateway_url: a.gateway_url,
        },
        validity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const GOOD: &str = "chs_enr_0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    #[test]
    fn token_format() {
        assert_eq!(Token::parse(GOOD).expect("good").expose(), GOOD);
        let lower = format!("  {}\n", GOOD.to_ascii_lowercase());
        assert_eq!(Token::parse(&lower).expect("lower").expose(), GOOD);
        for bad in [
            "",
            "chs_enr_",
            &GOOD[..GOOD.len() - 1],
            &format!("{GOOD}Y"),
            &GOOD.replace('A', "I"),
            &GOOD.replace('A', "U"),
            &GOOD.replace("chs_enr_", "chs_enx_"),
            &GOOD.replace("chs_enr_", "CHS_ENR_"),
            &GOOD.replace('A', "é"),
        ] {
            assert!(
                matches!(Token::parse(bad), Err(EnrollError::MalformedToken)),
                "{bad:?}"
            );
        }
        let t = Token::parse(GOOD).expect("good");
        assert_eq!(format!("{t:?}"), "Token(<redacted>)");
        assert!(!EnrollError::MalformedToken.to_string().contains("0123"));
        assert!(matches!(
            read_token(Some("nope".into())),
            Err(EnrollError::MalformedToken)
        ));
        assert!(read_token(Some(GOOD.into())).is_ok());
    }

    #[test]
    fn token_problems() {
        assert_eq!(
            TokenProblem::from_message("token expired"),
            TokenProblem::Expired
        );
        assert_eq!(
            TokenProblem::from_message("token already USED"),
            TokenProblem::AlreadyUsed
        );
        assert_eq!(
            TokenProblem::from_message("invalid token"),
            TokenProblem::Invalid
        );
        assert_eq!(TokenProblem::from_message(""), TokenProblem::Invalid);
        for p in [
            TokenProblem::Invalid,
            TokenProblem::Expired,
            TokenProblem::AlreadyUsed,
        ] {
            assert!(p.to_string().contains("web app"));
        }
    }

    #[test]
    fn key_and_csr() {
        let k = generate_key().expect("key");
        assert!(k.key_pem.contains("BEGIN PRIVATE KEY"));
        assert_eq!(k.public_key.len(), 65, "uncompressed P-256 point");
        let csr = rcgen::CertificateSigningRequestParams::from_der(&k.csr_der.clone().into())
            .expect("valid, self-signed CSR");
        assert_eq!(csr.public_key.algorithm(), &PKCS_ECDSA_P256_SHA256);
        assert!(!format!("{k:?}").contains("PRIVATE"));
    }

    #[test]
    fn server_messages_are_escaped_and_truncated() {
        assert_eq!(server_message("a\nb\u{1b}"), "a\\nb\\u{1b}");
        let long = "é".repeat(300);
        let m = server_message(&long);
        assert!(m.ends_with('…'));
        assert!(m.len() <= MAX_SERVER_MESSAGE + '…'.len_utf8());
    }

    #[test]
    fn status_mapping() {
        use tonic::{Code, Status};
        let ep = Endpoint::parse("x", "https://enroll.example").expect("ep");
        assert!(matches!(
            status_error(&ep, &Status::new(Code::Unauthenticated, "expired")),
            EnrollError::Token(TokenProblem::Expired)
        ));
        assert!(matches!(
            status_error(&ep, &Status::new(Code::DeadlineExceeded, "")),
            EnrollError::Timeout { .. }
        ));
        assert!(matches!(
            status_error(&ep, &Status::new(Code::Unavailable, "down")),
            EnrollError::Network { .. }
        ));
        let e = status_error(&ep, &Status::new(Code::ResourceExhausted, "slow\ndown"));
        assert_eq!(
            e.to_string(),
            "the enrollment service refused the request (ResourceExhausted): slow\\ndown"
        );
        assert!(matches!(
            status_error(&ep, &Status::new(Code::Unknown, "x")),
            EnrollError::Refused { .. }
        ));
        let io = std::io::Error::other("refused");
        let e = connect_error(
            &ep,
            &ConnectError::Tcp {
                authority: "a:1".into(),
                source: io,
            },
        );
        assert!(matches!(e, EnrollError::Network { .. }), "{e}");
        let e = connect_error(
            &ep,
            &ConnectError::Alpn {
                authority: "a:1".into(),
            },
        );
        assert!(matches!(e, EnrollError::Tls { .. }), "{e}");
        let e = connect_error(&ep, &std::io::Error::other("other"));
        assert!(matches!(e, EnrollError::Network { .. }), "{e}");
    }

    #[test]
    fn hub_ids() {
        assert!(hub_id_ok("sa1"));
        assert!(hub_id_ok("sa1-2"));
        assert!(!hub_id_ok(""));
        assert!(!hub_id_ok("sa 1"));
        assert!(!hub_id_ok(&"a".repeat(65)));
    }

    #[test]
    fn trust_anchors() {
        let e = trust_anchor_roots(&TrustAnchor::BuiltIn("")).expect_err("empty");
        assert!(e.to_string().contains("built-in"), "{e}");
        let e =
            trust_anchor_roots(&TrustAnchor::File("/nonexistent.pem".into())).expect_err("missing");
        assert!(e.to_string().contains("/nonexistent.pem"), "{e}");
    }

    proptest! {
        /// Arbitrary input never panics, and a refusal carries no data (so
        /// it cannot quote the input).
        #[test]
        fn token_parse_never_leaks(s in "\\PC{0,64}") {
            if let Err(e) = Token::parse(&s) {
                prop_assert!(matches!(e, EnrollError::MalformedToken));
            }
        }

        /// Every well-formed token is accepted and normalised.
        #[test]
        fn well_formed_tokens(body in "[0-9A-HJKMNP-TV-Za-hjkmnp-tv-z]{32}") {
            let t = Token::parse(&format!("chs_enr_{body}")).expect("valid");
            prop_assert_eq!(t.expose(), format!("chs_enr_{}", body.to_ascii_uppercase()));
        }
    }
}
