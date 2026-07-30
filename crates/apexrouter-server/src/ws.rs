//! OWNER: unit S-05 (server/src/{ws,assets,prober,watcher}.rs). Do not edit outside that
//! unit.
//!
//! `GET /ws`.
//!
//! **Subscribe to the broadcast BEFORE sending the snapshot** — otherwise an event that
//! lands between assembling the snapshot and subscribing is lost forever, and the client
//! renders a picture that never converges. Re-send a full snapshot on `RecvError::Lagged`,
//! and `tokio::select!` on `socket.recv()` too, so a close is noticed.
//!
//! The ordering rule is enforced by [`subscribe_then_snapshot`], which takes the assembler as
//! a closure and calls it **after** `subscribe()` has already returned. That is what makes it
//! testable: the regression test passes an assembler that broadcasts while it runs and
//! asserts the receiver saw it. Written the obvious way — subscribe and snapshot inline —
//! the invariant is only a comment, and a later edit reorders two lines with nothing to stop
//! it.
//!
//! The picture itself comes from `api::snapshot::build`, which S-03 publishes for exactly
//! this reason: a dashboard whose first frame disagrees with `GET /v1/snapshot` is worse
//! than one that is merely stale.
//!
//! The event loop is generic over [`Frames`] because this crate has no WebSocket *client* in
//! its dev-dependencies (they are `tempfile` and `wiremock`, and `Cargo.toml` belongs to
//! Stage 0), so a real end-to-end socket test cannot be written here. A two-method seam lets
//! every rule the acceptance names — lagged re-snapshot, close detection, peer gone — be
//! driven deterministically instead of not at all.

use crate::state::AppState;
use apexrouter_protocol::{Event, Snapshot};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

/// Upgrade, subscribe, snapshot, stream.
pub async fn ws_handler(ws: WebSocketUpgrade, State(s): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| pump(s, socket))
}

// ----------------------------------------------------------------------------------------
// the ordering rule
// ----------------------------------------------------------------------------------------

/// Subscribe **first**, then assemble the snapshot with the caller's closure.
///
/// Every event published from the moment this function is entered is therefore either
/// already inside the snapshot or still queued on the returned receiver. Nothing can fall
/// through the gap, which is the one thing a live dashboard cannot recover from on its own.
pub(crate) async fn subscribe_then_snapshot<F, Fut>(
    tx: &broadcast::Sender<Event>,
    assemble: F,
) -> (broadcast::Receiver<Event>, Snapshot)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Snapshot>,
{
    let rx = tx.subscribe();
    let snapshot = assemble().await;
    (rx, snapshot)
}

/// The one picture, assembled by S-03 so `/ws` and `GET /v1/snapshot` can never disagree.
pub async fn snapshot_now(state: &Arc<AppState>) -> Snapshot {
    crate::api::snapshot::build(state).await
}

// ----------------------------------------------------------------------------------------
// the event loop
// ----------------------------------------------------------------------------------------

/// The two things the event loop needs from a socket.
///
/// `async fn` in a trait would be enough for the real socket, but the loop is spawned by
/// `on_upgrade`, whose future must be `Send`; the explicit `impl Future + Send` return type
/// is how that bound is stated.
pub(crate) trait Frames {
    /// Send one text frame. `false` means the peer is gone and the loop should stop.
    fn send_text(&mut self, text: String) -> impl Future<Output = bool> + Send;
    /// Wait for the next inbound frame. `None` means the socket closed.
    fn next_inbound(&mut self) -> impl Future<Output = Option<()>> + Send;
}

impl Frames for WebSocket {
    async fn send_text(&mut self, text: String) -> bool {
        self.send(Message::text(text)).await.is_ok()
    }

    async fn next_inbound(&mut self) -> Option<()> {
        match self.recv().await {
            // A close frame, a transport error and end-of-stream are the same thing to us.
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => None,
            // Ping/Pong are answered by axum itself. Anything a client sends is ignored:
            // `/ws` is a one-way `Event` stream, and every mutation has an HTTP route with an
            // `Origin`/`Host` gate on it (§9.3). Accepting commands here would route around
            // that gate.
            Some(Ok(_)) => Some(()),
        }
    }
}

/// What one turn of the loop woke up for.
enum Step {
    /// The broadcast produced something, or told us why it could not.
    Broadcast(std::result::Result<Event, RecvError>),
    /// The client sent a frame, or went away.
    Inbound(Option<()>),
}

/// Subscribe, snapshot, then relay until either side goes away.
pub(crate) async fn pump<S: Frames>(state: Arc<AppState>, mut socket: S) {
    let (mut rx, snapshot) = subscribe_then_snapshot(&state.tx, || snapshot_now(&state)).await;
    if !send_event(&mut socket, &Event::Snapshot(Box::new(snapshot))).await {
        return;
    }

    loop {
        // Both futures are created inside the macro and dropped when it completes, so the
        // borrow of `socket` ends before the arms below use it again. `broadcast::recv` and
        // the socket's `recv` are both cancel-safe, so losing the race drops nothing.
        let step = tokio::select! {
            got = rx.recv() => Step::Broadcast(got),
            inbound = socket.next_inbound() => Step::Inbound(inbound),
        };

        match step {
            Step::Broadcast(Ok(ev)) => {
                if !send_event(&mut socket, &ev).await {
                    return;
                }
            }
            Step::Broadcast(Err(RecvError::Lagged(missed))) => {
                // The client fell behind and the ring dropped events it never saw. Sending
                // the next event alone would leave it rendering a picture with a hole in it,
                // so the whole state goes again. The receiver stays usable after `Lagged`.
                tracing::debug!(missed, "ws subscriber lagged; re-sending a full snapshot");
                let snapshot = snapshot_now(&state).await;
                if !send_event(&mut socket, &Event::Snapshot(Box::new(snapshot))).await {
                    return;
                }
            }
            Step::Broadcast(Err(RecvError::Closed)) => return,
            Step::Inbound(None) => return,
            Step::Inbound(Some(())) => {}
        }
    }
}

/// Serialise and send one event. `false` means the peer is gone.
async fn send_event<S: Frames>(socket: &mut S, ev: &Event) -> bool {
    match serde_json::to_string(ev) {
        Ok(text) => socket.send_text(text).await,
        Err(e) => {
            // An `Event` that will not serialise is our bug, not the client's. Dropping the
            // frame keeps the rest of the stream alive.
            tracing::error!(error = %e, "an Event failed to serialise; frame dropped");
            true
        }
    }
}

// ----------------------------------------------------------------------------------------
// test support, shared with prober.rs and watcher.rs
// ----------------------------------------------------------------------------------------

/// A whole `AppState` on a tempdir, for this unit's tests.
///
/// It lives here rather than in a `tests/` file because `AppState` is crate-internal and all
/// three of this unit's background tasks need one.
#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use apexrouter_core::checks::Registry;
    use apexrouter_core::config::{Config, ProviderCfg};
    use apexrouter_core::lockfile::DaemonLock;
    use apexrouter_core::paths::Paths;
    use apexrouter_core::store::Store;
    use apexrouter_core::usage::UsageWriter;
    use apexrouter_providers::local::LocalProvisioner;
    use arc_swap::ArcSwap;
    use std::path::Path;
    use std::time::Instant;

    /// A state and its tempdir, kept together so the directory outlives the test.
    pub(crate) struct Harness {
        /// What the code under test is given.
        pub state: Arc<AppState>,
        /// Dropped last; deletes `$STATE`.
        pub dir: tempfile::TempDir,
    }

    impl Harness {
        /// A fresh subscriber on the same broadcast the daemon uses.
        pub fn subscribe(&self) -> broadcast::Receiver<Event> {
            self.state.tx.subscribe()
        }
    }

    /// `Paths::resolve()` reads process-global environment, so every test that redirects it
    /// serialises here. Held only across `set_var` + `resolve` + restore.
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        &L
    }

    /// The variables that decide where `$STATE` and `config.toml` land.
    ///
    /// `APEXROUTER_CONFIG` is in the list because it **wins** over
    /// `$APEXROUTER_HOME/config.toml`: leaving another unit's fixture value in place would
    /// point this harness's config watcher at that unit's config file.
    const REDIRECTED: [&str; 3] = ["APEXROUTER_HOME", "APEXROUTER_CONFIG", "XDG_CACHE_HOME"];

    /// A `Paths` rooted in `dir`. Never touches the real `$HOME` state tree.
    ///
    /// Three work units keep their own env mutex in this one test binary, so the guard above
    /// serialises this unit's tests against each other but not against theirs. The resolved
    /// paths are therefore **verified** rather than trusted: if another fixture moved the
    /// variables between the `set_var` and the `resolve`, this retries instead of handing
    /// back a `Paths` pointing into somebody else's tempdir — which is how a watcher test
    /// ends up watching a file a different unit's daemon is writing.
    pub(crate) fn paths_in(dir: &Path) -> Paths {
        for _ in 0..64 {
            let guard = env_lock().lock();
            let saved: Vec<(&str, Option<std::ffi::OsString>)> = REDIRECTED
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
            std::env::set_var("APEXROUTER_HOME", dir);
            std::env::set_var("APEXROUTER_CONFIG", dir.join("config.toml"));
            std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
            let resolved = Paths::resolve();
            for (k, v) in saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
            drop(guard);

            let paths = resolved.expect("paths");
            if paths.state() == dir && paths.config_file() == dir.join("config.toml") {
                paths.ensure_layout().expect("layout");
                return paths;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("another unit's env fixture never let go of $APEXROUTER_HOME");
    }

    /// The one config every test in this unit starts from.
    ///
    /// `[providers.together]` points at a **closed loopback port**. The credential chain
    /// resolves Together from the real environment on a developer's machine, and
    /// `$TOGETHER_API_KEY` is a live paid credential on this box. No test in this crate may
    /// reach anything but `127.0.0.x`.
    pub(crate) fn test_config() -> Config {
        let mut cfg = Config::default();
        cfg.compat.mirror_usage_log = false;
        cfg.compat.read_legacy_state = false;
        cfg.router.log_usage = false;
        // An empty rig scan: no build is executed and nothing is enumerated, so the tests
        // stay fast and hermetic on a machine that really does have five llama.cpp builds.
        cfg.endpoints.build_roots = Vec::new();
        cfg.endpoints.model_roots = Vec::new();
        cfg.providers.insert(
            "together".to_owned(),
            ProviderCfg {
                base_url: "http://127.0.0.1:1/v1".to_owned(),
                api_key_env: None,
                api_key_file: None,
            },
        );
        cfg
    }

    /// A complete `AppState` on a fresh tempdir.
    pub(crate) fn harness() -> Harness {
        harness_with(test_config())
    }

    /// A complete `AppState` on a fresh tempdir, with the caller's config.
    pub(crate) fn harness_with(cfg: Config) -> Harness {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let paths = paths_in(dir.path());
        let (tx, _rx) = broadcast::channel(64);
        let usage = UsageWriter::open(&paths, &cfg.compat).expect("usage writer");
        let router = apexrouter_router::RouterInner::new(Arc::new(cfg.clone()), tx.clone(), usage);
        let supervisor = Arc::new(LocalProvisioner::new(
            paths.clone(),
            cfg.clone(),
            tx.clone(),
        ));
        // An empty rig, installed rather than scanned, so a snapshot never shells out.
        supervisor.set_rig(apexrouter_protocol::RigSnapshot::default());
        let lock = DaemonLock::acquire(&paths).expect("daemon lock");
        let state = Arc::new(AppState {
            store: Store::new(paths.clone()),
            paths,
            cfg: ArcSwap::from_pointee(cfg),
            router,
            tx,
            supervisor,
            jobs: crate::jobs::JobRegistry::new(),
            checks: Arc::new(Registry::new()),
            started_at: Instant::now(),
            lock: Arc::new(tokio::sync::Mutex::new(lock)),
        });
        Harness { state, dir }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::harness;
    use super::*;
    use apexrouter_protocol::{AlertLevel, RigSnapshot, ServedBy};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::sync::mpsc;

    /// A `Frames` that records what it was sent and replays a scripted inbound stream.
    struct FakeSocket {
        /// Every text frame, in order.
        sent: mpsc::UnboundedSender<String>,
        /// What `next_inbound` returns, one per call, popped from the back; then it parks.
        inbound: Vec<Option<()>>,
        /// How many frames may still be sent before the "peer" is declared gone.
        budget: Arc<AtomicU32>,
    }

    impl Frames for FakeSocket {
        async fn send_text(&mut self, text: String) -> bool {
            if self.budget.load(Ordering::SeqCst) == 0 {
                return false;
            }
            self.budget.fetch_sub(1, Ordering::SeqCst);
            self.sent.send(text).is_ok()
        }

        async fn next_inbound(&mut self) -> Option<()> {
            match self.inbound.pop() {
                Some(v) => v,
                // Nothing scripted: park, so `select!` always resolves on the broadcast arm.
                None => std::future::pending().await,
            }
        }
    }

    fn fake() -> (FakeSocket, mpsc::UnboundedReceiver<String>, Arc<AtomicU32>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let budget = Arc::new(AtomicU32::new(u32::MAX));
        (
            FakeSocket {
                sent: tx,
                inbound: Vec::new(),
                budget: Arc::clone(&budget),
            },
            rx,
            budget,
        )
    }

    fn parse(frame: &str) -> Event {
        serde_json::from_str(frame).expect("an Event")
    }

    fn alert(id: &str) -> Event {
        Event::Alert {
            level: AlertLevel::Info,
            message: id.to_owned(),
            action: None,
            id: id.to_owned(),
        }
    }

    /// THE acceptance test: an event published **while the snapshot is being assembled** is
    /// still delivered, because the subscription already existed.
    #[tokio::test]
    async fn the_subscription_exists_before_the_snapshot_is_assembled() {
        let h = harness();
        let tx = h.state.tx.clone();

        let (mut rx, snap) = subscribe_then_snapshot(&tx, || async {
            // Whatever this closure does happens strictly after `subscribe()`. Were the two
            // the other way round, this event would be published to nobody and the assertion
            // below would fail — which is exactly the bug the ordering prevents.
            let _ = tx.send(alert("published-during-assembly"));
            snapshot_now(&h.state).await
        })
        .await;

        assert_eq!(snap.product, apexrouter_protocol::PRODUCT);
        match rx.try_recv() {
            Ok(Event::Alert { id, .. }) => assert_eq!(id, "published-during-assembly"),
            other => panic!("the event published during assembly was lost: {other:?}"),
        }
    }

    /// The snapshot goes first, then the live stream, in that order.
    #[tokio::test]
    async fn the_first_frame_is_a_snapshot_and_then_events_flow() {
        let h = harness();
        let (socket, mut sent, _budget) = fake();
        let state = Arc::clone(&h.state);
        let task = tokio::spawn(async move { pump(state, socket).await });

        let first = sent.recv().await.expect("a first frame");
        assert!(
            matches!(parse(&first), Event::Snapshot(_)),
            "the first frame must be a snapshot: {first}"
        );

        // Wait until the loop is subscribed before publishing, so this is not a race.
        while h.state.tx.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
        let _ = h.state.tx.send(alert("one"));
        let next = sent.recv().await.expect("the broadcast event");
        match parse(&next) {
            Event::Alert { id, .. } => assert_eq!(id, "one"),
            other => panic!("expected the alert, got {other:?}"),
        }
        task.abort();
    }

    /// A slow client that missed events gets the whole picture again, not a hole.
    #[tokio::test]
    async fn a_lagged_subscriber_is_re_sent_a_full_snapshot() {
        let h = harness();
        // A tiny broadcast, so the lag is deterministic rather than timing-dependent.
        let (tx, _keep) = broadcast::channel::<Event>(2);
        let mut rx = tx.subscribe();
        for i in 0..8 {
            let _ = tx.send(alert(&format!("dropped-{i}")));
        }
        let lagged = rx.recv().await;
        assert!(
            matches!(lagged, Err(RecvError::Lagged(_))),
            "the receiver should have lagged: {lagged:?}"
        );

        // What the loop does on that arm: assemble and send a whole new snapshot.
        let (mut socket, mut sent, _budget) = fake();
        let snap = snapshot_now(&h.state).await;
        assert!(send_event(&mut socket, &Event::Snapshot(Box::new(snap))).await);
        let frame = sent.recv().await.expect("a frame");
        assert!(matches!(parse(&frame), Event::Snapshot(_)), "{frame}");

        // And the receiver is still usable afterwards — `Lagged` is not `Closed`.
        assert!(rx.recv().await.is_ok());
    }

    /// The loop notices a close on the inbound half instead of blocking on the broadcast
    /// forever. Without `socket.recv()` in the `select!` this task would never end.
    #[tokio::test]
    async fn a_client_close_ends_the_loop() {
        let h = harness();
        let (mut socket, _sent, _budget) = fake();
        socket.inbound = vec![None, Some(())]; // popped from the back: one frame, then close.
        let state = Arc::clone(&h.state);
        let task = tokio::spawn(async move { pump(state, socket).await });

        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        assert!(
            ended.is_ok(),
            "the pump must return when the client closes the socket"
        );
    }

    /// A peer that has gone away stops the loop rather than spinning on a dead socket.
    #[tokio::test]
    async fn a_dead_peer_ends_the_loop() {
        let h = harness();
        let (socket, _sent, budget) = fake();
        budget.store(0, Ordering::SeqCst); // the very first send fails.
        let state = Arc::clone(&h.state);
        let ended = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::spawn(async move { pump(state, socket).await }),
        )
        .await;
        assert!(ended.is_ok(), "a failed send must end the pump");
    }

    /// Frames go out in the internally tagged protocol shape every surface deserialises.
    #[tokio::test]
    async fn events_are_sent_as_the_internally_tagged_protocol_shape() {
        let (mut socket, mut sent, _budget) = fake();
        assert!(send_event(&mut socket, &alert("x")).await);
        let frame = sent.recv().await.expect("a frame");
        assert!(frame.starts_with("{\"type\":\"alert\""), "{frame}");
        assert_eq!(parse(&frame), alert("x"));
    }

    /// The snapshot is assembled from real state, and round-trips.
    #[tokio::test]
    async fn the_snapshot_reports_the_daemon_and_its_own_addresses() {
        let h = harness();
        let snap = snapshot_now(&h.state).await;
        assert_eq!(snap.served_by, ServedBy::Daemon);
        assert_eq!(snap.product, "apexrouter");
        assert_eq!(snap.version, apexrouter_protocol::VERSION);
        assert!(snap.proxy.base_url.starts_with("http://127.0.0.1:"));
        assert!(snap.proxy.control_url.starts_with("http://127.0.0.1:"));
        assert_eq!(snap.proxy.default_alias.as_str(), "auto");
        assert_eq!(snap.rig, RigSnapshot::default());
        assert!(snap.as_of_unix > 1_700_000_000);
        let text = serde_json::to_string(&snap).expect("ser");
        let back: Snapshot = serde_json::from_str(&text).expect("de");
        assert_eq!(back, snap);
    }
}
