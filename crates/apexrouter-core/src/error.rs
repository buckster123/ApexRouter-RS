//! OWNER: unit C-01 (core/paths.rs, core/error.rs). Do not edit outside that unit.
//!
//! The one error type every library function in this crate returns. `thiserror` here,
//! `anyhow` in binaries — never the other way round.

use apexrouter_protocol::IdError;

/// Everything that can go wrong below the binary layer.
///
/// Variants are named for what the *operator* needs to know, not for which syscall failed:
/// `PortInUse` names the holder, `InsufficientVram` names the endpoints holding it. That is
/// what makes an error message actionable instead of merely accurate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An I/O failure with the path it happened to.
    #[error("io error at {path}: {source}")]
    Io {
        /// The path involved.
        path: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// An I/O failure with no useful path context.
    #[error(transparent)]
    RawIo(#[from] std::io::Error),
    /// A TOML document would not parse.
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),
    /// A TOML document would not serialise.
    #[error("toml serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),
    /// A JSON document would not parse or serialise.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// An HTTP call failed at the transport level.
    #[error("http error: {0}")]
    Reqwest(#[from] reqwest::Error),
    /// A validated id would not parse.
    #[error("bad id: {0}")]
    Id(#[from] IdError),
    /// The thing asked for does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The thing asked for exists but is wrong.
    #[error("invalid {what}: {why}")]
    Invalid {
        /// What was invalid.
        what: String,
        /// Why.
        why: String,
    },
    /// A credential was needed and no link in the chain produced one.
    #[error(
        "no credential for {0}: set it with `apexrouter provider set`, an env var or a key file"
    )]
    MissingCredential(String),
    /// A port we wanted is taken. Names the holder when we could identify it.
    #[error("port {port} is in use{}", .held_by.as_ref().map(|h| format!(" by {h}")).unwrap_or_default())]
    PortInUse {
        /// The port.
        port: u16,
        /// Who holds it, when identifiable.
        held_by: Option<String>,
    },
    /// The fit solver says no, and `--force` was not given.
    #[error("insufficient VRAM: need {need_mb} MiB, {free_mb} MiB usable")]
    InsufficientVram {
        /// What the plan needs.
        need_mb: u64,
        /// What is actually available.
        free_mb: u64,
    },
    /// Somebody else is already doing this.
    #[error("conflict: {0}")]
    Conflict(String),
    /// A bounded operation ran out of wall clock.
    #[error("timed out after {ms} ms")]
    Timeout {
        /// The deadline that expired.
        ms: u64,
    },
    /// Anything that does not deserve its own variant yet.
    #[error("{0}")]
    Other(String),
}

/// The crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_what_the_operator_needs() {
        let e = Error::PortInUse {
            port: 8888,
            held_by: Some("endpoint_proxy.py (pid 4123)".into()),
        };
        assert_eq!(
            e.to_string(),
            "port 8888 is in use by endpoint_proxy.py (pid 4123)"
        );

        let e = Error::PortInUse {
            port: 8888,
            held_by: None,
        };
        assert_eq!(e.to_string(), "port 8888 is in use");

        let e = Error::InsufficientVram {
            need_mb: 5956,
            free_mb: 3072,
        };
        assert!(e.to_string().contains("5956"), "{e}");
        assert!(e.to_string().contains("3072"), "{e}");

        // A missing credential must tell the operator how to supply one.
        let e = Error::MissingCredential("vast".into());
        assert!(e.to_string().contains("apexrouter provider set"), "{e}");
    }

    #[test]
    fn io_errors_carry_the_path() {
        let e = Error::Io {
            path: "/home/nobody/.local/state/apexrouter/routes.json".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        };
        assert!(e.to_string().contains("routes.json"), "{e}");
        assert!(std::error::Error::source(&e).is_some());
    }

    #[test]
    fn id_errors_convert() {
        let id = apexrouter_protocol::BackendId::parse("Not A Slug").unwrap_err();
        let e: Error = id.into();
        assert!(matches!(e, Error::Id(_)));
    }
}
