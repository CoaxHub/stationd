// SPDX-License-Identifier: GPL-3.0-only
//! `tracing` setup for the binary.
//!
//! - Filter: `RUST_LOG` (default `info`).
//! - Format: `STATIOND_LOG_FORMAT=json` for one JSON object per line
//!   (journald/log shippers); anything else, or unset, for human-readable
//!   text.
//! - Output: stderr. Colours only when stderr is a terminal, so the
//!   journal (systemd) and redirected logs get plain text.
//!
//! Nothing in `stationd` logs key material; hub-authored strings are logged
//! with `Debug` formatting so control characters are escaped.

use tracing_subscriber::EnvFilter;

/// The environment variable selecting the log format.
pub const FORMAT_ENV: &str = "STATIOND_LOG_FORMAT";

/// The log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Human-readable lines.
    Text,
    /// One JSON object per line.
    Json,
}

impl Format {
    /// The format named by `value` (`json`, case-insensitive; else text).
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value {
            Some(v) if v.trim().eq_ignore_ascii_case("json") => Self::Json,
            _ => Self::Text,
        }
    }
}

/// Installs the global subscriber. Returns `false` if one was already set.
pub fn init() -> bool {
    let format = Format::from_env_value(std::env::var(FORMAT_ENV).ok().as_deref());
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr);
    match format {
        Format::Json => builder.json().try_init().is_ok(),
        Format::Text => builder.try_init().is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_from_env() {
        assert_eq!(Format::from_env_value(Some("json")), Format::Json);
        assert_eq!(Format::from_env_value(Some(" JSON ")), Format::Json);
        assert_eq!(Format::from_env_value(Some("text")), Format::Text);
        assert_eq!(Format::from_env_value(None), Format::Text);
    }

    #[test]
    fn init_twice_is_harmless() {
        let _ = init();
        assert!(!init());
    }
}
