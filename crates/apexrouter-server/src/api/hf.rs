//! OWNER: unit S-07 (server/src/api/{vast,hf,providers,checks,compare}.rs). Do not edit outside that unit.
//!
//! The `/v1/hf/*` set: search, files (authoritative sizes) and downloads.
//!
//! This is what closes the discovery→launch dead-end: an HF row becomes a local endpoint
//! without leaving the app. Search here, read the **authoritative** per-file sizes from
//! `paths-info` here, download here, and the result lands under
//! `[hf] download_dir/<repo-basename>/` where S-04's rig scan finds it as a `LocalModel`.
//!
//! # The client is injected, not constructed
//!
//! P-07 owns `HfClient` and publishes it with private state and no constructor, and
//! `AppState` is S-01's file with no HuggingFace slot in it. So the client arrives through
//! [`install_hf_source`] at daemon start and this module holds it behind [`HfSource`] — the
//! same three calls, object-safe, so a test can substitute a fake and no test ever reaches
//! `huggingface.co`. `impl HfSource for HfClient` is the one line that binds the two
//! together and lives here rather than in P-07's file, which this unit does not own.
//!
//! # Downloads are always jobs
//!
//! A 20 GB pull is not an HTTP request. `POST /v1/hf/downloads` therefore always answers
//! `202` with a [`JobRecord`] — `?no_wait` is accepted for symmetry with the rest of the
//! control plane and changes nothing — and the row is flipped to `Failed` on **every** error
//! path including a panic, because S-04's registry guarantees that.

use super::{ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::error::Result as CoreResult;
use apexrouter_protocol::{DownloadProgress, HfFileGroup, HfModel, JobId, JobRecord};
use apexrouter_providers::hf::HfClient;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::Json;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use tokio::sync::mpsc;

/// The job kind every download row carries, so `GET /v1/hf/downloads` is a filter and not a
/// second registry.
const DOWNLOAD_KIND: &str = "hf.download";
/// Default `?limit=` on search.
const DEFAULT_LIMIT: u32 = 20;
/// Hard cap on `?limit=`, because the UI renders every row.
const MAX_LIMIT: u32 = 100;
/// How many progress messages may queue before the downloader waits on the reporter.
const PROGRESS_DEPTH: usize = 64;

/// The `/v1/hf/*` routes.
///
/// `GET /v1/hf/models/{*repo}/files` is registered as `/v1/hf/models/{*rest}` and the
/// trailing `/files` is checked in the handler: a repo id contains a `/`, so the path has to
/// be a wildcard, and axum's matcher only accepts a wildcard as the **final** segment. The
/// wire contract is unchanged — the documented URL is what a client sends.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/hf/search", get(search))
        .route("/v1/hf/models/{*rest}", get(files))
        .route("/v1/hf/downloads", get(list_downloads).post(start_download))
        .route("/v1/hf/downloads/{job}", delete(cancel_download))
}

/// `?q=&limit=`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SearchQuery {
    /// The search string. Empty is allowed: HF returns its most-downloaded GGUF repos.
    #[serde(default)]
    pub q: Option<String>,
    /// Row cap, defaulted and hard-capped.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `POST /v1/hf/downloads`.
///
/// Either name the `files` explicitly or name a `quant` and let the grouped `paths-info`
/// listing pick the shards — the same grouping the UI shows, so what downloads is what was
/// clicked.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DownloadRequest {
    /// `"unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF"`.
    pub repo: String,
    /// Exact repo-relative paths. Wins over `quant` when both are given.
    #[serde(default)]
    pub files: Vec<String>,
    /// A quant label from the grouped listing, e.g. `"UD-Q4_K_XL"`.
    #[serde(default)]
    pub quant: Option<String>,
    /// Where to put it. Defaults to `[hf] download_dir/<repo-basename>/`.
    #[serde(default)]
    pub dest: Option<String>,
    /// Also fetch the vision projector that pairs with the chosen group.
    #[serde(default)]
    pub mmproj: bool,
}

/// `?no_wait=` — accepted for symmetry. A download is always a job.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct DownloadQuery {
    /// Ignored: see the module docs.
    #[serde(default)]
    pub no_wait: Option<bool>,
}

/// `GET /v1/hf/search?q=&limit=` — GGUF repos, newest and most-downloaded first.
pub async fn search(
    State(_s): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Vec<HfModel>> {
    let hf = require_hf()?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let rows = hf
        .search(q.q.as_deref().unwrap_or_default(), limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(rows))
}

/// `GET /v1/hf/models/{repo}/files` — the authoritative per-file sizes, grouped by quant.
///
/// Sizes come from `POST /api/models/{ns}/{repo}/paths-info/{rev}`, never from `siblings`,
/// which often omits them; shards are one group with a summed size, which is the number the
/// fit solver and the disk check need.
pub async fn files(
    State(_s): State<Arc<AppState>>,
    Path(rest): Path<String>,
) -> ApiResult<Vec<HfFileGroup>> {
    let hf = require_hf()?;
    let repo = rest
        .trim_matches('/')
        .strip_suffix("/files")
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no such control route; the files listing is GET /v1/hf/models/{}/files",
                rest.trim_matches('/')
            ))
        })?;
    if repo.is_empty() {
        return Err(ApiError::bad_request("invalid", "a repo id is required").with_param("repo"));
    }
    let groups = hf.files(repo).await.map_err(ApiError::from)?;
    Ok(Json(groups))
}

/// `GET /v1/hf/downloads` — every download job, newest first.
pub async fn list_downloads(State(s): State<Arc<AppState>>) -> ApiResult<Vec<JobRecord>> {
    s.jobs.ensure_wired(&s.tx, &s.paths);
    Ok(Json(
        s.jobs
            .all()
            .into_iter()
            .filter(|j| j.kind == DOWNLOAD_KIND)
            .collect(),
    ))
}

/// `POST /v1/hf/downloads` — resume-capable, progress-streaming, size-verified.
pub async fn start_download(
    State(s): State<Arc<AppState>>,
    Query(_q): Query<DownloadQuery>,
    Json(req): Json<DownloadRequest>,
) -> Result<Response, ApiError> {
    let hf = require_hf()?;
    let repo = req.repo.trim().to_owned();
    if repo.is_empty() {
        return Err(ApiError::bad_request("invalid", "repo is required").with_param("repo"));
    }
    if req.files.is_empty() && req.quant.is_none() {
        return Err(ApiError::bad_request(
            "invalid",
            "name either `files` or a `quant` from the grouped listing",
        )
        .with_param("quant"));
    }

    let dest = destination(&s, &req, &repo);
    s.jobs.ensure_wired(&s.tx, &s.paths);
    let job = s.jobs.spawn_with(DOWNLOAD_KIND, move |h| async move {
        let files = match req.files.is_empty() {
            false => req.files.clone(),
            true => {
                h.progress(Some(2.0), "reading paths-info");
                pick_files(hf.as_ref(), &repo, req.quant.as_deref(), req.mmproj).await?
            }
        };
        if files.is_empty() {
            anyhow::bail!(
                "no file in {repo} matched {}",
                req.quant.as_deref().unwrap_or("the request")
            );
        }

        // Progress is forwarded onto the job row (and therefore onto `/ws`) rather than
        // persisted: a 20 GB pull reports hundreds of times and each write would be an
        // `fsync`.
        let (tx, mut rx) = mpsc::channel::<DownloadProgress>(PROGRESS_DEPTH);
        let reporter = h.clone();
        let pump = tokio::spawn(async move {
            while let Some(p) = rx.recv().await {
                let pct = p
                    .bytes_total
                    .filter(|t| *t > 0)
                    .map(|t| (p.bytes_done as f64 / t as f64 * 100.0) as f32);
                reporter.progress(pct, format!("{} — {:.1} MB/s", p.file, f64::from(p.mbps)));
            }
        });

        let out = hf.download(&repo, &files, &dest, tx).await;
        let _ = pump.await;
        let paths = out?;
        Ok::<_, anyhow::Error>(
            paths
                .into_iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        )
    });
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

/// `DELETE /v1/hf/downloads/{job}` — stop one download.
pub async fn cancel_download(
    State(s): State<Arc<AppState>>,
    Path(job): Path<String>,
) -> ApiResult<JobRecord> {
    s.jobs.ensure_wired(&s.tx, &s.paths);
    let id = job
        .parse::<ulid::Ulid>()
        .map(JobId)
        .map_err(|_| ApiError::bad_request("bad_id", format!("`{job}` is not a job id")))?;
    let existing = s
        .jobs
        .get(id)
        .ok_or_else(|| ApiError::not_found(format!("no download {job}")).with_param("job"))?;
    s.jobs.cancel(id).map(Json).ok_or_else(|| {
        ApiError::conflict(format!(
            "download {job} had already finished as {:?}",
            existing.state
        ))
        .with_param("job")
    })
}

// ----------------------------------------------------------------------------------------
// the injected client
// ----------------------------------------------------------------------------------------

/// The three HuggingFace calls the control plane makes, object-safe.
///
/// Identical to `HfClient`'s inherent methods. It exists so this module can hold a client it
/// cannot construct — P-07 publishes no constructor — and so a test can substitute a fake
/// and stay inside `127.0.0.x`.
pub trait HfSource: Send + Sync {
    /// `GET /api/models?filter=gguf&search=`, following an RFC 5988 `Link: rel=next`.
    fn search<'a>(&'a self, q: &'a str, limit: u32) -> BoxFuture<'a, CoreResult<Vec<HfModel>>>;
    /// Files grouped by quant, with authoritative sizes from `paths-info`.
    fn files<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, CoreResult<Vec<HfFileGroup>>>;
    /// Resumable, progress-streaming download with a size verification on completion.
    fn download<'a>(
        &'a self,
        repo: &'a str,
        files: &'a [String],
        dest: &'a FsPath,
        tx: mpsc::Sender<DownloadProgress>,
    ) -> BoxFuture<'a, CoreResult<Vec<PathBuf>>>;
}

impl HfSource for HfClient {
    fn search<'a>(&'a self, q: &'a str, limit: u32) -> BoxFuture<'a, CoreResult<Vec<HfModel>>> {
        Box::pin(HfClient::search(self, q, limit))
    }

    fn files<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, CoreResult<Vec<HfFileGroup>>> {
        Box::pin(HfClient::files(self, repo))
    }

    fn download<'a>(
        &'a self,
        repo: &'a str,
        files: &'a [String],
        dest: &'a FsPath,
        tx: mpsc::Sender<DownloadProgress>,
    ) -> BoxFuture<'a, CoreResult<Vec<PathBuf>>> {
        Box::pin(HfClient::download(self, repo, files, dest, tx))
    }
}

/// Install the HuggingFace client the control plane should use. Idempotent; the last call
/// wins, and `None` removes it.
///
/// Called once at daemon start. Process-global for the same reason
/// [`super::shutdown_notify`] is: there is one daemon per process, and `AppState` is S-01's
/// file.
pub fn install_hf_source(source: Option<Arc<dyn HfSource>>) {
    if let Ok(mut slot) = hf_slot().write() {
        *slot = source;
    }
}

/// The installed client, if there is one.
pub fn hf_source() -> Option<Arc<dyn HfSource>> {
    hf_slot().read().ok().and_then(|s| s.clone())
}

// ----------------------------------------------------------------------------------------
// internals
// ----------------------------------------------------------------------------------------

/// Where [`hf_source`] reads from.
fn hf_slot() -> &'static RwLock<Option<Arc<dyn HfSource>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<dyn HfSource>>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// The installed client, or a `503` that says what is missing.
fn require_hf() -> Result<Arc<dyn HfSource>, ApiError> {
    hf_source().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "hf_unavailable",
            "this daemon has no HuggingFace client installed",
        )
    })
}

/// Which files a `quant` selects, from the authoritative grouped listing.
async fn pick_files(
    hf: &dyn HfSource,
    repo: &str,
    quant: Option<&str>,
    mmproj: bool,
) -> anyhow::Result<Vec<String>> {
    let groups = hf.files(repo).await?;
    let Some(wanted) = quant else {
        anyhow::bail!("no quant given and no files listed");
    };
    let group = groups
        .iter()
        .find(|g| {
            g.quant
                .as_deref()
                .is_some_and(|q| q.eq_ignore_ascii_case(wanted))
        })
        .or_else(|| {
            groups.iter().find(|g| {
                g.label
                    .to_ascii_lowercase()
                    .contains(&wanted.to_ascii_lowercase())
            })
        });
    let Some(group) = group else {
        let available: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        anyhow::bail!(
            "no quant `{wanted}` in {repo}; available: {}",
            available.join(", ")
        );
    };

    let mut files: Vec<String> = group.files.iter().map(|f| f.rfilename.clone()).collect();
    if mmproj {
        files.extend(group.mmproj.iter().map(|f| f.rfilename.clone()));
    }
    Ok(files)
}

/// `dest`, or `[hf] download_dir/<repo-basename>/`.
fn destination(s: &Arc<AppState>, req: &DownloadRequest, repo: &str) -> PathBuf {
    if let Some(d) = req.dest.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        return expand_tilde(d);
    }
    let root = expand_tilde(&s.cfg().hf.download_dir);
    let basename = repo.rsplit('/').next().unwrap_or(repo);
    root.join(basename)
}

/// `~` and `~/…` against `$HOME`; anything else is returned unchanged.
fn expand_tilde(s: &str) -> PathBuf {
    let s = s.trim();
    if s == "~" {
        if let Some(h) = dirs_home() {
            return h;
        }
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Some(h) = dirs_home() {
            return h.join(rest);
        }
    }
    PathBuf::from(s)
}

/// `$HOME`, without taking a dependency on `dirs` in this crate.
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::checks::serve_s07;
    use crate::api::testkit::{app, test_config};
    use apexrouter_protocol::{HfFile, JobState};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// The client slot is process-global, so the tests that install one serialise here.
    fn slot_lock() -> &'static tokio::sync::Mutex<()> {
        static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        L.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// A HuggingFace that lives entirely in memory. No test in this unit reaches the
    /// network.
    struct FakeHf {
        downloads: Arc<AtomicUsize>,
    }

    impl HfSource for FakeHf {
        fn search<'a>(&'a self, q: &'a str, limit: u32) -> BoxFuture<'a, CoreResult<Vec<HfModel>>> {
            Box::pin(async move {
                Ok((0..limit.min(3))
                    .map(|i| HfModel {
                        id: format!("unsloth/{q}-{i}-GGUF"),
                        author: Some("unsloth".to_owned()),
                        downloads: Some(1000 - u64::from(i)),
                        likes: Some(10),
                        gated: false,
                        last_modified: None,
                        tags: vec!["gguf".to_owned()],
                    })
                    .collect())
            })
        }

        fn files<'a>(&'a self, _repo: &'a str) -> BoxFuture<'a, CoreResult<Vec<HfFileGroup>>> {
            Box::pin(async move {
                Ok(vec![HfFileGroup {
                    label: "UD-Q4_K_XL (2 shards)".to_owned(),
                    quant: Some("UD-Q4_K_XL".to_owned()),
                    total_bytes: 18_000_000_000,
                    files: vec![
                        HfFile {
                            rfilename: "UD-Q4_K_XL/m-00001-of-00002.gguf".to_owned(),
                            size: Some(9_000_000_000),
                            quant: Some("UD-Q4_K_XL".to_owned()),
                            is_mmproj: false,
                            shard_of: Some((1, 2)),
                        },
                        HfFile {
                            rfilename: "UD-Q4_K_XL/m-00002-of-00002.gguf".to_owned(),
                            size: Some(9_000_000_000),
                            quant: Some("UD-Q4_K_XL".to_owned()),
                            is_mmproj: false,
                            shard_of: Some((2, 2)),
                        },
                    ],
                    mmproj: vec![HfFile {
                        rfilename: "mmproj-F16.gguf".to_owned(),
                        size: Some(600_000_000),
                        quant: None,
                        is_mmproj: true,
                        shard_of: None,
                    }],
                }])
            })
        }

        fn download<'a>(
            &'a self,
            repo: &'a str,
            files: &'a [String],
            dest: &'a FsPath,
            tx: mpsc::Sender<DownloadProgress>,
        ) -> BoxFuture<'a, CoreResult<Vec<PathBuf>>> {
            let counter = Arc::clone(&self.downloads);
            Box::pin(async move {
                counter.fetch_add(files.len(), Ordering::SeqCst);
                for f in files {
                    let _ = tx
                        .send(DownloadProgress {
                            job: JobId::new(),
                            repo: repo.to_owned(),
                            file: f.clone(),
                            bytes_done: 512,
                            bytes_total: Some(1024),
                            mbps: 88.5,
                        })
                        .await;
                }
                Ok(files.iter().map(|f| dest.join(f)).collect())
            })
        }
    }

    #[tokio::test]
    async fn search_and_files_go_through_the_installed_client() {
        let _guard = slot_lock().lock().await;
        install_hf_source(Some(Arc::new(FakeHf {
            downloads: Arc::new(AtomicUsize::new(0)),
        })));

        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;
        let http = reqwest::Client::new();

        let rows: Vec<HfModel> = http
            .get(format!("{base}/v1/hf/search?q=qwen&limit=2"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("Vec<HfModel>");
        assert_eq!(rows.len(), 2, "the limit is honoured");

        // The documented URL, wildcard repo id and all.
        let groups: Vec<HfFileGroup> = http
            .get(format!(
                "{base}/v1/hf/models/unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF/files"
            ))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("Vec<HfFileGroup>");
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].total_bytes, 18_000_000_000,
            "shards are summed, and the size is authoritative"
        );

        install_hf_source(None);
    }

    #[tokio::test]
    async fn a_quant_download_becomes_a_job_that_names_its_shards() {
        let _guard = slot_lock().lock().await;
        let downloads = Arc::new(AtomicUsize::new(0));
        install_hf_source(Some(Arc::new(FakeHf {
            downloads: Arc::clone(&downloads),
        })));

        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;
        let http = reqwest::Client::new();

        let res = http
            .post(format!("{base}/v1/hf/downloads"))
            .json(&serde_json::json!({
                "repo": "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF",
                "quant": "ud-q4_k_xl",
                "mmproj": true,
                "dest": state.paths.cache().join("dl").display().to_string(),
            }))
            .send()
            .await
            .expect("post");
        assert_eq!(res.status(), 202);
        let job: JobRecord = res.json().await.expect("JobRecord");
        assert_eq!(job.kind, "hf.download");

        let mut finished = None;
        for _ in 0..100 {
            let now: JobRecord = http
                .get(format!("{base}/v1/jobs/{}", job.id))
                .send()
                .await
                .expect("get")
                .json()
                .await
                .expect("JobRecord");
            if now.state == JobState::Succeeded || now.state == JobState::Failed {
                finished = Some(now);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let done = finished.expect("the job finished");
        assert_eq!(done.state, JobState::Succeeded, "{:?}", done.error);
        assert_eq!(
            downloads.load(Ordering::SeqCst),
            3,
            "two shards plus the projector"
        );

        // and it is listed as a download, not just as a job
        let listed: Vec<JobRecord> = http
            .get(format!("{base}/v1/hf/downloads"))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("Vec<JobRecord>");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, job.id);

        install_hf_source(None);
    }

    #[tokio::test]
    async fn an_unknown_quant_fails_the_job_with_the_available_ones() {
        let _guard = slot_lock().lock().await;
        install_hf_source(Some(Arc::new(FakeHf {
            downloads: Arc::new(AtomicUsize::new(0)),
        })));

        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;
        let http = reqwest::Client::new();

        let job: JobRecord = http
            .post(format!("{base}/v1/hf/downloads"))
            .json(&serde_json::json!({"repo": "a/b", "quant": "Q9_K_NOPE"}))
            .send()
            .await
            .expect("post")
            .json()
            .await
            .expect("JobRecord");

        for _ in 0..100 {
            let now: JobRecord = http
                .get(format!("{base}/v1/jobs/{}", job.id))
                .send()
                .await
                .expect("get")
                .json()
                .await
                .expect("JobRecord");
            if now.state == JobState::Failed {
                let msg = now.error.unwrap_or_default();
                assert!(msg.contains("UD-Q4_K_XL"), "it lists what is there: {msg}");
                install_hf_source(None);
                return;
            }
            assert_ne!(now.state, JobState::Succeeded);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        install_hf_source(None);
        panic!("the job never failed");
    }

    #[tokio::test]
    async fn without_a_client_every_route_is_a_503_that_says_so() {
        let _guard = slot_lock().lock().await;
        install_hf_source(None);

        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;
        let res = reqwest::Client::new()
            .get(format!("{base}/v1/hf/search?q=x"))
            .send()
            .await
            .expect("get");
        assert_eq!(res.status(), 503);
        let body: apexrouter_protocol::ErrorEnvelope = res.json().await.expect("envelope");
        assert_eq!(body.error.kind, "hf_unavailable");
    }

    #[tokio::test]
    async fn a_body_with_neither_files_nor_quant_is_a_400() {
        let _guard = slot_lock().lock().await;
        install_hf_source(Some(Arc::new(FakeHf {
            downloads: Arc::new(AtomicUsize::new(0)),
        })));
        let state = app(test_config());
        let base = serve_s07(Arc::clone(&state)).await;

        let res = reqwest::Client::new()
            .post(format!("{base}/v1/hf/downloads"))
            .json(&serde_json::json!({"repo": "a/b"}))
            .send()
            .await
            .expect("post");
        assert_eq!(res.status(), 400);
        let body: apexrouter_protocol::ErrorEnvelope = res.json().await.expect("envelope");
        assert_eq!(body.error.param.as_deref(), Some("quant"));
        install_hf_source(None);
    }

    #[test]
    fn the_default_destination_is_the_repo_basename_under_the_configured_root() {
        let state = app(test_config());
        let dest = destination(
            &state,
            &DownloadRequest::default(),
            "unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF",
        );
        assert!(
            dest.ends_with("Qwen3-Coder-30B-A3B-Instruct-GGUF"),
            "{}",
            dest.display()
        );
    }
}
