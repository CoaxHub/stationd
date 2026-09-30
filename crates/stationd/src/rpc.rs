// SPDX-License-Identifier: GPL-3.0-only
//! gRPC to the control plane (`Enrollment`, `StationControl`): dialing
//! through our own TLS 1.3 [`Connector`] and classifying failures, so
//! each client maps them to its own error type.

use std::sync::Arc;
use std::time::Duration;

use tonic::transport::{Channel, Uri};

use crate::config::Endpoint;
use crate::tls::{ConnectError, Connector};

/// How much of a server message is shown.
const MAX_SERVER_MESSAGE: usize = 200;

/// Why a call failed, before the caller attaches meaning to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The service could not be reached (TCP, DNS, an unavailable server).
    Network(String),
    /// The TLS handshake failed (certificate verification, ALPN).
    Tls(String),
    /// No answer in time.
    Timeout,
    /// The service answered with an error status.
    Status {
        /// The gRPC code.
        code: tonic::Code,
        /// The server's message, escaped and truncated
        /// ([`server_message`]).
        message: String,
    },
}

/// A server message fit for a terminal or a log: control characters
/// escaped, truncated.
pub(crate) fn server_message(message: &str) -> String {
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

/// Opens an HTTP/2 channel to `endpoint` over TLS with `config` (which
/// decides the trust anchor and whether a client certificate is sent).
pub(crate) async fn channel(
    endpoint: &Endpoint,
    config: Arc<rustls::ClientConfig>,
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<Channel, Failure> {
    let authority = endpoint.authority();
    let connector =
        Connector::new(config, endpoint).map_err(|e| Failure::Network(e.to_string()))?;
    let origin: Uri = format!("https://{authority}")
        .parse()
        .map_err(|e| Failure::Network(format!("{e}")))?;
    let ep = tonic::transport::Endpoint::from_shared(format!("http://{authority}"))
        .map_err(|e| Failure::Network(e.to_string()))?
        .origin(origin)
        .connect_timeout(connect_timeout)
        .timeout(request_timeout);
    match tokio::time::timeout(connect_timeout, ep.connect_with_connector(connector)).await {
        Err(_) => Err(Failure::Timeout),
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => Err(connect_failure(&e)),
    }
}

/// Classifies a transport error by its cause: TCP → [`Failure::Network`],
/// TLS or ALPN → [`Failure::Tls`].
pub(crate) fn connect_failure(e: &(dyn std::error::Error + 'static)) -> Failure {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(s) = source {
        if let Some(c) = s.downcast_ref::<ConnectError>() {
            return match c {
                ConnectError::Tcp { .. } => Failure::Network(c.to_string()),
                _ => Failure::Tls(c.to_string()),
            };
        }
        source = s.source();
    }
    Failure::Network(e.to_string())
}

/// Classifies an error status: a transport failure tonic reports as a
/// status goes to [`connect_failure`]; everything else is kept as
/// [`Failure::Status`].
pub(crate) fn status_failure(status: &tonic::Status) -> Failure {
    use tonic::Code;
    match status.code() {
        Code::DeadlineExceeded => Failure::Timeout,
        Code::Unavailable | Code::Unknown | Code::Cancelled => {
            if let Some(source) = std::error::Error::source(status) {
                return connect_failure(source);
            }
            if status.code() == Code::Unavailable {
                return Failure::Network(server_message(status.message()));
            }
            Failure::Status {
                code: status.code(),
                message: server_message(status.message()),
            }
        }
        code => Failure::Status {
            code,
            message: server_message(status.message()),
        },
    }
}

/// Runs `call` with a deadline on the request and on the whole future.
pub(crate) async fn with_timeout<T>(
    timeout: Duration,
    call: impl std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
) -> Result<T, Failure> {
    match tokio::time::timeout(timeout, call).await {
        Err(_) => Err(Failure::Timeout),
        Ok(Ok(r)) => Ok(r.into_inner()),
        Ok(Err(status)) => Err(status_failure(&status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::{Code, Status};

    #[test]
    fn server_messages_are_escaped_and_truncated() {
        assert_eq!(server_message("a\nb\u{1b}"), "a\\nb\\u{1b}");
        let long = "é".repeat(300);
        let m = server_message(&long);
        assert!(m.ends_with('…'));
        assert!(m.len() <= MAX_SERVER_MESSAGE + '…'.len_utf8());
    }

    #[test]
    fn statuses() {
        assert_eq!(
            status_failure(&Status::new(Code::DeadlineExceeded, "")),
            Failure::Timeout
        );
        assert_eq!(
            status_failure(&Status::new(Code::Unavailable, "down")),
            Failure::Network("down".into())
        );
        assert_eq!(
            status_failure(&Status::new(Code::Unknown, "x")),
            Failure::Status {
                code: Code::Unknown,
                message: "x".into()
            }
        );
        assert_eq!(
            status_failure(&Status::new(Code::ResourceExhausted, "slow\ndown")),
            Failure::Status {
                code: Code::ResourceExhausted,
                message: "slow\\ndown".into()
            }
        );
        let with_source = Status::from_error(Box::new(ConnectError::Alpn {
            authority: "a:1".into(),
        }));
        assert!(
            matches!(status_failure(&with_source), Failure::Tls(_)),
            "{with_source:?}"
        );
    }

    #[test]
    fn connect_failures() {
        let io = std::io::Error::other("refused");
        let e = connect_failure(&ConnectError::Tcp {
            authority: "a:1".into(),
            source: io,
        });
        assert!(matches!(e, Failure::Network(_)), "{e:?}");
        let e = connect_failure(&ConnectError::Alpn {
            authority: "a:1".into(),
        });
        assert!(matches!(e, Failure::Tls(_)), "{e:?}");
        let e = connect_failure(&std::io::Error::other("other"));
        assert!(matches!(e, Failure::Network(_)), "{e:?}");
    }

    #[tokio::test]
    async fn timeouts() {
        let never = std::future::pending::<Result<tonic::Response<()>, Status>>();
        assert_eq!(
            with_timeout(Duration::from_millis(10), never).await,
            Err(Failure::Timeout)
        );
        let ok = async { Ok(tonic::Response::new(7)) };
        assert_eq!(with_timeout(Duration::from_secs(1), ok).await, Ok(7));
        let e = async { Err::<tonic::Response<()>, _>(Status::permission_denied("revoked")) };
        assert!(matches!(
            with_timeout(Duration::from_secs(1), e).await,
            Err(Failure::Status {
                code: Code::PermissionDenied,
                ..
            })
        ));
    }
}
