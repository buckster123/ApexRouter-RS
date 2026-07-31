//! The stub OpenAI upstream: the same server, in this process, on a loopback port.
//!
//! Use this when the thing under test is the **router** — swap, failover, the Anthropic
//! translation, the usage tee. There is no binary to find, no supervisor, no argv: it
//! binds a port, hands you a [`Backend`] to put in the routing table, and lets you read
//! back every request it received.
//!
//! Use [`crate::FakeBuild`] instead when the thing under test is the **supervisor**.

use crate::behavior::Behavior;
use crate::record::LaunchRecord;
use crate::routing;
use crate::server::{Config, RecordedRequest, Server, State};
use apexrouter_protocol::Backend;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::JoinHandle;

/// The argv a stub pretends it was launched with: `--props`, `--metrics` and `--slots`
/// enabled, four slots, a 32k context and one advertised model.
const DEFAULT_ARGV: &[&str] = &[
    "-a",
    "stub-model",
    "-c",
    "32768",
    "-np",
    "4",
    "--props",
    "--metrics",
    "--slots",
];

/// A running in-process fake upstream. Dropping it stops accepting.
pub struct Stub {
    state: Arc<State>,
    addr: SocketAddr,
    accept: Option<JoinHandle<()>>,
}

impl Stub {
    /// Start one with default behaviour, advertising `stub-model`.
    ///
    /// # Panics
    /// Only if loopback cannot be bound, which is not a condition a test can handle.
    pub fn start() -> Stub {
        Stub::with("")
    }

    /// Start one with a behaviour spec — see [`crate::Behavior`].
    ///
    /// ```no_run
    /// let slow = apexrouter_tests_support::Stub::with("ttft_ms=250,chunks=16,chunk_ms=20");
    /// let flaky = apexrouter_tests_support::Stub::with("fail_first=2");
    /// ```
    ///
    /// # Panics
    /// If loopback cannot be bound.
    pub fn with(spec: &str) -> Stub {
        Stub::with_argv(DEFAULT_ARGV, spec)
    }

    /// Start one that also pretends to have been launched with this argv, which is what
    /// `/props`, `/v1/models` and `/slots` answer from.
    ///
    /// # Panics
    /// If loopback cannot be bound.
    pub fn with_argv(argv: &[&str], spec: &str) -> Stub {
        match Stub::try_with_argv(argv, spec) {
            Ok(s) => s,
            Err(e) => panic!("could not start the stub upstream: {e}"),
        }
    }

    /// As [`Stub::with_argv`], reporting instead of panicking.
    ///
    /// # Errors
    /// A rendered bind failure.
    pub fn try_with_argv(argv: &[&str], spec: &str) -> Result<Stub, String> {
        let server = Server::bind(Config {
            host: "127.0.0.1".to_owned(),
            port: 0,
            record: LaunchRecord::synthetic(argv),
            behavior: Behavior::parse(spec),
            log: Box::new(|_| {}),
            // Never: `exit_after_ms` and `POST /_apex/exit` would take the test binary
            // with them. In-process they stop the stub and turn it unhealthy instead.
            allow_process_exit: false,
        })
        .map_err(|e| e.to_string())?;

        let state = server.state();
        let addr = server.addr();
        // No load simulation by default: an upstream in a routing table is already up.
        state.ready.store(true, Ordering::SeqCst);
        if state.behavior().load_ms > 0 || state.behavior().loading_forever {
            state.ready.store(false, Ordering::SeqCst);
            server.start_loading();
        }
        let accept = std::thread::spawn(move || server.serve());

        Ok(Stub {
            state,
            addr,
            accept: Some(accept),
        })
    }

    /// `http://127.0.0.1:<port>` — **without** a trailing `/v1`, which is how every
    /// `Backend::base_url` in this codebase is stored.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The port it bound.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// A routing-table [`Backend`] pointing at this stub, `Ready`, advertising whatever
    /// `/v1/models` would.
    pub fn backend(&self, id: &str) -> Backend {
        let models: Vec<String> = self.state.model_ids();
        let refs: Vec<&str> = models.iter().map(String::as_str).collect();
        routing::backend(id, &self.base_url(), &refs)
    }

    /// Every request this stub received, oldest first.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        match self.state.requests.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// The most recent request — the one an assertion usually means.
    pub fn last_request(&self) -> Option<RecordedRequest> {
        self.requests().pop()
    }

    /// How many requests it has seen.
    pub fn hits(&self) -> usize {
        self.requests().len()
    }

    /// Forget the recorded requests.
    pub fn clear(&self) {
        match self.state.requests.lock() {
            Ok(mut g) => g.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
    }

    /// Change its mind while a test watches — `stub.set_behavior("chat_status=503")` is how
    /// a breaker is made to open.
    pub fn set_behavior(&self, spec: &str) {
        match self.state.behavior.lock() {
            Ok(mut g) => g.apply_spec(spec),
            Err(poisoned) => poisoned.into_inner().apply_spec(spec),
        }
    }

    /// The shared state, for assertions this API does not cover.
    pub fn state(&self) -> Arc<State> {
        Arc::clone(&self.state)
    }
}

impl Default for Stub {
    fn default() -> Self {
        Stub::start()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::SeqCst);
        // The accept loop is blocked in `accept()`; one connection wakes it so it can see
        // the flag. In-flight handler threads are detached and die with the process.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}
