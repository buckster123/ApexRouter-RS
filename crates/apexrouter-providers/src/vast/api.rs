//! OWNER: unit P-02 (providers/src/vast/{api,query,mod}.rs). Do not edit outside that unit.
//!
//! The REST client, behind a trait so the money path can be tested for free.
//!
//! Verified facts this must honour (`docs/port/00c`):
//!
//! * offer search is `PUT /api/v0/search/asks/` with a `{"q": …}` body;
//! * the create response's instance id is **`new_contract`**, not `id`;
//! * logs are a **two-phase `result_url`** poll with **no Bearer on the result fetch**;
//!   a first-fetch 403/404 is normal and the backoff goes to ~30 s;
//! * vast publishes no rate-limit headers, so 429 handling is exponential backoff with
//!   jitter, capped at 30 s.
//!
//! Two rules hold everywhere below. The api key travels **only** in an `Authorization`
//! header built per request — the `reqwest::Client` carries no default headers, which is what
//! makes "no Bearer on the result fetch" true by construction rather than by memory. And no
//! response body is ever interpolated into an error message wholesale: `GET /users/current/`
//! echoes the key, so only named fields and status codes are ever formatted.

use super::query::build_query;
use apexrouter_core::config::Config;
use apexrouter_core::error::{Error, Result};
use apexrouter_core::paths::Paths;
use apexrouter_core::secret::{resolve_vast, Secret};
use apexrouter_protocol::{
    ContainerLaunch, InstanceId, Offer, OfferQuery, OfferSearchResult, VastAccount, VastInstance,
};
use async_trait::async_trait;
use reqwest::Method;
use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Retry base for a 429. Vast publishes **no** rate-limit headers, so there is nothing to
/// read and the only honest policy is exponential backoff with jitter.
const RETRY_BASE_MS: u64 = 400;
/// Nothing ever sleeps longer than this between attempts.
const RETRY_CAP_MS: u64 = 30_000;
/// Attempts after the first, for a retriable status.
const RETRY_MAX_ATTEMPTS: u32 = 4;
/// Statuses worth a second try. A 4xx that is not 429 is our bug, and retrying it only
/// wastes the operator's time.
const RETRIABLE: [u16; 4] = [429, 502, 503, 504];

/// First delay in the two-phase `result_url` poll — the object is usually live within a
/// second.
const RESULT_FIRST_DELAY_MS: u64 = 200;
/// Total budget for the `result_url` poll before it is called a timeout.
const RESULT_DEADLINE_MS: u64 = 60_000;

/// `runtype` is a space-separated **token set**, not an enum, and the spelling production
/// actually receives is `ssh_direc` — without the `t`.
const RUNTYPE: &str = "ssh_direc ssh_proxy";

/// What we tell the operator when a create returns without an id. A created instance bills
/// from the moment it exists, so this message has to be actionable, not merely accurate.
const NO_CONTRACT: &str = "vast create returned no `new_contract`: an instance may be RUNNING \
     AND BILLING with no local record — check `apexrouter vast ls` now, and destroy anything \
     you do not recognise";

/// The live client.
pub struct VastApiHttp {
    /* P-02 */
    http: reqwest::Client,
    base: String,
    cred: Secret<String>,
}

impl VastApiHttp {
    /// A client for `base_url` (`https://console.vast.ai/api/v0`) holding `cred`.
    ///
    /// The `reqwest::Client` is built **without default headers**, so the unauthenticated
    /// `result_url` fetch cannot accidentally inherit the api key.
    pub fn new(base_url: &str, cred: Secret<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("apexrouter/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self::with_client(http, base_url, cred))
    }

    /// Same, reusing a caller-owned connection pool.
    pub fn with_client(http: reqwest::Client, base_url: &str, cred: Secret<String>) -> Self {
        VastApiHttp {
            http,
            base: base_url.trim_end_matches('/').to_owned(),
            cred,
        }
    }

    /// Build one from config, resolving the credential through the **one** chain in
    /// `core::secret`. A missing key is [`Error::MissingCredential`], which already tells the
    /// operator how to supply one.
    pub fn from_config(cfg: &Config, paths: &Paths) -> Result<Self> {
        let found =
            resolve_vast(cfg, paths)?.ok_or_else(|| Error::MissingCredential("vast".to_owned()))?;
        Self::new(&cfg.vast.base_url, found.secret)
    }

    /// The API root this client talks to. Never contains the key.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// One authenticated call, retrying only what is worth retrying.
    ///
    /// Returns the status alongside the parsed body so a caller can tell "gone" (404) from
    /// "broken" without parsing an error string.
    async fn send_raw(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<RawResponse> {
        let url = format!("{}{}", self.base, path);
        let mut attempt = 0u32;
        loop {
            let mut req = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(self.cred.expose());
            if let Some(b) = &body {
                req = req.json(b);
            }
            match req.send().await {
                Ok(res) => {
                    let status = res.status().as_u16();
                    if RETRIABLE.contains(&status) && attempt < RETRY_MAX_ATTEMPTS {
                        let delay = backoff_delay(attempt);
                        tracing::warn!(
                            status,
                            attempt,
                            delay_ms = delay.as_millis() as u64,
                            path,
                            "vast rate-limited or unavailable; backing off"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    let text = res.text().await.map_err(Error::Reqwest)?;
                    let body = if text.trim().is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_str(&text).unwrap_or(Value::Null)
                    };
                    return Ok(RawResponse { status, body });
                }
                Err(e) if e.is_timeout() && attempt < RETRY_MAX_ATTEMPTS => {
                    let delay = backoff_delay(attempt);
                    tracing::warn!(
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        path,
                        "vast request timed out; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(e) => return Err(Error::Reqwest(e)),
            }
        }
    }

    /// [`Self::send_raw`] plus the status/`success` check every caller but `instance` and
    /// `destroy` wants.
    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let res = self.send_raw(method, path, body).await?;
        check_status(path, &res)?;
        Ok(res.body)
    }

    /// Phase two of the logs/exec pattern: fetch `result_url` with a **plain** client.
    ///
    /// The URL is not live when it is handed to us — a 403 or 404 on the first fetch is
    /// normal, not an error — so this backs off from 200 ms towards the 30 s ceiling until
    /// the deadline. Nothing here attaches the api key: the URL is a pre-signed object-store
    /// link on somebody else's host.
    async fn fetch_result_url(&self, url: &str, what: &str) -> Result<String> {
        let started = SystemTime::now();
        let mut attempt = 0u32;
        let mut last = String::new();
        loop {
            match self.http.get(url).send().await {
                Ok(res) => {
                    let status = res.status().as_u16();
                    if res.status().is_success() {
                        return res.text().await.map_err(Error::Reqwest);
                    }
                    // 403/404 is "the object is not there yet" — the normal first answer.
                    last = format!("{what} result_url -> {status}");
                }
                Err(e) => last = format!("{what} result_url transport error: {e}"),
            }
            let waited = started
                .elapsed()
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default();
            let delay = result_delay(attempt);
            if waited + delay.as_millis() as u64 > RESULT_DEADLINE_MS {
                tracing::warn!(detail = %last, "vast result_url never became readable");
                return Err(Error::Timeout {
                    ms: RESULT_DEADLINE_MS,
                });
            }
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    /// The shared half of `logs` and `exec`: ask, then either follow `result_url` or accept
    /// the body itself as the answer.
    async fn two_phase(&self, path: &str, body: Value, what: &str) -> Result<String> {
        let res = self.send(Method::PUT, path, Some(body)).await?;
        match res.get("result_url").and_then(Value::as_str) {
            Some(url) if !url.trim().is_empty() => {
                let url = url.to_owned();
                self.fetch_result_url(&url, what).await
            }
            // "If the response has no `result_url`, the JSON body itself is the answer."
            _ => Ok(inline_result(&res)),
        }
    }
}

/// A status and a parsed body, before any judgement is passed on them.
struct RawResponse {
    /// HTTP status.
    status: u16,
    /// Parsed body, or `Null` when there was not one.
    body: Value,
}

/// Turn a status (and vast's in-band `success: false`) into an [`Error`].
///
/// Only `msg` is ever read out of the body: the account object echoes the api key, and a
/// blanket "here is the whole body" error message would put it in a log file.
fn check_status(path: &str, res: &RawResponse) -> Result<()> {
    let msg = res
        .body
        .get("msg")
        .and_then(Value::as_str)
        .map(|m| m.chars().take(300).collect::<String>());
    match res.status {
        200..=299 => {
            if res.body.get("success").and_then(Value::as_bool) == Some(false) {
                return Err(Error::Other(format!(
                    "vast {path} refused: {}",
                    msg.unwrap_or_else(|| "no message".to_owned())
                )));
            }
            Ok(())
        }
        401 | 403 => Err(Error::Invalid {
            what: "vast credential".to_owned(),
            why: format!(
                "{path} -> {} ({})",
                res.status,
                msg.unwrap_or_else(|| "check the key with `apexrouter doctor`".to_owned())
            ),
        }),
        404 => Err(Error::NotFound(format!("vast {path}"))),
        429 => Err(Error::Other(format!(
            "vast {path} -> 429: rate limited, and the backoff budget is spent"
        ))),
        s => Err(Error::Other(format!(
            "vast {path} -> {s}{}",
            msg.map(|m| format!(": {m}")).unwrap_or_default()
        ))),
    }
}

/// Exponential backoff with jitter, capped at 30 s.
///
/// The cap is what matters: vast publishes no `Retry-After`, so an uncapped doubling would
/// leave a boot watchdog asleep for minutes while a rented box bills.
fn backoff_delay(attempt: u32) -> Duration {
    let base = RETRY_BASE_MS
        .saturating_mul(1u64 << attempt.min(20))
        .min(RETRY_CAP_MS);
    Duration::from_millis(jitter(base).min(RETRY_CAP_MS))
}

/// The `result_url` poll schedule: quick at first, then the same 30 s ceiling.
fn result_delay(attempt: u32) -> Duration {
    let base = RESULT_FIRST_DELAY_MS
        .saturating_mul(1u64 << attempt.min(20))
        .min(RETRY_CAP_MS);
    Duration::from_millis(base)
}

/// ±20 % of `base`, derived from the clock. No `rand` dependency: the only property that
/// matters is that two clients retrying the same 429 do not do it in lockstep.
fn jitter(base: u64) -> u64 {
    let span = base / 5;
    if span == 0 {
        return base;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or_default();
    base - span + (nanos % (2 * span + 1))
}

/// Unix seconds, now.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// The answer when there is no `result_url` — prefer a plain text field, fall back to the
/// document. This body is command output, not the account object, so rendering it is safe.
fn inline_result(v: &Value) -> String {
    for key in ["logs", "result", "output", "msg"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            return s.to_owned();
        }
    }
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Split a log blob into lines, keeping at most the last `tail` of them.
fn tail_lines(text: &str, tail: u32) -> Vec<String> {
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let tail = tail as usize;
    if tail > 0 && lines.len() > tail {
        lines[lines.len() - tail..].to_vec()
    } else {
        lines
    }
}

/// Read `{"instances": …}`. The envelope key is the same for the list and the detail call,
/// but the detail call puts an **object** there, not an array.
fn instances_from(body: &Value) -> Vec<VastInstance> {
    let node = body.get("instances").unwrap_or(body);
    let rows: Vec<&Value> = match node {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![node],
        _ => Vec::new(),
    };
    rows.into_iter()
        .filter_map(
            |row| match serde_json::from_value::<VastInstance>(row.clone()) {
                Ok(i) => Some(i),
                Err(e) => {
                    tracing::warn!(error = %e, "skipping an unparseable vast instance row");
                    None
                }
            },
        )
        .collect()
}

/// Parse the `{"offers": […]}` envelope one row at a time.
///
/// A row that will not parse is dropped with a warning rather than failing the whole search:
/// losing one exotic listing is survivable, losing the market is not. Unknown fields are kept
/// by [`Offer`]'s `flatten`, so this only ever fires on a row missing a *required* field.
fn offers_from(body: &Value) -> Vec<Offer> {
    let node = body.get("offers").unwrap_or(body);
    let Some(rows) = node.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| match serde_json::from_value::<Offer>(row.clone()) {
            Ok(o) => Some(o),
            Err(e) => {
                let id = row.get("id").and_then(Value::as_u64);
                tracing::warn!(offer_id = ?id, error = %e, "skipping an unparseable vast offer");
                None
            }
        })
        .collect()
}

/// The `PUT /asks/{offer_id}/` body.
///
/// Faithful to what production actually receives (`docs/port/09` §1.5), with three decisions
/// worth naming:
///
/// * `args` overrides the image `ENTRYPOINT`. The published images already run
///   `/app/launch.sh` from their entrypoint *and* we run it from `onstart`, which would put
///   two `llama-server`s on port 8000; `sleep infinity` leaves exactly one.
/// * `cancel_unavail` is **true**: if the offer went away between search and create, fail
///   rather than accept a stopped instance that quietly bills disk.
/// * a `-p` mapping is added only for `expose_public`. The default posture is tunnel-only,
///   and the vast env map encodes docker flags as *keys* — `{"-p 8000:8000": "1"}`.
fn create_body(launch: &ContainerLaunch, label: &str) -> Value {
    let mut env = Map::new();
    for (k, v) in &launch.env {
        env.insert(k.clone(), Value::String(v.clone()));
    }
    if launch.expose_public {
        env.insert(
            format!("-p {}:{}", launch.port, launch.port),
            Value::String("1".to_owned()),
        );
    }

    let mut body = Map::new();
    body.insert("client_id".to_owned(), Value::String("me".to_owned()));
    body.insert("image".to_owned(), Value::String(launch.image.clone()));
    body.insert("disk".to_owned(), Value::from(launch.disk_gb));
    body.insert("env".to_owned(), Value::Object(env));
    body.insert("onstart".to_owned(), Value::String(launch.onstart.clone()));
    body.insert("runtype".to_owned(), Value::String(RUNTYPE.to_owned()));
    body.insert(
        "args".to_owned(),
        Value::Array(vec![
            Value::String("sleep".to_owned()),
            Value::String("infinity".to_owned()),
        ]),
    );
    body.insert("label".to_owned(), Value::String(label.to_owned()));
    body.insert(
        "target_state".to_owned(),
        Value::String("running".to_owned()),
    );
    body.insert("price".to_owned(), Value::Null);
    body.insert("cancel_unavail".to_owned(), Value::Bool(true));
    body.insert("image_login".to_owned(), Value::Null);
    body.insert("use_jupyter_lab".to_owned(), Value::Bool(false));
    body.insert("python_utf8".to_owned(), Value::Bool(false));
    body.insert("lang_utf8".to_owned(), Value::Bool(false));
    Value::Object(body)
}

/// Everything we ask vast.ai to do.
#[async_trait]
pub trait VastApi: Send + Sync {
    /// `GET /users/current/`. Free, and the one live call in the test suite.
    async fn account(&self) -> Result<VastAccount>;
    /// `PUT /api/v0/search/asks/` with a `{"q": …}` body.
    async fn search(&self, q: &OfferQuery) -> Result<OfferSearchResult>;
    /// `PUT /api/v0/asks/{offer_id}/`. **Reads the id from `new_contract`.**
    async fn create(
        &self,
        offer_id: u64,
        launch: &ContainerLaunch,
        label: &str,
    ) -> Result<InstanceId>;
    /// The whole fleet.
    async fn instances(&self) -> Result<Vec<VastInstance>>;
    /// One instance, or `None` when it is gone.
    async fn instance(&self, id: InstanceId) -> Result<Option<VastInstance>>;
    /// `PUT /instances/{id}/` with `{"state": "stopped" | "running"}` — park and wake.
    ///
    /// Stopping releases the GPUs and keeps billing the disk; starting resumes the hourly
    /// bill, **which is why the callers gate it behind a `SpendApproval`**. The caller
    /// verifies the state change happened; vast accepting the PUT is not the same thing.
    async fn set_target_state(&self, id: InstanceId, running: bool) -> Result<()>;
    /// `DELETE`. The caller **verifies before forgetting**.
    async fn destroy(&self, id: InstanceId) -> Result<()>;
    /// `PUT /api/v0/instances/request_logs/{id}/`, then the two-phase `result_url` poll.
    async fn logs(&self, id: InstanceId, tail: u32) -> Result<Vec<String>>;
    /// Run something on the box through vast's exec endpoint.
    async fn exec(&self, id: InstanceId, cmd: &str) -> Result<String>;
}

#[async_trait]
impl VastApi for VastApiHttp {
    async fn account(&self) -> Result<VastAccount> {
        let body = self.send(Method::GET, "/users/current/", None).await?;
        // `VastAccount` has no `api_key` field, so the echoed key is dropped right here and
        // can never reach a snapshot, a log line or a `--json` envelope.
        serde_json::from_value(body).map_err(Error::Json)
    }

    async fn search(&self, q: &OfferQuery) -> Result<OfferSearchResult> {
        let body = self
            .send(Method::PUT, "/search/asks/", Some(build_query(q)))
            .await?;
        Ok(OfferSearchResult {
            offers: offers_from(&body),
            // P-03 owns relaxation and the vocabulary; the transport reports neither.
            relaxations: Vec::new(),
            queried_at_unix: now_unix(),
            gpu_name_vocabulary: Vec::new(),
        })
    }

    async fn create(
        &self,
        offer_id: u64,
        launch: &ContainerLaunch,
        label: &str,
    ) -> Result<InstanceId> {
        let path = format!("/asks/{offer_id}/");
        let body = self
            .send(Method::PUT, &path, Some(create_body(launch, label)))
            .await?;
        // THE id is `new_contract`. `id` on this response is the ask, and using it would
        // leave us polling a contract that does not exist while a real one bills.
        let id = body
            .get("new_contract")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Other(NO_CONTRACT.to_owned()))?;
        Ok(InstanceId(id))
    }

    async fn instances(&self) -> Result<Vec<VastInstance>> {
        let body = self.send(Method::GET, "/instances/?owner=me", None).await?;
        Ok(instances_from(&body))
    }

    async fn instance(&self, id: InstanceId) -> Result<Option<VastInstance>> {
        let path = format!("/instances/{id}/?owner=me");
        let res = self.send_raw(Method::GET, &path, None).await?;
        if res.status == 404 {
            return Ok(None);
        }
        check_status(&path, &res)?;
        let rows = instances_from(&res.body);
        Ok(rows
            .iter()
            .find(|i| i.id == id)
            .cloned()
            .or_else(|| rows.into_iter().next()))
    }

    async fn set_target_state(&self, id: InstanceId, running: bool) -> Result<()> {
        let path = format!("/instances/{id}/");
        let mut body = Map::new();
        body.insert(
            "state".to_owned(),
            Value::String(if running { "running" } else { "stopped" }.to_owned()),
        );
        self.send(Method::PUT, &path, Some(Value::Object(body)))
            .await
            .map(|_| ())
    }

    async fn destroy(&self, id: InstanceId) -> Result<()> {
        let path = format!("/instances/{id}/");
        let res = self
            .send_raw(Method::DELETE, &path, Some(Value::Object(Map::new())))
            .await?;
        // Already gone is the outcome we wanted.
        if res.status == 404 {
            return Ok(());
        }
        check_status(&path, &res)
    }

    async fn logs(&self, id: InstanceId, tail: u32) -> Result<Vec<String>> {
        let mut body = Map::new();
        body.insert("tail".to_owned(), Value::String(tail.to_string()));
        let text = self
            .two_phase(
                &format!("/instances/request_logs/{id}/"),
                Value::Object(body),
                "logs",
            )
            .await?;
        Ok(tail_lines(&text, tail))
    }

    async fn exec(&self, id: InstanceId, cmd: &str) -> Result<String> {
        let mut body = Map::new();
        body.insert("command".to_owned(), Value::String(cmd.to_owned()));
        self.two_phase(
            &format!("/instances/command/{id}/"),
            Value::Object(body),
            "command",
        )
        .await
    }
}

// ---------------------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------------------

/// One call a [`FixtureVast`] saw, so a test can assert *what happened* and not only what
/// came back — "the reservation row exists before `create` was called" is an ordering claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixtureCall {
    /// `account()`.
    Account,
    /// `search()`.
    Search,
    /// `create()`.
    Create {
        /// The offer that would have been rented.
        offer_id: u64,
        /// The label that would have been set.
        label: String,
    },
    /// `instances()`.
    Instances,
    /// `instance()`.
    Instance(InstanceId),
    /// `set_target_state()` — `true` is a wake, `false` a park.
    SetTargetState(InstanceId, bool),
    /// `destroy()`.
    Destroy(InstanceId),
    /// `logs()`.
    Logs(InstanceId),
    /// `exec()`.
    Exec(InstanceId),
}

/// A create a [`FixtureVast`] recorded instead of performing.
#[derive(Clone, Debug, PartialEq)]
pub struct FixtureCreate {
    /// The offer id.
    pub offer_id: u64,
    /// The label.
    pub label: String,
    /// The container contract, so a test can assert `HOST=127.0.0.1` survived.
    pub launch: ContainerLaunch,
    /// The id handed back.
    pub instance_id: InstanceId,
}

/// What a fixture create does instead of succeeding.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum CreateBehaviour {
    /// Return the next instance id.
    #[default]
    Succeed,
    /// Panic, so P-04 can prove a `Reserved` row survives an aborted create.
    Panic,
    /// Fail with a message.
    Fail(String),
}

/// The mutable half of the fixture.
#[derive(Default)]
struct FixtureState {
    /// What `account()` returns.
    account: VastAccount,
    /// The market.
    offers: Vec<Offer>,
    /// The fleet.
    instances: Vec<VastInstance>,
    /// What `logs()` returns.
    logs: Vec<String>,
    /// What `exec()` returns.
    exec_output: String,
    /// The id the next create hands back.
    next_instance_id: u64,
    /// What create does.
    create: CreateBehaviour,
    /// Statuses `instance()` walks through.
    status_script: VecDeque<String>,
    /// Everything that was called.
    calls: Vec<FixtureCall>,
    /// The last query `search()` was given.
    last_query: Option<OfferQuery>,
    /// Every recorded create.
    created: Vec<FixtureCreate>,
}

/// Replays recorded JSON — **this is what lets the money path be tested for free.** No test
/// ever creates an instance.
///
/// It is a *fake*, not a stub: `search` applies the filters the market applies (so a test
/// that widens a query really does see more rows), `create` mints an instance and adds it to
/// the fleet, `destroy` removes it, and every call is recorded in [`FixtureVast::calls`].
pub struct FixtureVast {
    /* P-02 */
    state: Mutex<FixtureState>,
}

/// Offers recorded from the live market on 2026-07-30 (`docs/port/00c` §"Live market
/// snapshot"), including fields we do not model, so the `flatten` path is exercised.
const RECORDED_OFFERS: &str = r#"{"offers": [
  {"id": 43731729, "ask_contract_id": 43731729, "machine_id": 142595, "gpu_name": "RTX 3090",
   "num_gpus": 2, "gpu_ram": 24576, "gpu_total_ram": 49152, "dph_total": 0.305, "dph_base": 0.301,
   "storage_cost": 0.12, "inet_down_cost": 0.02, "inet_up_cost": 0.02, "cpu_ram": 85864,
   "cpu_cores_effective": 12.0, "disk_space": 383.0, "cuda_max_good": 13.2,
   "driver_version": "595.84", "geolocation": "Czechia, CZ", "inet_down": 561.8, "inet_up": 551.0,
   "reliability2": 0.9897, "direct_port_count": 199, "static_ip": true, "rented": false,
   "rentable": true, "dlperf": 68.4, "dlperf_per_dphtotal": 224.3, "duration": 864000.0,
   "verification": "verified", "gpu_arch": "nvidia"},
  {"id": 43731730, "machine_id": 142596, "gpu_name": "H100 SXM", "num_gpus": 2, "gpu_ram": 81559,
   "gpu_total_ram": 163118, "dph_total": 3.344, "dph_base": 3.30, "cpu_ram": 258048,
   "cpu_cores_effective": 32.0, "disk_space": 1024.0, "cuda_max_good": 13.1,
   "geolocation": "Montana, US", "inet_down": 3200.0, "inet_up": 2900.0, "reliability2": 0.9971,
   "direct_port_count": 256, "static_ip": true, "rented": false, "rentable": true,
   "dlperf": 320.1, "dlperf_per_dphtotal": 95.7, "verification": "verified"},
  {"id": 43731731, "machine_id": 142597, "gpu_name": "RTX 4090", "num_gpus": 4, "gpu_ram": 24564,
   "gpu_total_ram": 98256, "dph_total": 1.42, "cpu_ram": 128000, "disk_space": 512.0,
   "cuda_max_good": 12.8, "geolocation": "Sweden, SE", "inet_down": 940.0, "inet_up": 910.0,
   "reliability2": 0.9812, "direct_port_count": 64, "rentable": true, "rented": false,
   "verification": "unverified"}
]}"#;

impl Default for FixtureVast {
    fn default() -> Self {
        Self::new()
    }
}

impl FixtureVast {
    /// An empty market and an empty fleet.
    pub fn new() -> Self {
        FixtureVast {
            state: Mutex::new(FixtureState {
                next_instance_id: 28_675_431,
                ..FixtureState::default()
            }),
        }
    }

    /// The recorded 2026-07-30 market: three offers and the real `$7.73` credit, so a cost
    /// preview in a test is the same arithmetic the operator sees.
    pub fn recorded() -> Self {
        let offers = serde_json::from_str::<Value>(RECORDED_OFFERS)
            .map(|v| offers_from(&v))
            .unwrap_or_default();
        Self::new().with_offers(offers).with_account(VastAccount {
            id: 291_079,
            credit: 7.729,
            balance: Some(0.0),
            can_pay: Some(true),
            has_billing: Some(true),
        })
    }

    /// Replace the account.
    pub fn with_account(self, account: VastAccount) -> Self {
        self.mutate(|s| s.account = account);
        self
    }

    /// Replace the market.
    pub fn with_offers(self, offers: Vec<Offer>) -> Self {
        self.mutate(|s| s.offers = offers);
        self
    }

    /// Replay a recorded `{"offers": […]}` document (or a bare array).
    pub fn with_offers_json(self, json: &str) -> Result<Self> {
        let v: Value = serde_json::from_str(json).map_err(Error::Json)?;
        Ok(self.with_offers(offers_from(&v)))
    }

    /// Replace the fleet.
    pub fn with_instances(self, instances: Vec<VastInstance>) -> Self {
        self.mutate(|s| s.instances = instances);
        self
    }

    /// Replay a recorded `{"instances": …}` document (array, bare array or single object).
    pub fn with_instances_json(self, json: &str) -> Result<Self> {
        let v: Value = serde_json::from_str(json).map_err(Error::Json)?;
        Ok(self.with_instances(instances_from(&v)))
    }

    /// What `logs()` returns.
    pub fn with_logs(self, lines: Vec<String>) -> Self {
        self.mutate(|s| s.logs = lines);
        self
    }

    /// What `exec()` returns.
    pub fn with_exec_output(self, out: impl Into<String>) -> Self {
        let out = out.into();
        self.mutate(|s| s.exec_output = out);
        self
    }

    /// The id the next `create` hands back.
    pub fn with_next_instance_id(self, id: u64) -> Self {
        self.mutate(|s| s.next_instance_id = id);
        self
    }

    /// Statuses `instance()` reports in order, the last one repeating forever. This is how a
    /// boot-watchdog test walks `created → loading → running` (or straight to `exited`)
    /// without a timer.
    pub fn with_status_script<I, S>(self, statuses: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let script: VecDeque<String> = statuses.into_iter().map(Into::into).collect();
        self.mutate(|s| s.status_script = script);
        self
    }

    /// Make `create` panic. P-04's acceptance needs exactly this: a `Reserved` ledger row
    /// must survive a create that never returns.
    pub fn panicking_on_create(self) -> Self {
        self.mutate(|s| s.create = CreateBehaviour::Panic);
        self
    }

    /// Make `create` fail with a message.
    pub fn failing_create(self, why: impl Into<String>) -> Self {
        let why = why.into();
        self.mutate(|s| s.create = CreateBehaviour::Fail(why));
        self
    }

    /// Every call this fixture saw, in order.
    pub fn calls(&self) -> Vec<FixtureCall> {
        self.read(|s| s.calls.clone())
    }

    /// The last query `search` was given — the proof that "auto" and the browser ran the
    /// same one.
    pub fn last_query(&self) -> Option<OfferQuery> {
        self.read(|s| s.last_query.clone())
    }

    /// Every create this fixture recorded instead of performing.
    pub fn created(&self) -> Vec<FixtureCreate> {
        self.read(|s| s.created.clone())
    }

    /// The fleet as it stands.
    pub fn fleet(&self) -> Vec<VastInstance> {
        self.read(|s| s.instances.clone())
    }

    /// Mutate the state. A poisoned lock is recovered rather than propagated: the fixture
    /// panics on purpose in one of its modes, and that must not turn every later assertion
    /// into a second panic.
    fn mutate<F: FnOnce(&mut FixtureState)>(&self, f: F) {
        let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut guard);
    }

    /// Read the state.
    fn read<T, F: FnOnce(&FixtureState) -> T>(&self, f: F) -> T {
        let guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
        f(&guard)
    }
}

/// Does one recorded offer satisfy the query? The same predicate the market applies, so a
/// relaxation in a test really does widen the result set.
fn offer_matches(o: &Offer, q: &OfferQuery) -> bool {
    if !q.gpu_names.is_empty() && !q.gpu_names.iter().any(|n| n == &o.gpu_name) {
        return false;
    }
    if q.num_gpus_min > 0 && o.num_gpus < q.num_gpus_min {
        return false;
    }
    if q.num_gpus_max > 0 && o.num_gpus > q.num_gpus_max {
        return false;
    }
    if o.rentable == Some(false) {
        return false;
    }
    if q.max_dph.is_some_and(|m| o.dph_total > m) {
        return false;
    }
    if q.min_reliability
        .is_some_and(|min| o.reliability2.unwrap_or_default() < min)
    {
        return false;
    }
    if q.min_inet_down
        .is_some_and(|min| o.inet_down.unwrap_or_default() < min)
    {
        return false;
    }
    if q.min_disk_gb
        .is_some_and(|min| o.disk_space.unwrap_or_default() < f64::from(min))
    {
        return false;
    }
    if q.min_cuda
        .is_some_and(|min| o.cuda_max_good.unwrap_or_default() < min)
    {
        return false;
    }
    if q.verified == Some(true)
        && o.extra
            .get("verification")
            .and_then(Value::as_str)
            .is_some_and(|v| v != "verified")
    {
        return false;
    }
    true
}

#[async_trait]
impl VastApi for FixtureVast {
    async fn account(&self) -> Result<VastAccount> {
        self.mutate(|s| s.calls.push(FixtureCall::Account));
        Ok(self.read(|s| s.account.clone()))
    }

    async fn search(&self, q: &OfferQuery) -> Result<OfferSearchResult> {
        self.mutate(|s| {
            s.calls.push(FixtureCall::Search);
            s.last_query = Some(q.clone());
        });
        let mut offers: Vec<Offer> = self.read(|s| {
            s.offers
                .iter()
                .filter(|o| offer_matches(o, q))
                .cloned()
                .collect()
        });
        offers.sort_by(|a, b| {
            a.dph_total
                .partial_cmp(&b.dph_total)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if q.limit > 0 {
            offers.truncate(q.limit as usize);
        }
        Ok(OfferSearchResult {
            offers,
            relaxations: Vec::new(),
            queried_at_unix: now_unix(),
            gpu_name_vocabulary: Vec::new(),
        })
    }

    async fn create(
        &self,
        offer_id: u64,
        launch: &ContainerLaunch,
        label: &str,
    ) -> Result<InstanceId> {
        let behaviour = self.read(|s| s.create.clone());
        self.mutate(|s| {
            s.calls.push(FixtureCall::Create {
                offer_id,
                label: label.to_owned(),
            })
        });
        match behaviour {
            CreateBehaviour::Panic => panic!("FixtureVast: create panicked on purpose"),
            CreateBehaviour::Fail(why) => Err(Error::Other(why)),
            CreateBehaviour::Succeed => {
                let id = InstanceId(self.read(|s| s.next_instance_id));
                let offer = self.read(|s| s.offers.iter().find(|o| o.id == offer_id).cloned());
                let mut row = Map::new();
                row.insert("id".to_owned(), Value::from(id.0));
                row.insert(
                    "actual_status".to_owned(),
                    Value::String("created".to_owned()),
                );
                row.insert("label".to_owned(), Value::String(label.to_owned()));
                if let Some(o) = &offer {
                    row.insert("gpu_name".to_owned(), Value::String(o.gpu_name.clone()));
                    row.insert("num_gpus".to_owned(), Value::from(o.num_gpus));
                    if let Some(n) = serde_json::Number::from_f64(o.dph_total) {
                        row.insert("dph_total".to_owned(), Value::Number(n));
                    }
                }
                let instance = serde_json::from_value::<VastInstance>(Value::Object(row))
                    .map_err(Error::Json)?;
                self.mutate(|s| {
                    s.next_instance_id += 1;
                    s.created.push(FixtureCreate {
                        offer_id,
                        label: label.to_owned(),
                        launch: launch.clone(),
                        instance_id: id,
                    });
                    s.instances.push(instance);
                });
                Ok(id)
            }
        }
    }

    async fn instances(&self) -> Result<Vec<VastInstance>> {
        self.mutate(|s| s.calls.push(FixtureCall::Instances));
        Ok(self.read(|s| s.instances.clone()))
    }

    async fn instance(&self, id: InstanceId) -> Result<Option<VastInstance>> {
        self.mutate(|s| {
            s.calls.push(FixtureCall::Instance(id));
            // Advance the scripted status, holding on the last entry forever.
            if !s.status_script.is_empty() {
                let next = if s.status_script.len() == 1 {
                    s.status_script.front().cloned()
                } else {
                    s.status_script.pop_front()
                };
                if let (Some(next), Some(inst)) =
                    (next, s.instances.iter_mut().find(|i| i.id == id))
                {
                    inst.actual_status = Some(next);
                }
            }
        });
        Ok(self.read(|s| s.instances.iter().find(|i| i.id == id).cloned()))
    }

    async fn set_target_state(&self, id: InstanceId, running: bool) -> Result<()> {
        self.mutate(|s| {
            s.calls.push(FixtureCall::SetTargetState(id, running));
            if let Some(inst) = s.instances.iter_mut().find(|i| i.id == id) {
                inst.actual_status = Some(if running { "running" } else { "stopped" }.to_owned());
                inst.intended_status = Some(if running { "running" } else { "stopped" }.to_owned());
            }
        });
        Ok(())
    }

    async fn destroy(&self, id: InstanceId) -> Result<()> {
        self.mutate(|s| {
            s.calls.push(FixtureCall::Destroy(id));
            s.instances.retain(|i| i.id != id);
        });
        Ok(())
    }

    async fn logs(&self, id: InstanceId, tail: u32) -> Result<Vec<String>> {
        self.mutate(|s| s.calls.push(FixtureCall::Logs(id)));
        let lines = self.read(|s| s.logs.clone());
        Ok(tail_lines(&lines.join("\n"), tail))
    }

    async fn exec(&self, id: InstanceId, _cmd: &str) -> Result<String> {
        self.mutate(|s| s.calls.push(FixtureCall::Exec(id)));
        Ok(self.read(|s| s.exec_output.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{ContainerRuntime, GeoFilter, ImageType};
    use serde_json::json;
    use std::collections::BTreeMap;
    use wiremock::matchers::{bearer_token, body_partial_json, method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Every test in this module talks to a `wiremock` server on 127.0.0.1 and nothing else.
    fn client(server: &MockServer) -> VastApiHttp {
        VastApiHttp::new(&server.uri(), Secret::new("test-key-not-a-real-one".into()))
            .expect("client")
    }

    fn query() -> OfferQuery {
        OfferQuery {
            gpu_names: vec!["RTX 3090".into()],
            num_gpus_min: 2,
            num_gpus_max: 2,
            max_dph: None,
            min_reliability: None,
            min_inet_down: None,
            min_disk_gb: None,
            min_cuda: None,
            geo: GeoFilter::Any,
            verified: Some(true),
            limit: 3,
            order: Vec::new(),
            extra: Map::new(),
        }
    }

    fn launch() -> ContainerLaunch {
        let mut env = BTreeMap::new();
        env.insert("HOST".to_owned(), "127.0.0.1".to_owned());
        env.insert("PORT".to_owned(), "8000".to_owned());
        env.insert("HF_TOKEN".to_owned(), "hf_secret".to_owned());
        ContainerLaunch {
            runtime: ContainerRuntime::LlamaCpp,
            image: "ghcr.io/buckster123/vastai-gguf:prebuilt".to_owned(),
            image_type: ImageType::Prebuilt,
            disk_gb: 80,
            env,
            onstart: "bash /app/launch.sh > /var/log/launch.log 2>&1 &".to_owned(),
            host: "127.0.0.1".to_owned(),
            port: 8000,
            expose_public: false,
        }
    }

    // ---- search ----------------------------------------------------------------------

    #[tokio::test]
    async fn search_puts_the_verified_body_and_keeps_unknown_offer_fields() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/search/asks/"))
            .and(body_partial_json(json!({
                "q": {"gpu_name": {"eq": "RTX 3090"}, "num_gpus": {"eq": 2},
                      "rentable": {"eq": true}, "type": "ask", "limit": 3}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "offers": [
                    {"id": 1, "gpu_name": "RTX 3090", "num_gpus": 2, "gpu_ram": 24576,
                     "gpu_total_ram": 49152, "dph_total": 0.305, "geolocation": "Czechia, CZ",
                     "a_field_vast_added_last_tuesday": 7},
                    {"id": 2, "gpu_name": "RTX 3090"}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let res = client(&server).search(&query()).await.expect("search");
        // The second row is missing required fields: dropped, not fatal.
        assert_eq!(res.offers.len(), 1);
        assert_eq!(res.offers[0].geo_code(), Some("CZ"));
        assert!(res.offers[0]
            .extra
            .contains_key("a_field_vast_added_last_tuesday"));
        assert!(res.queried_at_unix > 0);
        // Relaxation and the vocabulary belong to P-03, not to the transport.
        assert!(res.relaxations.is_empty());
        assert!(res.gpu_name_vocabulary.is_empty());
    }

    #[tokio::test]
    async fn every_authenticated_request_carries_the_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/current/"))
            .and(bearer_token("test-key-not-a-real-one"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1, "credit": 1.0})))
            .expect(1)
            .mount(&server)
            .await;
        client(&server).account().await.expect("account");
    }

    // ---- create ----------------------------------------------------------------------

    #[tokio::test]
    async fn create_reads_the_instance_id_from_new_contract() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/asks/43731729/"))
            .respond_with(
                // `id` here is the ask. Reading it instead of `new_contract` is the classic
                // way to poll a contract that does not exist while a real one bills.
                ResponseTemplate::new(200).set_body_json(
                    json!({"success": true, "new_contract": 7835610, "id": 43731729}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let id = client(&server)
            .create(43_731_729, &launch(), "apexrouter-mk1")
            .await
            .expect("create");
        assert_eq!(id, InstanceId(7_835_610));
    }

    #[tokio::test]
    async fn a_create_without_new_contract_names_the_billing_risk() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/asks/\d+/$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
            .mount(&server)
            .await;

        let err = client(&server)
            .create(1, &launch(), "l")
            .await
            .expect_err("must not be silently ok");
        let msg = err.to_string();
        assert!(msg.contains("new_contract"), "{msg}");
        assert!(msg.contains("BILLING"), "{msg}");
    }

    /// The body gets a fixture test because ARCHITECTURE §3.7 says it does: the `args`
    /// override is the only thing between us and two `llama-server`s on port 8000.
    #[test]
    fn the_create_body_is_the_verified_shape() {
        let body = create_body(&launch(), "apexrouter-mk1");
        assert_eq!(body["client_id"], json!("me"));
        assert_eq!(
            body["image"],
            json!("ghcr.io/buckster123/vastai-gguf:prebuilt")
        );
        assert_eq!(body["disk"], json!(80));
        assert_eq!(body["runtype"], json!("ssh_direc ssh_proxy"));
        assert_eq!(body["args"], json!(["sleep", "infinity"]));
        assert_eq!(body["label"], json!("apexrouter-mk1"));
        assert_eq!(body["target_state"], json!("running"));
        assert_eq!(body["cancel_unavail"], json!(true));
        assert_eq!(
            body["onstart"],
            json!("bash /app/launch.sh > /var/log/launch.log 2>&1 &")
        );
        // The token lives in the env MAP, never in the onstart string vast echoes back.
        assert_eq!(body["env"]["HF_TOKEN"], json!("hf_secret"));
        assert_eq!(body["env"]["HOST"], json!("127.0.0.1"));
        assert!(!body["onstart"]
            .as_str()
            .unwrap_or_default()
            .contains("hf_secret"));
        // Tunnel-only posture: no docker port mapping unless asked for.
        assert!(body["env"].get("-p 8000:8000").is_none(), "{body}");

        let mut public = launch();
        public.expose_public = true;
        let body = create_body(&public, "l");
        assert_eq!(body["env"]["-p 8000:8000"], json!("1"));
    }

    // ---- instances -------------------------------------------------------------------

    #[tokio::test]
    async fn the_fleet_is_read_out_of_the_instances_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/instances/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "instances_found": 1,
                "instances": [{"id": 28675431, "actual_status": "running",
                               "brand_new_column": "kept"}]
            })))
            .mount(&server)
            .await;
        let fleet = client(&server).instances().await.expect("instances");
        assert_eq!(fleet.len(), 1);
        assert!(fleet[0].extra.contains_key("brand_new_column"));
    }

    #[tokio::test]
    async fn instance_detail_reads_the_object_under_the_plural_key_and_404_is_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/instances/28675431/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "instances": {"id": 28675431, "actual_status": "running"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/instances/999/"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"msg": "no such"})))
            .mount(&server)
            .await;

        let api = client(&server);
        let one = api
            .instance(InstanceId(28_675_431))
            .await
            .expect("detail")
            .expect("present");
        assert_eq!(one.id, InstanceId(28_675_431));
        assert!(api.instance(InstanceId(999)).await.expect("gone").is_none());
    }

    #[tokio::test]
    async fn destroying_something_already_gone_is_success() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/instances/28675431/"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"msg": "not found"})))
            .mount(&server)
            .await;
        client(&server)
            .destroy(InstanceId(28_675_431))
            .await
            .expect("already gone is the outcome we wanted");
    }

    // ---- logs: the two-phase result_url ----------------------------------------------

    #[tokio::test]
    async fn logs_poll_the_result_url_without_a_bearer_and_tolerate_a_first_403() {
        let server = MockServer::start().await;
        let result_url = format!("{}/s3/logs.txt", server.uri());
        Mock::given(method("PUT"))
            .and(path("/instances/request_logs/28675431/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"success": true, "result_url": result_url})),
            )
            .mount(&server)
            .await;
        // The object is not live yet — the documented normal first answer.
        Mock::given(method("GET"))
            .and(path("/s3/logs.txt"))
            .respond_with(ResponseTemplate::new(403))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/s3/logs.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string("line one\nline two\n"))
            .with_priority(2)
            .mount(&server)
            .await;

        let lines = client(&server)
            .logs(InstanceId(28_675_431), 1000)
            .await
            .expect("logs");
        assert_eq!(lines, vec!["line one".to_owned(), "line two".to_owned()]);

        // The result URL is a pre-signed link on somebody else's host. Our key must not
        // travel to it.
        let seen = server
            .received_requests()
            .await
            .expect("wiremock records requests");
        let fetches: Vec<_> = seen
            .iter()
            .filter(|r| r.url.path() == "/s3/logs.txt")
            .collect();
        assert_eq!(fetches.len(), 2, "first 403 then 200");
        for f in fetches {
            assert!(
                f.headers.get("authorization").is_none(),
                "no Bearer on the result fetch"
            );
        }
    }

    #[tokio::test]
    async fn a_response_without_a_result_url_is_itself_the_answer() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/instances/command/7/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"success": true, "result": "Wed Jul 30 12:00:00 2026"})),
            )
            .mount(&server)
            .await;
        let out = client(&server)
            .exec(InstanceId(7), "date")
            .await
            .expect("exec");
        assert_eq!(out, "Wed Jul 30 12:00:00 2026");
    }

    #[tokio::test]
    async fn logs_keep_only_the_requested_tail() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/instances/request_logs/7/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true, "logs": "a\nb\nc\nd"
            })))
            .mount(&server)
            .await;
        let lines = client(&server).logs(InstanceId(7), 2).await.expect("logs");
        assert_eq!(lines, vec!["c".to_owned(), "d".to_owned()]);
    }

    // ---- errors and backoff ----------------------------------------------------------

    #[tokio::test]
    async fn a_429_is_retried_and_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/current/"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/users/current/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": 291079, "credit": 7.73})),
            )
            .with_priority(2)
            .mount(&server)
            .await;

        let account = client(&server).account().await.expect("retried");
        assert_eq!(account.id, 291_079);
    }

    /// Vast publishes no rate-limit headers, so the only contract is: doubling, jittered,
    /// and never longer than 30 s.
    #[test]
    fn backoff_doubles_with_jitter_and_is_capped_at_thirty_seconds() {
        for attempt in 0..3 {
            let base = RETRY_BASE_MS << attempt;
            let d = backoff_delay(attempt).as_millis() as u64;
            assert!(
                d >= base - base / 5 && d <= base + base / 5,
                "attempt {attempt}: {d} ms is not within +-20% of {base} ms"
            );
        }
        for attempt in 7..40 {
            assert!(
                backoff_delay(attempt) <= Duration::from_millis(RETRY_CAP_MS),
                "attempt {attempt} exceeded the cap"
            );
        }
        // The result_url schedule shares the ceiling and starts far quicker.
        assert_eq!(
            result_delay(0),
            Duration::from_millis(RESULT_FIRST_DELAY_MS)
        );
        assert!(result_delay(30) <= Duration::from_millis(RETRY_CAP_MS));
    }

    #[tokio::test]
    async fn an_in_band_failure_is_an_error_even_with_a_200() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/instances/7/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"success": false, "msg": "instance is not yours"})),
            )
            .mount(&server)
            .await;
        let err = client(&server)
            .destroy(InstanceId(7))
            .await
            .expect_err("success:false is a failure");
        assert!(err.to_string().contains("not yours"), "{err}");
    }

    /// The account object echoes the api key. Neither the struct nor any error built out of
    /// that response may carry it.
    #[tokio::test]
    async fn the_account_never_carries_the_api_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/current/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 291079, "credit": 7.72899, "balance": 0, "can_pay": true,
                "has_billing": true, "api_key": "SECRET-DO-NOT-KEEP", "email": "x@example.invalid"
            })))
            .mount(&server)
            .await;
        let account = client(&server).account().await.expect("account");
        let rendered = format!(
            "{}{account:?}",
            serde_json::to_string(&account).unwrap_or_default()
        );
        assert!(!rendered.contains("SECRET"), "{rendered}");
        assert!(!rendered.contains("api_key"), "{rendered}");
        assert!((account.credit - 7.72899).abs() < 1e-9);
    }

    #[tokio::test]
    async fn an_unauthorised_call_says_so_without_echoing_the_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/current/"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({"msg": "bad key"})))
            .mount(&server)
            .await;
        let err = client(&server).account().await.expect_err("401");
        let msg = err.to_string();
        assert!(msg.contains("vast credential"), "{msg}");
        assert!(!msg.contains("test-key-not-a-real-one"), "{msg}");
    }

    // ---- the fixture -----------------------------------------------------------------

    #[tokio::test]
    async fn the_fixture_replays_the_recorded_market_and_filters_like_the_real_one() {
        let api = FixtureVast::recorded();
        assert_eq!(api.account().await.expect("account").id, 291_079);

        let broad = OfferQuery {
            gpu_names: Vec::new(),
            num_gpus_min: 0,
            num_gpus_max: 0,
            verified: None,
            limit: 0,
            ..query()
        };
        let all = api.search(&broad).await.expect("search");
        assert_eq!(all.offers.len(), 3);
        // Cheapest first, like the real order clause.
        assert!(all.offers[0].dph_total <= all.offers[1].dph_total);

        let narrow = api.search(&query()).await.expect("search");
        assert_eq!(narrow.offers.len(), 1);
        assert_eq!(narrow.offers[0].gpu_name, "RTX 3090");

        // A tighter bound really does shrink the set — which is what makes a relaxation
        // test meaningful.
        let mut strict = query();
        strict.min_reliability = Some(0.999);
        assert!(api.search(&strict).await.expect("search").offers.is_empty());

        assert_eq!(
            api.last_query().map(|q| q.min_reliability),
            Some(Some(0.999))
        );
        assert!(api.calls().contains(&FixtureCall::Account));
    }

    #[tokio::test]
    async fn the_fixture_mints_destroys_and_scripts_an_instance() {
        let api = FixtureVast::recorded().with_next_instance_id(7_835_610);
        let id = api
            .create(43_731_729, &launch(), "apexrouter-mk1")
            .await
            .expect("create");
        assert_eq!(id, InstanceId(7_835_610));

        let created = api.created();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].offer_id, 43_731_729);
        assert_eq!(created[0].launch.host, "127.0.0.1");

        let api = api.with_status_script(["loading", "running"]);
        let mut phases = Vec::new();
        for _ in 0..3 {
            phases.push(
                api.instance(id)
                    .await
                    .expect("detail")
                    .and_then(|i| i.actual_status),
            );
        }
        assert_eq!(
            phases,
            vec![
                Some("loading".to_owned()),
                Some("running".to_owned()),
                Some("running".to_owned())
            ]
        );

        api.destroy(id).await.expect("destroy");
        assert!(api.instance(id).await.expect("gone").is_none());
        assert!(api.fleet().is_empty());
    }

    /// P-04 needs exactly this: a create that never returns, so the `Reserved` ledger row
    /// written *before* the call is the thing that saves the operator.
    #[tokio::test]
    async fn the_fixture_can_panic_inside_create() {
        let api = FixtureVast::new().panicking_on_create();
        let launch = launch();
        let call = std::panic::AssertUnwindSafe(api.create(1, &launch, "l"));
        let out = futures_util::FutureExt::catch_unwind(call).await;
        assert!(out.is_err(), "create must have panicked");
        // The call was recorded before the panic, and the fixture is still usable.
        assert!(matches!(
            api.calls().first(),
            Some(FixtureCall::Create { offer_id: 1, .. })
        ));
        assert!(api.created().is_empty(), "nothing was actually rented");
    }

    #[tokio::test]
    async fn a_failing_fixture_create_is_an_error_not_a_panic() {
        let api = FixtureVast::new().failing_create("offer went away");
        let err = api.create(1, &launch(), "l").await.expect_err("must fail");
        assert!(err.to_string().contains("offer went away"), "{err}");
    }

    #[tokio::test]
    async fn the_fixture_replays_recorded_documents() {
        let api = FixtureVast::new()
            .with_instances_json(r#"{"instances": [{"id": 7, "actual_status": "running"}]}"#)
            .expect("instances json")
            .with_logs(vec!["one".into(), "two".into()])
            .with_exec_output("ok");
        assert_eq!(api.fleet().len(), 1);
        assert_eq!(
            api.logs(InstanceId(7), 1).await.expect("logs"),
            vec!["two".to_owned()]
        );
        assert_eq!(api.exec(InstanceId(7), "true").await.expect("exec"), "ok");
        assert!(api.calls().contains(&FixtureCall::Exec(InstanceId(7))));
    }

    // ---- the live call ---------------------------------------------------------------

    /// `GET /users/current/` is free and read-only, so it is the one call worth checking
    /// against the real key. It is **`#[ignore]`d *and* env-gated**, so the suite stays
    /// hermetic: `cargo test` — and even `cargo test -- --include-ignored` — never leaves
    /// 127.0.0.0/8 unless a human sets `APEXROUTER_LIVE_VAST=1`.
    ///
    /// Verified by hand on 2026-07-30: `200`, `credit` 7.729, and the parsed struct has no
    /// field carrying the api key the response echoes.
    #[tokio::test]
    #[ignore = "live: opt in with APEXROUTER_LIVE_VAST=1"]
    async fn live_users_current_parses_against_the_real_key() {
        if std::env::var("APEXROUTER_LIVE_VAST").unwrap_or_default() != "1" {
            return;
        }
        let cfg = Config::default();
        let paths = Paths::resolve().expect("paths");
        let api = VastApiHttp::from_config(&cfg, &paths).expect("a vast key must be resolvable");
        let account = api.account().await.expect("GET /users/current/");
        assert!(account.id > 0);
        assert!(account.credit.is_finite());
        let json = serde_json::to_string(&account).expect("ser");
        assert!(!json.contains("api_key"), "{json}");
    }
}
