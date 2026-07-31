//! OWNER: unit P-07 (providers/src/hf.rs). Do not edit outside that unit.
//!
//! HuggingFace. Six hand-rolled calls; no `hf-hub` crate, which would drag reqwest 0.13 in.
//!
//! Sizes come from `POST /api/models/{ns}/{repo}/paths-info/{rev}` — the **authoritative**
//! call — not from `siblings`, which often omits them. Gated repos are classified on
//! (status, header-if-present, body) with an **anonymous retry** to distinguish a bad token
//! from genuine gating, and always surface the request-access URL — never "not found".
//!
//! **This closes the discovery→launch dead-end: an HF row can become a local endpoint
//! without leaving the app.**
//!
//! Three things here are deliberate and easy to undo by accident:
//!
//! * **One search path.** [`HfClient::search`] is the only query builder, and the `Link:
//!   rel="next"` cursor is followed only while it stays on the *same origin* as
//!   [`HfClient::base_url`] — so a hostile or mis-parsed header can never walk the client
//!   (or the bearer token) off the configured host. It is also what keeps the test suite on
//!   127.0.0.1.
//! * **`paths-info` is not optional.** `?blobs=true` is asked for, but a `siblings[].size`
//!   is only ever the fallback for a path `paths-info` did not answer for.
//! * **A `.part` file is never renamed before its size is verified**, so an interrupted
//!   transfer cannot be mistaken for a launchable model.

use apexrouter_core::config::Config;
use apexrouter_core::error::{Error, Result};
use apexrouter_core::secret::{resolve_hf, Secret};
use apexrouter_core::Paths;
use apexrouter_protocol::{DownloadProgress, HfFile, HfFileGroup, HfModel, JobId};
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// The public Hub. The only host this client talks to unless a caller overrides it.
pub const HF_BASE_URL: &str = "https://huggingface.co";

/// Sent on every call so Hub-side rate limiting can see who we are.
const UA: &str = concat!("ApexRouter-RS/", env!("CARGO_PKG_VERSION"));

/// Metadata calls are small; a hung Hub must not wedge a UI refresh. Applied **per
/// request**, never on the client: a whole-request timeout on the client would abort a
/// perfectly healthy 18 GiB download 30 seconds in.
const META_TIMEOUT: Duration = Duration::from_secs(30);

/// TCP connect budget for every call.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// A transfer that goes this long without delivering a byte is dead rather than slow. This
/// is the only clock a download runs against.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Rows asked for per search page. The Hub accepts up to 1000; 100 keeps one page cheap and
/// lets the `Link` cursor do the rest.
const PAGE_LIMIT: u32 = 100;

/// Hard stop on cursor following, so a Hub that always emits a `next` link cannot loop.
const MAX_PAGES: usize = 25;

/// `paths` per `paths-info` body. The documented maximum is 2000.
const PATHS_INFO_BATCH: usize = 2_000;

/// How much of an error body is worth carrying into a message.
const ERR_BODY_LIMIT: usize = 512;

/// Minimum wall-clock between two [`DownloadProgress`] events for the same file.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

// ===========================================================================================
// client
// ===========================================================================================

/// The HuggingFace client.
///
/// Cheap to clone — the inner `reqwest::Client` shares one connection pool. Clone it with
/// [`HfClient::for_job`] to stamp a [`JobId`] onto the progress events a download emits.
#[derive(Clone)]
pub struct HfClient {
    http: reqwest::Client,
    /// No trailing slash, ever: everything below concatenates onto it.
    base: String,
    token: Option<Secret<String>>,
    download_root: PathBuf,
    job: Option<JobId>,
}

impl std::fmt::Debug for HfClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HfClient")
            .field("base", &self.base)
            .field("token", &self.token.as_ref().map(|_| "***"))
            .field("download_root", &self.download_root)
            .field("job", &self.job)
            .finish()
    }
}

impl HfClient {
    /// The configured client: token through the documented chain, downloads under
    /// `[hf] download_dir`.
    ///
    /// A missing token is **not** an error — the public Hub answers anonymously, and the
    /// anonymous retry in [`classify_repo_response`] is what tells a bad token apart from a
    /// genuinely gated repo.
    pub fn new(cfg: &Config, paths: &Paths) -> Result<Self> {
        let token = resolve_hf(cfg, paths)?.map(|c| c.secret);
        Self::new_at(HF_BASE_URL, token, expand_tilde(&cfg.hf.download_dir))
    }

    /// The primitive constructor: an explicit base, token and download root.
    ///
    /// Tests use it with a loopback mock server; a self-hosted Hub mirror would too. The
    /// base is stored without its trailing slash.
    pub fn new_at(
        base: impl Into<String>,
        token: Option<Secret<String>>,
        download_root: impl Into<PathBuf>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()?;
        Ok(HfClient {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
            token,
            download_root: download_root.into(),
            job: None,
        })
    }

    /// A copy that stamps `job` onto every [`DownloadProgress`] it emits.
    pub fn for_job(&self, job: JobId) -> Self {
        HfClient {
            job: Some(job),
            ..self.clone()
        }
    }

    /// The host every call goes to.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Where downloads land when the caller has no opinion: the parent of
    /// `<root>/<repo-basename>/`.
    pub fn download_root(&self) -> &Path {
        &self.download_root
    }

    /// Whether a token was resolved. Never exposes it.
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// The human page for a repo — the URL a gated-repo message must carry.
    pub fn repo_url(&self, repo: &str) -> String {
        format!("{}/{}", self.base, repo.trim_matches('/'))
    }

    /// `GET /api/whoami-v2` — the cheapest possible token-validity check.
    ///
    /// Returns the account name. No token is an error here: asking "who am I" without one
    /// is a caller mistake rather than a normal state.
    pub async fn whoami(&self) -> Result<String> {
        if self.token.is_none() {
            return Err(Error::MissingCredential("hf".to_owned()));
        }
        let url = format!("{}/api/whoami-v2", self.base);
        let resp = self
            .call(
                reqwest::Method::GET,
                &url,
                None,
                None,
                Some(META_TIMEOUT),
                "whoami-v2",
            )
            .await?;
        let v: serde_json::Value = resp.json().await?;
        v.get("name")
            .and_then(|n| n.as_str())
            .map(str::to_owned)
            .ok_or_else(|| Error::Invalid {
                what: "huggingface whoami-v2 response".to_owned(),
                why: "no `name` field".to_owned(),
            })
    }

    // -------------------------------------------------------------------------------------
    // search
    // -------------------------------------------------------------------------------------

    /// `GET /api/models?filter=gguf&search=`, following an RFC 5988 `Link: rel=next`.
    ///
    /// GGUF-only by construction: a repo we cannot run is not a search result. Rows are
    /// de-duplicated by id and truncated to `limit`, and the cursor is followed only while
    /// it stays on [`HfClient::base_url`]'s origin.
    pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<HfModel>> {
        let want = limit.clamp(1, 1_000) as usize;
        let page = PAGE_LIMIT.min(want as u32);
        let mut url = format!(
            "{}/api/models?filter=gguf&search={}&sort=downloads&direction=-1&limit={}",
            self.base,
            percent_encode(q),
            page
        );

        let mut out: Vec<HfModel> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for _ in 0..MAX_PAGES {
            let resp = self
                .call(
                    reqwest::Method::GET,
                    &url,
                    None,
                    None,
                    Some(META_TIMEOUT),
                    "api/models",
                )
                .await?;
            let next = next_link(resp.headers().get("link").and_then(|v| v.to_str().ok()));
            let raw: Vec<RawModel> = resp.json().await?;
            let empty = raw.is_empty();

            for r in raw {
                let Some(m) = r.into_model() else { continue };
                if seen.insert(m.id.clone()) {
                    out.push(m);
                }
                if out.len() >= want {
                    return Ok(out);
                }
            }

            match next {
                _ if empty => break,
                // A cursor that leaves our origin is dropped, never followed.
                Some(n) if same_origin(&self.base, &n) => url = n,
                Some(n) => {
                    tracing::debug!(next = %n, base = %self.base, "hf: cross-origin Link ignored");
                    break;
                }
                None => break,
            }
        }
        Ok(out)
    }

    // -------------------------------------------------------------------------------------
    // files
    // -------------------------------------------------------------------------------------

    /// Files grouped by quant, with authoritative sizes from `paths-info` and shards summed.
    ///
    /// `siblings` supplies the *names*; every size is asked for again through `paths-info`,
    /// which is the only call that always answers. A sibling size is used only where
    /// `paths-info` returned nothing for that path.
    ///
    /// `repo` may be `ns/name` or a full `https://huggingface.co/ns/name` URL.
    pub async fn files(&self, repo: &str) -> Result<Vec<HfFileGroup>> {
        let repo = normalise_repo(repo)?;
        let url = format!("{}/api/models/{}?blobs=true", self.base, repo);
        let info: RawRepoInfo = self
            .call(
                reqwest::Method::GET,
                &url,
                None,
                None,
                Some(META_TIMEOUT),
                &repo,
            )
            .await?
            .json()
            .await?;

        let rev = info
            .sha
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("main")
            .to_owned();

        let mut sibling_size: BTreeMap<String, u64> = BTreeMap::new();
        let mut names: Vec<String> = Vec::new();
        for s in &info.siblings {
            if !s.rfilename.to_ascii_lowercase().ends_with(".gguf") {
                continue;
            }
            if let Some(sz) = s.size.or_else(|| s.lfs.as_ref().and_then(|l| l.size)) {
                sibling_size.insert(s.rfilename.clone(), sz);
            }
            names.push(s.rfilename.clone());
        }
        if names.is_empty() {
            return Err(Error::NotFound(format!(
                "no .gguf files in huggingface repo {repo} — it may hold safetensors only"
            )));
        }
        names.sort();
        names.dedup();

        // The authoritative sizing call. Siblings are the fallback, never the source.
        let authoritative = self.paths_info(&repo, &rev, &names).await?;

        let files: Vec<HfFile> = names
            .into_iter()
            .map(|rfilename| {
                let size = authoritative
                    .get(&rfilename)
                    .copied()
                    .or_else(|| sibling_size.get(&rfilename).copied());
                let stem = strip_gguf(&rfilename);
                HfFile {
                    quant: quant_token(stem),
                    is_mmproj: is_projector(file_name(stem)),
                    shard_of: shard_of(file_name(stem)).map(|(_, i, n)| (i, n)),
                    size,
                    rfilename,
                }
            })
            .collect();

        Ok(group(files))
    }

    /// `POST /api/models/{ns}/{repo}/paths-info/{rev}` — the authoritative sizing call.
    ///
    /// Batched at the documented 2000-path maximum. A path the Hub does not answer for is
    /// simply absent from the map rather than defaulted to zero.
    pub async fn paths_info(
        &self,
        repo: &str,
        rev: &str,
        paths: &[String],
    ) -> Result<BTreeMap<String, u64>> {
        let repo = normalise_repo(repo)?;
        let url = format!(
            "{}/api/models/{}/paths-info/{}",
            self.base,
            repo,
            safe_rev(rev)
        );
        let mut out = BTreeMap::new();

        for chunk in paths.chunks(PATHS_INFO_BATCH) {
            let body = serde_json::json!({ "paths": chunk, "expand": false });
            let resp = self
                .call(
                    reqwest::Method::POST,
                    &url,
                    Some(&body),
                    None,
                    Some(META_TIMEOUT),
                    &repo,
                )
                .await?;
            let rows: Vec<RawPathInfo> = resp.json().await?;
            for r in rows {
                if let Some(sz) = r.size.or_else(|| r.lfs.as_ref().and_then(|l| l.size)) {
                    out.insert(r.path, sz);
                }
            }
        }
        Ok(out)
    }

    // -------------------------------------------------------------------------------------
    // download
    // -------------------------------------------------------------------------------------

    /// Resumable, progress-streaming download into `<dest>/<repo-basename>/`, with a size
    /// verification on completion.
    ///
    /// `dest` is the **root** (`~/models` by default — see [`HfClient::download_root`]); the
    /// repo-basename directory below it is created here. Repo-relative sub-directories are
    /// preserved, so `UD-Q4_K_XL/model-00001-of-00002.gguf` keeps its folder and the shards
    /// of one quant stay together.
    ///
    /// Every file lands as `<name>.part` and is renamed only once its size matches what
    /// `paths-info` said. A restart re-requests with `Range:` and appends; a server that
    /// refuses the range restarts that file cleanly rather than corrupting it. A file
    /// already present at the right size is reported and skipped.
    ///
    /// Progress is emitted at most every 250 ms per file, plus once on completion. A dropped
    /// receiver stops the reporting, never the transfer.
    pub async fn download(
        &self,
        repo: &str,
        files: &[String],
        dest: &Path,
        tx: mpsc::Sender<DownloadProgress>,
    ) -> Result<Vec<PathBuf>> {
        let repo = normalise_repo(repo)?;
        if files.is_empty() {
            return Err(Error::Invalid {
                what: format!("download of {repo}"),
                why: "no files selected".to_owned(),
            });
        }
        let dir = dest.join(repo_basename(&repo));
        tokio::fs::create_dir_all(&dir).await.map_err(io_at(&dir))?;

        // Authoritative sizes up front: they are both the progress denominator and the
        // completion check. A repo that will not answer is a hard error here — pulling
        // gigabytes we cannot verify is worse than refusing.
        let expected = self.paths_info(&repo, "main", files).await?;

        let job = self.job.unwrap_or_default();
        let mut written = Vec::with_capacity(files.len());
        let mut reporting = true;

        for f in files {
            let rel = safe_relative(f)?;
            let out = dir.join(&rel);
            if let Some(parent) = out.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(io_at(parent))?;
            }
            let want = expected.get(f).copied();

            // Already here at the right size: nothing to do, but still report it.
            if let (Some(w), Ok(meta)) = (want, tokio::fs::metadata(&out).await) {
                if meta.len() == w {
                    reporting = emit(
                        &tx,
                        reporting,
                        DownloadProgress {
                            job,
                            repo: repo.clone(),
                            file: f.clone(),
                            bytes_done: w,
                            bytes_total: Some(w),
                            mbps: 0.0,
                        },
                    )
                    .await;
                    written.push(out);
                    continue;
                }
            }

            reporting = self
                .fetch_one(&repo, f, &out, want, job, &tx, reporting)
                .await?;
            written.push(out);
        }
        Ok(written)
    }

    /// One file, with resume, streamed progress and the completion size check. Returns
    /// whether progress reporting is still live.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_one(
        &self,
        repo: &str,
        rfilename: &str,
        out: &Path,
        want: Option<u64>,
        job: JobId,
        tx: &mpsc::Sender<DownloadProgress>,
        mut reporting: bool,
    ) -> Result<bool> {
        let part = part_path(out);
        let url = format!(
            "{}/{}/resolve/main/{}",
            self.base,
            repo,
            encode_path(rfilename)
        );

        let mut have = tokio::fs::metadata(&part)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        // A part longer than the file cannot be a prefix of it; start again.
        if want.is_some_and(|w| have > w) {
            let _ = tokio::fs::remove_file(&part).await;
            have = 0;
        }

        let mut resp = self
            .call(
                reqwest::Method::GET,
                &url,
                None,
                (have > 0).then_some(have),
                None,
                repo,
            )
            .await?;

        // The range was refused: the only safe answer is to take the whole file again.
        if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            let _ = tokio::fs::remove_file(&part).await;
            have = 0;
            resp = self
                .call(reqwest::Method::GET, &url, None, None, None, repo)
                .await?;
        }

        let resumed = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        let mut done = if resumed { have } else { 0 };
        let total = want.or_else(|| resp.content_length().map(|c| c + done));

        let mut file = if resumed {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&part)
                .await
                .map_err(io_at(&part))?
        } else {
            tokio::fs::File::create(&part).await.map_err(io_at(&part))?
        };

        let started = Instant::now();
        let mut last_tick = Instant::now();
        let mut since_tick: u64 = 0;
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await.map_err(io_at(&part))?;
            done += chunk.len() as u64;
            since_tick += chunk.len() as u64;

            if last_tick.elapsed() >= PROGRESS_EVERY {
                let mbps = megabits(since_tick, last_tick.elapsed());
                since_tick = 0;
                last_tick = Instant::now();
                reporting = emit(
                    tx,
                    reporting,
                    DownloadProgress {
                        job,
                        repo: repo.to_owned(),
                        file: rfilename.to_owned(),
                        bytes_done: done,
                        bytes_total: total,
                        mbps,
                    },
                )
                .await;
            }
        }
        file.flush().await.map_err(io_at(&part))?;
        file.sync_all().await.map_err(io_at(&part))?;
        drop(file);

        // Verify **before** the rename: a `.part` that never becomes a `.gguf` cannot be
        // launched by mistake.
        let got = tokio::fs::metadata(&part)
            .await
            .map_err(io_at(&part))?
            .len();
        if let Some(w) = want {
            if got != w {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(Error::Invalid {
                    what: format!("download of {rfilename} from {repo}"),
                    why: format!("size mismatch: got {got} bytes, paths-info says {w}"),
                });
            }
        }
        tokio::fs::rename(&part, out).await.map_err(io_at(out))?;

        let fetched = got.saturating_sub(if resumed { have } else { 0 });
        reporting = emit(
            tx,
            reporting,
            DownloadProgress {
                job,
                repo: repo.to_owned(),
                file: rfilename.to_owned(),
                bytes_done: got,
                bytes_total: total.or(Some(got)),
                mbps: megabits(fetched, started.elapsed()),
            },
        )
        .await;
        Ok(reporting)
    }

    // -------------------------------------------------------------------------------------
    // transport
    // -------------------------------------------------------------------------------------

    /// One request, unclassified. `auth` decides whether the token is attached — the
    /// anonymous retry is the whole reason that is a parameter. `timeout` is `None` for a
    /// download, which must be bounded by [`READ_TIMEOUT`] alone.
    async fn send_once(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&serde_json::Value>,
        range: Option<u64>,
        timeout: Option<Duration>,
        auth: bool,
    ) -> Result<reqwest::Response> {
        let mut rb = self.http.request(method, url);
        if let Some(t) = timeout {
            rb = rb.timeout(t);
        }
        if auth {
            if let Some(t) = &self.token {
                rb = rb.header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", t.expose()),
                );
            }
        }
        if let Some(from) = range {
            rb = rb.header(reqwest::header::RANGE, format!("bytes={from}-"));
        }
        if let Some(b) = body {
            rb = rb.json(b);
        }
        Ok(rb.send().await?)
    }

    /// A request whose failures are classified, including the anonymous retry that tells a
    /// bad token apart from a genuinely gated repo.
    async fn call(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&serde_json::Value>,
        range: Option<u64>,
        timeout: Option<Duration>,
        repo: &str,
    ) -> Result<reqwest::Response> {
        let resp = self
            .send_once(method.clone(), url, body, range, timeout, true)
            .await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }

        let code = resp
            .headers()
            .get("x-error-code")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let text = resp.text().await.unwrap_or_default();

        // Only 401/403 *with* a token in hand is ambiguous, and only then do we spend a
        // second request finding out which side is at fault.
        let anonymous_ok = if self.token.is_some()
            && matches!(
                status,
                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
            ) {
            Some(
                self.send_once(method, url, body, range, timeout, false)
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false),
            )
        } else {
            None
        };

        let mut access = classify_repo_response(
            &self.base,
            repo,
            status.as_u16(),
            code.as_deref(),
            &text,
            anonymous_ok,
        );
        if let RepoAccess::RateLimited { retry_after_secs } = &mut access {
            *retry_after_secs = retry_after;
        }
        match access.into_result() {
            Err(e) => Err(e),
            // Unreachable: `classify_repo_response` only returns `Granted` for a 2xx.
            Ok(()) => Err(Error::Other(format!(
                "huggingface returned {status} for {repo}"
            ))),
        }
    }
}

// ===========================================================================================
// gated-repo classification
// ===========================================================================================

/// How the Hub answered a request about one repository.
///
/// The distinction that matters operationally is the middle one: a 401 on a *public* repo
/// means our token is bad, and a 401 on a gated repo means access has not been granted.
/// Only the anonymous retry can tell them apart, and getting it wrong sends the operator
/// either hunting for a typo or rotating a perfectly good token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoAccess {
    /// The request succeeded.
    Granted,
    /// The repo exists but access must be requested. Always carries the URL to request it
    /// at — this is never reported as "not found".
    Gated {
        /// `ns/name`.
        repo: String,
        /// The page where access is requested.
        request_url: String,
        /// What the Hub actually said.
        detail: String,
    },
    /// Our token is the problem: the same request succeeded without it.
    BadToken {
        /// `ns/name`.
        repo: String,
        /// What the Hub actually said.
        detail: String,
    },
    /// The repo genuinely is not there, and nothing suggested gating.
    Missing {
        /// `ns/name`.
        repo: String,
        /// What the Hub actually said, plus where to request access in case it is private.
        detail: String,
    },
    /// The Hub asked us to slow down.
    RateLimited {
        /// `Retry-After`, when it was sent.
        retry_after_secs: Option<u64>,
    },
    /// Anything else.
    Failed {
        /// HTTP status.
        status: u16,
        /// What the Hub actually said.
        detail: String,
    },
}

impl RepoAccess {
    /// Did the call succeed?
    pub fn is_granted(&self) -> bool {
        matches!(self, RepoAccess::Granted)
    }

    /// Turn the verdict into the operator-facing error, or `Ok(())` when it succeeded.
    ///
    /// Every message says what to *do*: request access at a URL, replace a token, or wait.
    pub fn into_result(self) -> Result<()> {
        match self {
            RepoAccess::Granted => Ok(()),
            RepoAccess::Gated {
                repo,
                request_url,
                detail,
            } => Err(Error::Invalid {
                what: format!("huggingface repo {repo}"),
                why: format!(
                    "gated — request access at {request_url} and ensure your token has read \
                     permission ({detail})"
                ),
            }),
            RepoAccess::BadToken { repo, detail } => Err(Error::Invalid {
                what: "huggingface token".to_owned(),
                why: format!(
                    "rejected for {repo}, but the same request succeeded anonymously — the token \
                     is bad or expired; replace it with `apexrouter provider set hf` ({detail})"
                ),
            }),
            RepoAccess::Missing { repo, detail } => Err(Error::NotFound(format!(
                "huggingface repo {repo} ({detail})"
            ))),
            RepoAccess::RateLimited { retry_after_secs } => Err(Error::Other(format!(
                "huggingface rate limit reached{}",
                retry_after_secs
                    .map(|s| format!("; retry after {s}s"))
                    .unwrap_or_default()
            ))),
            RepoAccess::Failed { status, detail } => Err(Error::Other(format!(
                "huggingface returned {status}: {detail}"
            ))),
        }
    }
}

/// Classify a Hub failure on **(status, header-if-present, body)**, in that priority, plus
/// the outcome of the anonymous retry.
///
/// `anonymous_ok` is `Some(true)` when the identical request succeeded without the token,
/// `Some(false)` when it failed too, and `None` when no retry was attempted (there was no
/// token to drop).
///
/// The header is `[?]`: HF is widely reported to send `X-Error-Code: GatedRepo`, and that
/// could not be verified when the port was written. So it *refines* the answer and never
/// gates it — **any** 401/403 that anonymous access also refuses is treated as gating and
/// surfaces the request-access URL, because "not found" is the one answer that wastes the
/// operator's time. Even a genuine `Missing` carries the repo page, so a private repo
/// cannot masquerade as a typo either.
pub fn classify_repo_response(
    base: &str,
    repo: &str,
    status: u16,
    error_code: Option<&str>,
    body: &str,
    anonymous_ok: Option<bool>,
) -> RepoAccess {
    if (200..300).contains(&status) {
        return RepoAccess::Granted;
    }
    let detail = detail_of(status, error_code, body);
    let request_url = format!("{}/{}", base.trim_end_matches('/'), repo.trim_matches('/'));
    let code = error_code.map(str::trim).filter(|c| !c.is_empty());
    let says_gated =
        code.is_some_and(|c| c.eq_ignore_ascii_case("GatedRepo")) || mentions_gating(body);
    let says_missing = code.is_some_and(|c| c.eq_ignore_ascii_case("RepoNotFound"));
    let missing = |detail: String| RepoAccess::Missing {
        repo: repo.to_owned(),
        detail: format!("{detail}; if it is private or gated, request access at {request_url}"),
    };

    match status {
        429 => RepoAccess::RateLimited {
            retry_after_secs: None,
        },
        401 | 403 => {
            if anonymous_ok == Some(true) {
                RepoAccess::BadToken {
                    repo: repo.to_owned(),
                    detail,
                }
            } else if says_gated || !says_missing {
                RepoAccess::Gated {
                    repo: repo.to_owned(),
                    request_url,
                    detail,
                }
            } else {
                missing(detail)
            }
        }
        404 => {
            if says_gated {
                RepoAccess::Gated {
                    repo: repo.to_owned(),
                    request_url,
                    detail,
                }
            } else {
                missing(detail)
            }
        }
        _ => RepoAccess::Failed { status, detail },
    }
}

/// Does the body read like a gating refusal rather than a missing repo?
fn mentions_gating(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    [
        "gated",
        "access to model",
        "awaiting",
        "accept the",
        "authorized",
    ]
    .iter()
    .any(|needle| b.contains(needle))
}

/// A short, bounded quotation of what the Hub said.
fn detail_of(status: u16, error_code: Option<&str>, body: &str) -> String {
    let trimmed: String = body.trim().chars().take(ERR_BODY_LIMIT).collect();
    match (
        error_code.map(str::trim).filter(|c| !c.is_empty()),
        trimmed.is_empty(),
    ) {
        (Some(c), true) => format!("HTTP {status}, X-Error-Code: {c}"),
        (Some(c), false) => format!("HTTP {status}, X-Error-Code: {c}: {trimmed}"),
        (None, true) => format!("HTTP {status}"),
        (None, false) => format!("HTTP {status}: {trimmed}"),
    }
}

// ===========================================================================================
// filename parsing — the same rules `core::discover::models` applies to local files
// ===========================================================================================

/// Split a filename stem into lowercase tokens on `-`, `_`, `.` and space.
///
/// Tokens, never substrings — `my-mmprojector-model` is not a projector.
fn tokens(stem: &str) -> Vec<String> {
    stem.split(['-', '_', '.', ' '])
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Is this file a vision projector rather than a model?
///
/// Matched as a filename **token**, so a directory called `mmproj-experiments/` cannot hide
/// the weights inside it. Pass the file name, not the path.
pub fn is_projector(file_stem: &str) -> bool {
    tokens(file_stem).iter().any(|t| t == "mmproj")
}

/// Split `<base>-00001-of-00003` into `("<base>", 1, 3)`. `None` when it is not a shard.
///
/// The suffix is fixed-width by the llama.cpp convention `-%05d-of-%05d`, which is why
/// `model-1-of-3` is deliberately not a shard.
pub fn shard_of(file_stem: &str) -> Option<(&str, u32, u32)> {
    const TAIL: usize = 15; // "-00001-of-00003"
    if file_stem.len() <= TAIL || !file_stem.is_char_boundary(file_stem.len() - TAIL) {
        return None;
    }
    let (base, tail) = file_stem.split_at(file_stem.len() - TAIL);
    let b = tail.as_bytes();
    if b[0] != b'-' || &b[6..10] != b"-of-" {
        return None;
    }
    if !b[1..6].iter().all(u8::is_ascii_digit) || !b[10..15].iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some((
        base,
        tail.get(1..6)?.parse().ok()?,
        tail.get(10..15)?.parse().ok()?,
    ))
}

/// The quantisation token in a repo-relative path, leftmost match, alternatives in order.
///
/// The published pattern is `(UD-Q\d+[^.\s_-]*|Q\d+_K_[A-Z]+|Q\d+_\d+)`, and this is the
/// same widened form `core::discover::models` uses, so a Hub row and the local file it
/// becomes agree on the quant string:
///
/// * bare `Q\d+_K` — `Carnice-9b-Q6_K.gguf`, the only real model on the dev box, matches
///   none of the three published alternatives;
/// * `IQ\d+_[A-Z0-9]+`, for the `IQ4_XS` family in wide circulation;
/// * the `UD-` class keeps underscores, because `[^.\s_-]*` stops at the first `_` and
///   would yield `UD-Q4` for `UD-Q4_K_XL` — a prefix, and a worse handle for it.
///
/// Run over the **whole** repo-relative path, so the unsloth layout
/// (`UD-Q4_K_XL/model-00001-of-00002.gguf`) is labelled from its directory.
pub fn quant_token(path_stem: &str) -> Option<String> {
    (0..path_stem.len()).find_map(|i| quant_at(path_stem, i))
}

/// Try every alternative at one offset.
fn quant_at(stem: &str, i: usize) -> Option<String> {
    let rest = stem.as_bytes().get(i..)?;

    // UD-Q\d+…  — the Unsloth dynamic quants, e.g. UD-Q4_K_XL.
    if rest.starts_with(b"UD-Q") {
        let mut j = 4;
        while rest.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        if j > 4 {
            while rest
                .get(j)
                .is_some_and(|c| !matches!(c, b'.' | b'-' | b'/' | b' ' | b'\t' | b'\n' | b'\r'))
            {
                j += 1;
            }
            return stem.get(i..i + j).map(str::to_owned);
        }
    }

    // Q\d+_K[_A-Z]*  |  Q\d+_\d+  |  IQ\d+_[A-Z0-9]+
    let digits_at = if rest.starts_with(b"IQ") {
        2usize
    } else if rest.starts_with(b"Q") {
        1usize
    } else {
        return None;
    };
    let mut j = digits_at;
    while rest.get(j).is_some_and(u8::is_ascii_digit) {
        j += 1;
    }
    if j == digits_at || rest.get(j) != Some(&b'_') {
        return None;
    }
    j += 1;
    let start_of_suffix = j;
    while rest
        .get(j)
        .is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        j += 1;
        // `Q4_K_M`, `Q4_K_XL`: one more `_`-separated uppercase run is part of the token.
        if rest.get(j) == Some(&b'_') && rest.get(j + 1).is_some_and(u8::is_ascii_uppercase) {
            j += 1;
        }
    }
    if j == start_of_suffix {
        return None;
    }
    stem.get(i..i + j).map(str::to_owned)
}

/// The path with a trailing `.gguf` removed, case-insensitively.
fn strip_gguf(path: &str) -> &str {
    match path.len().checked_sub(5) {
        Some(cut) if path.is_char_boundary(cut) && path[cut..].eq_ignore_ascii_case(".gguf") => {
            &path[..cut]
        }
        _ => path,
    }
}

/// The last `/`-separated component.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Everything before the last `/`; `""` at the repo root.
fn dir_name(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// What precedes the `mmproj` token in a projector's stem — the prefix a model name must
/// start with for the two to belong together. A root-level `mmproj-F16` has an empty prefix
/// and therefore pairs with everything beside it.
fn projector_prefix(file_stem: &str) -> String {
    let lower = file_stem.to_lowercase();
    match lower.find("mmproj") {
        Some(i) => file_stem[..i].trim_end_matches(['-', '_', '.']).to_owned(),
        None => String::new(),
    }
}

// ===========================================================================================
// grouping
// ===========================================================================================

/// Collapse a flat file list into downloadable units: shards summed, projectors paired,
/// smallest first.
///
/// `total_bytes` is the **weights** total, exactly as `LocalModel::total_bytes` is: a
/// projector is listed separately with its own size, because one group can offer several
/// and only one of them is ever loaded.
fn group(files: Vec<HfFile>) -> Vec<HfFileGroup> {
    let mut projectors_by_dir: BTreeMap<String, Vec<HfFile>> = BTreeMap::new();
    for p in files.iter().filter(|f| f.is_mmproj) {
        projectors_by_dir
            .entry(dir_name(&p.rfilename).to_owned())
            .or_default()
            .push(p.clone());
    }

    let mut buckets: BTreeMap<(String, String), Vec<HfFile>> = BTreeMap::new();
    for f in files.into_iter().filter(|f| !f.is_mmproj) {
        let stem = strip_gguf(&f.rfilename);
        let name = file_name(stem);
        let base = shard_of(name)
            .map(|(b, _, _)| b.to_owned())
            .unwrap_or_else(|| name.to_owned());
        buckets
            .entry((dir_name(stem).to_owned(), base))
            .or_default()
            .push(f);
    }

    let mut out: Vec<HfFileGroup> = buckets
        .into_iter()
        .map(|((dir, base), mut shards)| {
            shards.sort_by(|a, b| {
                a.shard_of
                    .map(|(i, _)| i)
                    .unwrap_or(0)
                    .cmp(&b.shard_of.map(|(i, _)| i).unwrap_or(0))
                    .then_with(|| a.rfilename.cmp(&b.rfilename))
            });
            let quant = shards.iter().find_map(|f| f.quant.clone());
            let total_bytes = shards.iter().filter_map(|f| f.size).sum();
            let head = quant.clone().unwrap_or_else(|| base.clone());
            let label = if shards.len() > 1 {
                format!("{head} ({} shards)", shards.len())
            } else {
                head
            };
            HfFileGroup {
                label,
                quant,
                total_bytes,
                mmproj: projectors_for(&base, &dir, &projectors_by_dir),
                files: shards,
            }
        })
        .collect();

    // Smallest first: on a memory-constrained box the smallest thing that fits is the
    // interesting one. The label breaks ties, so the order is reproducible.
    out.sort_by(|a, b| {
        a.total_bytes
            .cmp(&b.total_bytes)
            .then_with(|| a.label.cmp(&b.label))
    });
    out
}

/// The projectors that belong with one group: same directory first, falling back to the
/// repo root — the unsloth layout puts `mmproj-F16.gguf` beside the quant folders.
///
/// Longest prefix first, so `mmproj.first()` is the one `--mmproj` should be given.
fn projectors_for(base: &str, dir: &str, by_dir: &BTreeMap<String, Vec<HfFile>>) -> Vec<HfFile> {
    let pick = |where_: &str| -> Vec<(usize, HfFile)> {
        by_dir
            .get(where_)
            .map(|ps| {
                ps.iter()
                    .filter_map(|p| {
                        let prefix = projector_prefix(file_name(strip_gguf(&p.rfilename)));
                        base.starts_with(&prefix)
                            .then_some((prefix.len(), p.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut found = pick(dir);
    if found.is_empty() && !dir.is_empty() {
        found = pick("");
    }
    found.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.rfilename.cmp(&b.1.rfilename))
    });
    found.into_iter().map(|(_, p)| p).collect()
}

// ===========================================================================================
// url + path helpers
// ===========================================================================================

/// `ns/name`, from anything an operator might paste: a bare id, a full Hub URL, a trailing
/// slash, an `hf.co/` short form.
///
/// Strict on purpose — the result is concatenated into a URL *and* joined onto a filesystem
/// path, so `..`, backslashes, query strings and spaces are refused rather than escaped.
pub fn normalise_repo(repo: &str) -> Result<String> {
    let bad = |why: &str| Error::Invalid {
        what: format!("huggingface repo id {repo:?}"),
        why: why.to_owned(),
    };
    let s = repo.trim();
    let s = [
        "https://huggingface.co/",
        "http://huggingface.co/",
        "hf.co/",
    ]
    .iter()
    .find_map(|p| s.strip_prefix(p))
    .unwrap_or(s);
    let s = s.trim_matches('/');
    if s.is_empty() {
        return Err(bad("empty"));
    }
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() > 2 {
        return Err(bad("expected `name` or `namespace/name`"));
    }
    if parts.iter().any(|p| {
        p.is_empty()
            || *p == "."
            || *p == ".."
            || !p
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    }) {
        return Err(bad("only [A-Za-z0-9._-] is allowed in a repo id"));
    }
    Ok(s.to_owned())
}

/// A revision safe to concatenate into a URL: a commit sha, a branch or a tag.
///
/// The `sha` we send back comes off the wire, so anything with a `/`, a `?` or a `#` in it
/// falls back to `main` rather than reshaping the request path.
fn safe_rev(rev: &str) -> &str {
    let r = rev.trim();
    let ok = !r.is_empty()
        && r != "."
        && r != ".."
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        r
    } else {
        "main"
    }
}

/// The directory name a repo downloads into: `unsloth/Qwen3-GGUF` → `Qwen3-GGUF`.
pub fn repo_basename(repo: &str) -> &str {
    repo.trim_matches('/').rsplit('/').next().unwrap_or(repo)
}

/// A repo-relative path that provably stays inside the destination directory.
///
/// The file list can come from the Hub or from a caller, so it is never trusted: absolute
/// paths, `..`, `.` and empty components are refused rather than normalised away.
fn safe_relative(rfilename: &str) -> Result<PathBuf> {
    let bad = |why: &str| Error::Invalid {
        what: format!("file path {rfilename:?}"),
        why: why.to_owned(),
    };
    let s = rfilename.trim();
    if s.is_empty() || s.starts_with('/') || s.contains('\\') || s.contains('\0') {
        return Err(bad("must be a relative repo path"));
    }
    let mut out = PathBuf::new();
    for c in s.split('/') {
        if c.is_empty() || c == "." || c == ".." {
            return Err(bad("contains an empty or traversing path component"));
        }
        out.push(c);
    }
    Ok(out)
}

/// `x.gguf` → `x.gguf.part`. Appended, never substituted, so `with_extension` cannot eat
/// the real one.
fn part_path(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Percent-encode a query-string value. Unreserved characters pass through.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-encode a URL path, keeping `/` as the separator.
fn encode_path(s: &str) -> String {
    s.split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/")
}

/// The `rel="next"` target out of an RFC 5988 `Link` header, if there is one.
///
/// Handles quoted and unquoted `rel`, several links, and extra parameters.
fn next_link(header: Option<&str>) -> Option<String> {
    let h = header?;
    for part in h.split(',') {
        let part = part.trim();
        if !part.starts_with('<') {
            continue;
        }
        let Some(close) = part.find('>') else {
            continue;
        };
        let url = &part[1..close];
        let is_next = part[close + 1..].split(';').any(|p| {
            let Some((k, v)) = p.split_once('=') else {
                return false;
            };
            k.trim().eq_ignore_ascii_case("rel") && v.trim().trim_matches('"') == "next"
        });
        if is_next && !url.is_empty() {
            return Some(url.to_owned());
        }
    }
    None
}

/// Do two URLs share a scheme, host and port?
///
/// The cursor is data from the network; this is what stops it redirecting the client — and
/// the bearer token — onto another host.
fn same_origin(a: &str, b: &str) -> bool {
    match (origin(a), origin(b)) {
        (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
        _ => false,
    }
}

/// `scheme://host[:port]` out of an absolute URL.
fn origin(url: &str) -> Option<&str> {
    let i = url.find("://")?;
    let rest = url.get(i + 3..)?;
    let end = rest.find('/').unwrap_or(rest.len());
    url.get(..i + 3 + end)
}

/// `~/models` → `$HOME/models`. `~user` is left alone — it is not ours to guess.
fn expand_tilde(s: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    if s == "~" {
        return home().unwrap_or_else(|| PathBuf::from(s));
    }
    match s.strip_prefix("~/") {
        Some(rest) => match home() {
            Some(h) => h.join(rest),
            None => PathBuf::from(s),
        },
        None => PathBuf::from(s),
    }
}

/// Megabits per second — the same unit as the download-stall thresholds, so a UI can show
/// one number and a check can compare it to `< 50 Mbps`.
fn megabits(bytes: u64, over: Duration) -> f32 {
    let secs = over.as_secs_f32();
    if secs <= 0.0 {
        return 0.0;
    }
    (bytes as f32 * 8.0) / 1_000_000.0 / secs
}

/// Send one progress event. Returns whether the channel is still worth writing to — a
/// dropped receiver stops the reporting, never the transfer.
async fn emit(tx: &mpsc::Sender<DownloadProgress>, live: bool, p: DownloadProgress) -> bool {
    if !live {
        return false;
    }
    tx.send(p).await.is_ok()
}

/// An `Error::Io` that remembers the path it happened to.
fn io_at(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.display().to_string(),
        source,
    }
}

// ===========================================================================================
// wire types
// ===========================================================================================

/// One row of `GET /api/models`. Everything is optional: the list endpoint's shape varies
/// with `expand`/`full`, and a missing field must never lose the row.
#[derive(Debug, Deserialize)]
struct RawModel {
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "modelId")]
    model_id: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    downloads: Option<u64>,
    #[serde(default)]
    likes: Option<u64>,
    /// `false`, `true`, `"auto"` or `"manual"` — HF sends all four.
    #[serde(default)]
    gated: Option<serde_json::Value>,
    #[serde(default, rename = "lastModified")]
    last_modified: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

impl RawModel {
    fn into_model(self) -> Option<HfModel> {
        let id = self
            .id
            .or(self.model_id)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())?;
        let author = self
            .author
            .or_else(|| id.split_once('/').map(|(ns, _)| ns.to_owned()));
        Some(HfModel {
            author,
            downloads: self.downloads,
            likes: self.likes,
            gated: gated_flag(self.gated.as_ref()),
            last_modified: self.last_modified,
            tags: self.tags,
            id,
        })
    }
}

/// `gated` is a bool on some responses and a string (`"auto"`, `"manual"`) on others.
fn gated_flag(v: Option<&serde_json::Value>) -> bool {
    match v {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => !s.is_empty() && !s.eq_ignore_ascii_case("false"),
        _ => false,
    }
}

/// `GET /api/models/{repo}?blobs=true`.
#[derive(Debug, Deserialize)]
struct RawRepoInfo {
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    siblings: Vec<RawSibling>,
}

/// One `siblings[]` entry. `size` is frequently absent, which is the whole reason
/// `paths-info` exists.
#[derive(Debug, Deserialize)]
struct RawSibling {
    rfilename: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    lfs: Option<RawLfs>,
}

/// One `paths-info` row.
#[derive(Debug, Deserialize)]
struct RawPathInfo {
    path: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    lfs: Option<RawLfs>,
}

/// The LFS pointer block; its `size` is the real file size when the outer one is missing.
#[derive(Debug, Deserialize)]
struct RawLfs {
    #[serde(default)]
    size: Option<u64>,
}

// ===========================================================================================
// tests
// ===========================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path as pathm, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn client(server: &MockServer, token: Option<&str>) -> HfClient {
        HfClient::new_at(
            server.uri(),
            token.map(|t| Secret::new(t.to_owned())),
            std::env::temp_dir(),
        )
        .expect("client")
    }

    // -- filename parsing ---------------------------------------------------------------

    #[test]
    fn the_quant_token_is_pulled_out_of_the_path() {
        // The three published alternatives.
        assert_eq!(
            quant_token("Qwen3-30B-A3B-UD-Q4_K_XL").as_deref(),
            Some("UD-Q4_K_XL"),
            "the UD- class keeps its underscores"
        );
        assert_eq!(quant_token("Qwen3.5-9B-Q4_K_M").as_deref(), Some("Q4_K_M"));
        assert_eq!(
            quant_token("Ternary-Bonsai-27B-Q2_0").as_deref(),
            Some("Q2_0")
        );
        // The widenings, identical to core::discover::models.
        assert_eq!(quant_token("Carnice-9b-Q6_K").as_deref(), Some("Q6_K"));
        assert_eq!(quant_token("phi-4-IQ4_XS").as_deref(), Some("IQ4_XS"));
        assert_eq!(quant_token("some-model-f16"), None);
        // The unsloth directory layout is labelled from its folder.
        assert_eq!(
            quant_token("UD-Q4_K_XL/model-00001-of-00002").as_deref(),
            Some("UD-Q4_K_XL")
        );
    }

    #[test]
    fn shards_carry_index_and_total() {
        assert_eq!(
            shard_of("Qwen3-235B-Q4_K_M-00002-of-00005"),
            Some(("Qwen3-235B-Q4_K_M", 2, 5))
        );
        assert_eq!(shard_of("Carnice-9b-Q6_K"), None);
        assert_eq!(shard_of("model-1-of-3"), None, "the width is fixed at five");
    }

    #[test]
    fn mmproj_is_a_filename_token_not_a_path_substring() {
        assert!(is_projector("mmproj-F16"));
        assert!(is_projector("Ternary-Bonsai-27B-mmproj-f16"));
        assert!(!is_projector("my-mmprojector-model-Q4_K_M"));
        // A directory of that name must not hide the weights inside it.
        assert!(!is_projector(file_name("mmproj-experiments/model-Q4_K_M")));
    }

    #[test]
    fn repo_ids_are_normalised_and_traversal_is_refused() {
        assert_eq!(
            normalise_repo(" unsloth/Qwen3-GGUF/ ").expect("ok"),
            "unsloth/Qwen3-GGUF"
        );
        assert_eq!(
            normalise_repo("https://huggingface.co/unsloth/Qwen3-GGUF").expect("ok"),
            "unsloth/Qwen3-GGUF"
        );
        assert_eq!(normalise_repo("gpt2").expect("ok"), "gpt2");
        // A revision off the wire never reshapes the request path.
        assert_eq!(safe_rev("deadbeef"), "deadbeef");
        assert_eq!(safe_rev(" main "), "main");
        assert_eq!(safe_rev("../../admin"), "main");
        assert_eq!(safe_rev(""), "main");
        for bad in ["", "a/b/c", "../etc", "a/../b", "ns/na me", "ns/na\\me"] {
            assert!(normalise_repo(bad).is_err(), "{bad:?} must be refused");
        }
        assert_eq!(repo_basename("unsloth/Qwen3-GGUF"), "Qwen3-GGUF");
    }

    #[test]
    fn download_paths_cannot_escape_the_destination() {
        assert_eq!(
            safe_relative("UD-Q4_K_XL/model-00001-of-00002.gguf").expect("ok"),
            PathBuf::from("UD-Q4_K_XL/model-00001-of-00002.gguf")
        );
        for bad in [
            "/etc/passwd",
            "../../etc/passwd",
            "a/../b",
            "a//b",
            "",
            "a\\b",
        ] {
            assert!(safe_relative(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn part_files_append_rather_than_replace_the_extension() {
        assert_eq!(
            part_path(Path::new("/m/x.gguf")),
            PathBuf::from("/m/x.gguf.part")
        );
    }

    // -- Link header --------------------------------------------------------------------

    #[test]
    fn the_rfc_5988_next_link_is_followed_only_on_our_own_origin() {
        assert_eq!(
            next_link(Some(
                "<https://huggingface.co/api/models?cursor=abc>; rel=\"next\""
            ))
            .as_deref(),
            Some("https://huggingface.co/api/models?cursor=abc")
        );
        assert_eq!(
            next_link(Some(
                "<https://h/a>; rel=\"prev\", <https://h/b>; rel=next; type=x"
            ))
            .as_deref(),
            Some("https://h/b")
        );
        assert_eq!(next_link(Some("<https://h/a>; rel=\"prev\"")), None);
        assert_eq!(next_link(None), None);

        assert!(same_origin(
            "https://huggingface.co",
            "https://huggingface.co/api/models?x=1"
        ));
        assert!(!same_origin(
            "https://huggingface.co",
            "https://evil.example/api"
        ));
        assert!(!same_origin("http://127.0.0.1:9", "http://127.0.0.1:10"));
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(percent_encode("qwen3 30b/a3b"), "qwen3%2030b%2Fa3b");
        assert_eq!(encode_path("UD-Q4_K_XL/a b.gguf"), "UD-Q4_K_XL/a%20b.gguf");
    }

    // -- gated classification -----------------------------------------------------------

    #[test]
    fn a_gated_repo_is_never_reported_as_not_found() {
        // Header present.
        let a = classify_repo_response(
            HF_BASE_URL,
            "meta/Llama",
            403,
            Some("GatedRepo"),
            "",
            Some(false),
        );
        assert!(matches!(a, RepoAccess::Gated { .. }), "{a:?}");

        // Header absent, body silent: a refused 401/403 still means gating, never "missing".
        let b = classify_repo_response(HF_BASE_URL, "meta/Llama", 401, None, "", Some(false));
        assert!(matches!(b, RepoAccess::Gated { .. }), "{b:?}");

        // Body-only signal, on a 404.
        let c = classify_repo_response(
            HF_BASE_URL,
            "meta/Llama",
            404,
            None,
            r#"{"error":"Access to model meta/Llama is restricted, gated"}"#,
            None,
        );
        assert!(matches!(c, RepoAccess::Gated { .. }), "{c:?}");

        // Every gated message carries the request-access URL.
        for access in [a, b, c] {
            let e = access.into_result().expect_err("gated is an error");
            let msg = e.to_string();
            assert!(msg.contains("https://huggingface.co/meta/Llama"), "{msg}");
            assert!(msg.contains("gated"), "{msg}");
            assert!(!msg.contains("not found"), "{msg}");
        }
    }

    #[test]
    fn the_anonymous_retry_distinguishes_a_bad_token() {
        let bad = classify_repo_response(HF_BASE_URL, "ns/pub", 401, None, "", Some(true));
        assert!(matches!(bad, RepoAccess::BadToken { .. }), "{bad:?}");
        let msg = bad.into_result().expect_err("err").to_string();
        assert!(msg.contains("anonymously"), "{msg}");
        assert!(msg.contains("token"), "{msg}");
    }

    #[test]
    fn a_missing_repo_is_missing_but_still_points_at_the_repo_page() {
        let m = classify_repo_response(HF_BASE_URL, "ns/typo", 404, Some("RepoNotFound"), "", None);
        assert!(matches!(m, RepoAccess::Missing { .. }), "{m:?}");
        let msg = m.into_result().expect_err("err").to_string();
        assert!(msg.contains("https://huggingface.co/ns/typo"), "{msg}");

        let ok = classify_repo_response(HF_BASE_URL, "ns/x", 200, None, "", None);
        assert!(ok.is_granted());
        assert!(ok.into_result().is_ok());

        let rl = classify_repo_response(HF_BASE_URL, "ns/x", 429, None, "", None);
        assert!(matches!(rl, RepoAccess::RateLimited { .. }), "{rl:?}");
    }

    // -- search -------------------------------------------------------------------------

    #[tokio::test]
    async fn search_is_gguf_filtered_and_follows_the_next_link() {
        let server = MockServer::start().await;
        let page2 = format!("{}/api/models?cursor=p2", server.uri());

        Mock::given(method("GET"))
            .and(pathm("/api/models"))
            .and(query_param("filter", "gguf"))
            .and(query_param("search", "qwen3"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("link", format!("<{page2}>; rel=\"next\"").as_str())
                    .set_body_json(json!([
                        {"id": "unsloth/Qwen3-GGUF", "downloads": 10, "likes": 2,
                         "gated": false, "lastModified": "2026-06-01T00:00:00.000Z",
                         "tags": ["gguf"]},
                        {"modelId": "bad/dup", "gated": "manual"}
                    ])),
            )
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(pathm("/api/models"))
            .and(query_param("cursor", "p2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "bad/dup"},
                {"id": "other/Model-GGUF", "downloads": 3}
            ])))
            .mount(&server)
            .await;

        let got = client(&server, None)
            .search("qwen3", 50)
            .await
            .expect("search");
        let ids: Vec<&str> = got.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["unsloth/Qwen3-GGUF", "bad/dup", "other/Model-GGUF"]);
        assert_eq!(got[0].author.as_deref(), Some("unsloth"));
        assert!(got[1].gated, "`gated: \"manual\"` is gated");
        assert!(!got[0].gated);
        assert_eq!(
            got[2].author.as_deref(),
            Some("other"),
            "author falls back to the namespace"
        );
    }

    #[tokio::test]
    async fn search_stops_at_the_limit_and_never_leaves_our_origin() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(pathm("/api/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    // A hostile cursor: following it would leave 127.0.0.1.
                    .insert_header("link", "<https://evil.example/api/models>; rel=\"next\"")
                    .set_body_json(json!([{"id": "a/one"}, {"id": "b/two"}, {"id": "c/three"}])),
            )
            .mount(&server)
            .await;

        let c = client(&server, None);
        let got = c.search("x", 2).await.expect("search");
        assert_eq!(got.len(), 2, "limit truncates");

        let all = c.search("x", 50).await.expect("search");
        assert_eq!(
            all.len(),
            3,
            "the cross-origin cursor was dropped, not followed"
        );
    }

    // -- files --------------------------------------------------------------------------

    fn repo_info_mock(files: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"sha": "deadbeef", "siblings": files}))
    }

    #[tokio::test]
    async fn sizes_come_from_paths_info_and_shards_are_summed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(pathm("/api/models/unsloth/Qwen3-GGUF"))
            .respond_with(repo_info_mock(json!([
                {"rfilename": "README.md"},
                {"rfilename": "UD-Q4_K_XL/model-00001-of-00002.gguf"},
                {"rfilename": "UD-Q4_K_XL/model-00002-of-00002.gguf"},
                // siblings claims a size, and it is wrong: paths-info wins.
                {"rfilename": "Qwen3-Q6_K.gguf", "size": 1},
                {"rfilename": "mmproj-F16.gguf"}
            ])))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(pathm("/api/models/unsloth/Qwen3-GGUF/paths-info/deadbeef"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"path": "UD-Q4_K_XL/model-00001-of-00002.gguf", "size": 9_000},
                {"path": "UD-Q4_K_XL/model-00002-of-00002.gguf", "lfs": {"size": 9_000}},
                {"path": "Qwen3-Q6_K.gguf", "size": 7_000},
                {"path": "mmproj-F16.gguf", "size": 600}
            ])))
            .mount(&server)
            .await;

        let groups = client(&server, None)
            .files("unsloth/Qwen3-GGUF")
            .await
            .expect("files");

        assert_eq!(
            groups.len(),
            2,
            "README is not a group, mmproj is not a group: {groups:#?}"
        );
        // Smallest first.
        assert_eq!(groups[0].quant.as_deref(), Some("Q6_K"));
        assert_eq!(
            groups[0].total_bytes, 7_000,
            "paths-info beats the siblings size of 1"
        );
        assert_eq!(groups[0].label, "Q6_K");

        let ud = &groups[1];
        assert_eq!(
            ud.quant.as_deref(),
            Some("UD-Q4_K_XL"),
            "quant read off the directory"
        );
        assert_eq!(
            ud.total_bytes, 18_000,
            "two shards summed, projector excluded"
        );
        assert_eq!(ud.label, "UD-Q4_K_XL (2 shards)");
        assert_eq!(ud.files.len(), 2);
        assert_eq!(ud.files[0].shard_of, Some((1, 2)));
        assert_eq!(
            ud.mmproj.len(),
            1,
            "the root projector pairs with a quant folder"
        );
        assert_eq!(ud.mmproj[0].size, Some(600));
        assert!(ud.mmproj[0].is_mmproj);
    }

    #[tokio::test]
    async fn a_repo_with_no_gguf_says_so() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(pathm("/api/models/ns/safetensors-only"))
            .respond_with(repo_info_mock(json!([{"rfilename": "model.safetensors"}])))
            .mount(&server)
            .await;

        let e = client(&server, None)
            .files("ns/safetensors-only")
            .await
            .expect_err("no gguf");
        assert!(e.to_string().contains("no .gguf"), "{e}");
    }

    #[tokio::test]
    async fn a_gated_repo_triggers_the_anonymous_retry_and_names_the_access_url() {
        let server = MockServer::start().await;
        // Authenticated: refused. Anonymous: also refused. => genuinely gated.
        Mock::given(method("GET"))
            .and(pathm("/api/models/meta/Llama"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-error-code", "GatedRepo")
                    .set_body_string("gated"),
            )
            .mount(&server)
            .await;

        let e = client(&server, Some("hf_bad"))
            .files("meta/Llama")
            .await
            .expect_err("gated");
        let msg = e.to_string();
        assert!(
            msg.contains(&format!("{}/meta/Llama", server.uri())),
            "{msg}"
        );
        assert!(msg.contains("request access"), "{msg}");
        assert_eq!(
            server.received_requests().await.expect("recorded").len(),
            2,
            "the anonymous retry is what proves the token is not the problem"
        );
    }

    #[tokio::test]
    async fn a_bad_token_on_a_public_repo_blames_the_token() {
        let server = MockServer::start().await;
        // With the header: 401. Without it: 200. => the token is the problem.
        Mock::given(method("GET"))
            .and(pathm("/api/models/ns/public"))
            .and(|r: &Request| r.headers.get("authorization").is_some())
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid credentials"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(pathm("/api/models/ns/public"))
            .and(|r: &Request| r.headers.get("authorization").is_none())
            .respond_with(repo_info_mock(json!([{"rfilename": "a-Q4_K_M.gguf"}])))
            .mount(&server)
            .await;

        let e = client(&server, Some("hf_expired"))
            .files("ns/public")
            .await
            .expect_err("bad token");
        let msg = e.to_string();
        assert!(msg.contains("anonymously"), "{msg}");
        assert!(
            !msg.contains("hf_expired"),
            "the token is never echoed: {msg}"
        );
    }

    // -- download -----------------------------------------------------------------------

    async fn drain(mut rx: mpsc::Receiver<DownloadProgress>) -> Vec<DownloadProgress> {
        let mut v = Vec::new();
        while let Some(p) = rx.recv().await {
            v.push(p);
        }
        v
    }

    #[tokio::test]
    async fn download_writes_under_the_repo_basename_and_verifies_the_size() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let body = vec![7u8; 4_096];

        Mock::given(method("POST"))
            .and(pathm("/api/models/unsloth/Qwen3-GGUF/paths-info/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"path": "UD-Q4_K_XL/model.gguf", "size": 4_096}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(pathm(
                "/unsloth/Qwen3-GGUF/resolve/main/UD-Q4_K_XL/model.gguf",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;

        let (tx, rx) = mpsc::channel(64);
        let job = JobId::new();
        let files = vec!["UD-Q4_K_XL/model.gguf".to_owned()];
        let out = client(&server, None)
            .for_job(job)
            .download("unsloth/Qwen3-GGUF", &files, tmp.path(), tx)
            .await
            .expect("download");

        let want = tmp.path().join("Qwen3-GGUF/UD-Q4_K_XL/model.gguf");
        assert_eq!(out, vec![want.clone()]);
        assert_eq!(std::fs::read(&want).expect("read"), body);
        assert!(!part_path(&want).exists(), "the .part file is gone");

        let events = drain(rx).await;
        let last = events.last().expect("at least the completion event");
        assert_eq!(last.job, job, "for_job stamps the progress events");
        assert_eq!(last.bytes_done, 4_096);
        assert_eq!(last.bytes_total, Some(4_096));
    }

    #[tokio::test]
    async fn an_interrupted_download_resumes_with_a_range_request() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");

        Mock::given(method("POST"))
            .and(pathm("/api/models/ns/repo/paths-info/main"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([{"path": "m.gguf", "size": 10}])),
            )
            .mount(&server)
            .await;
        // Only a ranged request is answered, and only with the tail.
        Mock::given(method("GET"))
            .and(pathm("/ns/repo/resolve/main/m.gguf"))
            .and(|r: &Request| {
                r.headers
                    .get("range")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v == "bytes=4-")
            })
            .respond_with(ResponseTemplate::new(206).set_body_bytes(vec![b'B'; 6]))
            .mount(&server)
            .await;

        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("m.gguf.part"), b"AAAA").expect("partial");

        let (tx, rx) = mpsc::channel(64);
        let files = vec!["m.gguf".to_owned()];
        client(&server, None)
            .download("ns/repo", &files, tmp.path(), tx)
            .await
            .expect("resume");

        assert_eq!(
            std::fs::read(dir.join("m.gguf")).expect("read"),
            b"AAAABBBBBB",
            "the partial prefix was kept and the tail appended"
        );
        assert_eq!(drain(rx).await.last().expect("event").bytes_done, 10);
    }

    #[tokio::test]
    async fn a_short_file_is_refused_rather_than_renamed() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");

        Mock::given(method("POST"))
            .and(pathm("/api/models/ns/repo/paths-info/main"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!([{"path": "m.gguf", "size": 4_096}])),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(pathm("/ns/repo/resolve/main/m.gguf"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 10]))
            .mount(&server)
            .await;

        let (tx, _rx) = mpsc::channel(64);
        let files = vec!["m.gguf".to_owned()];
        let e = client(&server, None)
            .download("ns/repo", &files, tmp.path(), tx)
            .await
            .expect_err("size mismatch");
        assert!(e.to_string().contains("size mismatch"), "{e}");
        assert!(
            !tmp.path().join("repo/m.gguf").exists(),
            "nothing launchable was left behind"
        );
        assert!(
            !tmp.path().join("repo/m.gguf.part").exists(),
            "the part file is cleaned up"
        );
    }

    #[tokio::test]
    async fn an_already_complete_file_is_not_fetched_again() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");

        Mock::given(method("POST"))
            .and(pathm("/api/models/ns/repo/paths-info/main"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([{"path": "m.gguf", "size": 5}])),
            )
            .mount(&server)
            .await;

        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("m.gguf"), b"hello").expect("write");

        let (tx, rx) = mpsc::channel(64);
        let files = vec!["m.gguf".to_owned()];
        let out = client(&server, None)
            .download("ns/repo", &files, tmp.path(), tx)
            .await
            .expect("skip");
        assert_eq!(out, vec![dir.join("m.gguf")]);
        // Only the paths-info call happened; no /resolve/ mock was even registered.
        assert_eq!(server.received_requests().await.expect("recorded").len(), 1);
        assert_eq!(drain(rx).await.len(), 1);
    }

    #[tokio::test]
    async fn a_traversing_file_name_never_reaches_the_network() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");
        Mock::given(method("POST"))
            .and(pathm("/api/models/ns/repo/paths-info/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let (tx, _rx) = mpsc::channel(4);
        let files = vec!["../../etc/passwd".to_owned()];
        let e = client(&server, None)
            .download("ns/repo", &files, tmp.path(), tx)
            .await
            .expect_err("traversal");
        assert!(e.to_string().contains("traversing"), "{e}");
    }

    #[tokio::test]
    async fn download_refuses_an_empty_selection() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let (tx, _rx) = mpsc::channel(4);
        let e = client(&server, None)
            .download("ns/repo", &[], tmp.path(), tx)
            .await
            .expect_err("empty");
        assert!(e.to_string().contains("no files selected"), "{e}");
    }

    // -- misc ---------------------------------------------------------------------------

    #[tokio::test]
    async fn whoami_without_a_token_is_a_missing_credential_not_a_request() {
        let server = MockServer::start().await;
        let e = client(&server, None).whoami().await.expect_err("no token");
        assert!(
            matches!(e, Error::MissingCredential(ref p) if p == "hf"),
            "{e}"
        );
        assert!(server
            .received_requests()
            .await
            .expect("recorded")
            .is_empty());
    }

    #[test]
    fn the_client_never_debug_prints_its_token() {
        let c = HfClient::new_at(
            "http://127.0.0.1:1",
            Some(Secret::new("hf_secret".into())),
            "/m",
        )
        .expect("client");
        let s = format!("{c:?}");
        assert!(!s.contains("hf_secret"), "{s}");
        assert!(c.has_token());
        assert_eq!(c.base_url(), "http://127.0.0.1:1");
        assert_eq!(c.download_root(), Path::new("/m"));
        assert_eq!(c.repo_url("ns/x"), "http://127.0.0.1:1/ns/x");
    }

    #[test]
    fn megabits_is_megabits() {
        assert!((megabits(1_000_000, Duration::from_secs(1)) - 8.0).abs() < 0.001);
        assert_eq!(megabits(10, Duration::ZERO), 0.0);
    }
}
