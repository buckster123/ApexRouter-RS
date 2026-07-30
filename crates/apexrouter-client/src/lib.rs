//! OWNER: unit CL-01 (client/src/lib.rs, client/src/ws.rs). Do not edit outside that unit.
//!
//! `NodeClient` — the thin HTTP + WebSocket client every non-server surface uses.
//! **No business logic**: the CLI, the MCP server and the Slint app are all edge clients of
//! the same HTTP API, so there is never a second implementation of "what is active".
//!
//! One deliberate detail: a manual status/text check happens **before**
//! `serde_json::from_str`, so a 500 HTML page yields a useful error rather than
//! "expected value at line 1 column 1".

#![allow(unused)]

pub mod ws;

use apexrouter_protocol::{Event, Snapshot};
use futures_util::Stream;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

/// Everything that can go wrong talking to a daemon.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Transport failure.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    /// The daemon answered, but not with what we asked for. Carries the body prefix, which
    /// is what makes an HTML error page debuggable.
    #[error("{status} from {path}: {body}")]
    Status {
        /// HTTP status.
        status: u16,
        /// The path we called.
        path: String,
        /// The first part of the body.
        body: String,
    },
    /// The body was not the JSON we expected.
    #[error("could not parse {path}: {source}")]
    Decode {
        /// The path we called.
        path: String,
        /// The parse failure.
        #[source]
        source: serde_json::Error,
    },
    /// The WebSocket failed.
    #[error("websocket error: {0}")]
    Ws(String),
    /// The URL was not a URL.
    #[error("invalid url: {0}")]
    Url(String),
}

/// The client's result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// A handle on one ApexRouter control plane.
pub struct NodeClient {
    /* CL-01 */
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl NodeClient {
    /// Build a client with a 300 s timeout — long, because a `/v1/endpoints` POST blocks
    /// until the endpoint is `Ready` unless `?no_wait`.
    pub fn new(base: impl Into<String>, token: Option<String>) -> Self {
        NodeClient {
            http: reqwest::Client::new(),
            base: base.into(),
            token,
        }
    }

    /// Attach the bearer, when there is one.
    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        todo!("CL-01: NodeClient::auth")
    }

    /// `GET /health` on the control plane.
    pub async fn health(&self) -> Result<Value> {
        todo!("CL-01: NodeClient::health")
    }

    /// `GET /v1/snapshot`.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        todo!("CL-01: NodeClient::snapshot")
    }

    /// Any `GET`, decoded into a protocol type.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        todo!("CL-01: NodeClient::get")
    }

    /// Any `POST`.
    pub async fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, b: &B) -> Result<T> {
        todo!("CL-01: NodeClient::post")
    }

    /// Any `PUT`.
    pub async fn put<B: Serialize, T: DeserializeOwned>(&self, path: &str, b: &B) -> Result<T> {
        todo!("CL-01: NodeClient::put")
    }

    /// Any `DELETE`.
    pub async fn delete(&self, path: &str) -> Result<()> {
        todo!("CL-01: NodeClient::delete")
    }

    /// Subscribe to `/ws`. Reconnects with backoff and re-emits the snapshot on reconnect.
    pub async fn subscribe(&self) -> Result<impl Stream<Item = Result<Event>>> {
        // CL-01: replace with the real /ws subscription. The empty stream keeps the opaque
        // return type inferable while the body is a stub.
        Ok(futures_util::stream::empty::<Result<Event>>())
    }

    /// Consume one of the SSE endpoints (`/v1/smoke`, `/v1/diagnose`, log follows).
    pub async fn sse(&self, path: &str) -> Result<impl Stream<Item = Result<Event>>> {
        // CL-01: replace with the real SSE reader.
        Ok(futures_util::stream::empty::<Result<Event>>())
    }

    /// The base URL this client was built with.
    pub fn base(&self) -> &str {
        &self.base
    }
}
