//! OWNER: unit C-04 (core/store.rs). Do not edit outside that unit.
//!
//! Atomic state writes and the offline read/write lock.
//!
//! Every write goes through one helper: mode `0600` at `OpenOptions` time, a temp file in
//! the **same directory**, `fsync(file)` → `rename` → `fsync(dir)`. A reader during a
//! rename never sees a partial file.
//!
//! Two locks live in `$STATE` and they are not interchangeable
//! (ARCHITECTURE §1.2, §5.3):
//!
//! * `apexrouterd.lock` — **daemon only**. Held `LOCK_EX` for the daemon's lifetime; its
//!   presence *is* the answer to "is there an owner?". Nothing in this module touches it.
//! * `state.lock` — what this module uses. Offline *readers* take `LOCK_SH`, offline
//!   *writers* take `LOCK_EX`, so a concurrent `apexrouter status --watch` can never make
//!   `apexrouter serve` believe a daemon is already running.

use crate::error::{Error, Result};
use crate::paths::Paths;
use apexrouter_protocol::{
    Alias, Backend, BackendId, EndpointRecord, FavoriteHost, RouteFile, ServiceRecord,
    StudioRecord, TunnelStatus,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Mode for every state file we write: owner read/write, nothing else.
const FILE_MODE: u32 = 0o600;
/// Mode for every state directory we create.
const DIR_MODE: u32 = 0o700;
/// The `schema_version` a freshly minted `routes.json` carries.
const ROUTES_SCHEMA_VERSION: u32 = 1;
/// The alias a zero-config install falls back to (`[router] default_alias`).
const DEFAULT_ALIAS: &str = "auto";

/// Distinguishes temp files written by concurrent writers inside one process.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Wrap an `io::Error` with the path it happened to, because "No such file or directory"
/// on its own has never helped anybody.
fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Removes a half-written temp file unless it was renamed into place.
struct TmpFile {
    path: PathBuf,
    armed: bool,
}

impl TmpFile {
    fn new(path: PathBuf) -> TmpFile {
        TmpFile { path, armed: true }
    }

    /// The rename succeeded — the temp name no longer exists, so do not unlink it.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TmpFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// An `flock`ed `state.lock`, released when the `File` is dropped — including on unwind.
struct StateLock {
    _file: File,
}

impl StateLock {
    fn acquire(path: &Path, exclusive: bool) -> Result<StateLock> {
        ensure_dir(path.parent())?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(FILE_MODE)
            .open(path)
            .map_err(|e| io_err(path, e))?;
        let op = if exclusive {
            rustix::fs::FlockOperation::LockExclusive
        } else {
            rustix::fs::FlockOperation::LockShared
        };
        rustix::fs::flock(&file, op).map_err(|e| io_err(path, std::io::Error::from(e)))?;
        Ok(StateLock { _file: file })
    }
}

/// Create a directory (and its parents) at `0700` if it is not already there.
fn ensure_dir(dir: Option<&Path>) -> Result<()> {
    let Some(dir) = dir else { return Ok(()) };
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    // `create_dir_all` honours the umask; restate the mode so a permissive umask cannot
    // leave `$STATE` group-readable.
    fs::set_permissions(dir, Permissions::from_mode(DIR_MODE)).map_err(|e| io_err(dir, e))?;
    Ok(())
}

/// Owns `$STATE` and every read/write against it.
#[derive(Debug)]
pub struct Store {
    /* C-04 */
    paths: Paths,
}

impl Store {
    /// Bind a store to a resolved path set.
    pub fn new(paths: Paths) -> Store {
        Store { paths }
    }

    /// tmp in the SAME dir → write → `fsync(file)` → `rename` → `fsync(dir)`; mode set at
    /// `OpenOptions` time so the file is never briefly world-readable.
    ///
    /// The temp file shares the destination's directory so the `rename` is same-filesystem
    /// and therefore atomic. A reader racing the rename sees either the whole old file or
    /// the whole new one — never a prefix, never zero bytes.
    pub fn write_atomic(&self, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        let dir = path.parent().ok_or_else(|| Error::Invalid {
            what: "state path".to_string(),
            why: format!("{} has no parent directory", path.display()),
        })?;
        ensure_dir(Some(dir))?;

        let tmp_path = dir.join(tmp_name(path));
        let mut tmp = TmpFile::new(tmp_path);

        {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&tmp.path)
                .map_err(|e| io_err(&tmp.path, e))?;
            f.write_all(bytes).map_err(|e| io_err(&tmp.path, e))?;
            // Durability before visibility: the bytes must be on the platter before a name
            // points at them, or a crash yields a file that exists and is empty.
            f.sync_all().map_err(|e| io_err(&tmp.path, e))?;
        }

        // `OpenOptions::mode` is masked by the umask; restate it so `0600` is a fact and
        // not a hope. Still after `create_new`, so there is no permissive window.
        fs::set_permissions(&tmp.path, Permissions::from_mode(mode))
            .map_err(|e| io_err(&tmp.path, e))?;

        fs::rename(&tmp.path, path).map_err(|e| io_err(path, e))?;
        tmp.disarm();

        // fsync the directory so the rename itself survives a power cut.
        let dir_file = File::open(dir).map_err(|e| io_err(dir, e))?;
        dir_file.sync_all().map_err(|e| io_err(dir, e))?;
        Ok(())
    }

    /// `Ok(None)` when the file does not exist — a missing state file is normal.
    ///
    /// A zero-length file is also `None`: that is the signature of a pre-ApexRouter
    /// non-atomic writer that died mid-write, and it is not worth failing a daemon start
    /// over. Anything else that will not parse is an error, loudly.
    pub fn read_json<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(path, e)),
        };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// Serialise and write atomically at `0600`.
    ///
    /// Pretty-printed with a trailing newline: everything in `$STATE` is something a human
    /// might `cat` or a script might `jq`.
    pub fn write_json<T: Serialize>(&self, path: &Path, v: &T) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(v)?;
        bytes.push(b'\n');
        self.write_atomic(path, &bytes, FILE_MODE)
    }

    /// `LOCK_SH` on `$STATE/state.lock` around an offline read.
    ///
    /// Offline readers **never** touch `apexrouterd.lock`, so a concurrent
    /// `apexrouter status --watch` can never make `apexrouter serve` believe a daemon is
    /// already running.
    pub fn with_state_lock_shared<R>(&self, f: impl FnOnce() -> Result<R>) -> Result<R> {
        let _guard = StateLock::acquire(&self.paths.state_lock(), false)?;
        f()
    }

    /// `LOCK_EX` on `$STATE/state.lock` around an offline read-modify-write.
    ///
    /// Not reentrant: taking either state lock again from inside `f` on the same thread
    /// deadlocks against ourselves. Do the whole read-modify-write in one closure.
    pub fn with_state_lock_exclusive<R>(&self, f: impl FnOnce() -> Result<R>) -> Result<R> {
        let _guard = StateLock::acquire(&self.paths.state_lock(), true)?;
        f()
    }

    /// Every `$STATE/endpoints/*.json`. A row that will not parse is reported, not fatal.
    ///
    /// Sorted by id, because `read_dir` order is whatever the filesystem feels like and a
    /// UI that reshuffles its rows on every refresh is a bug report waiting to happen.
    pub fn list_endpoints(&self) -> Result<Vec<EndpointRecord>> {
        let dir = self.paths.endpoints_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&dir, e)),
        };

        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&dir, e))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match self.read_json::<EndpointRecord>(&path) {
                Ok(Some(rec)) => out.push(rec),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "skipping unreadable endpoint record");
                }
            }
        }
        out.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(out)
    }

    /// Write one endpoint record atomically at `0600`.
    pub fn put_endpoint(&self, r: &EndpointRecord) -> Result<()> {
        self.write_json(&self.paths.endpoint_file(&r.id), r)
    }

    /// Forget one endpoint record. Idempotent: already-absent is success.
    pub fn remove_endpoint(&self, id: &BackendId) -> Result<()> {
        let path = self.paths.endpoint_file(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(&path, e)),
        }
    }

    /// `$STATE/routes.json`.
    ///
    /// A missing file is a working zero-config install, not an error: it yields an empty
    /// table pointed at the default alias.
    pub fn load_routes(&self) -> Result<RouteFile> {
        match self.read_json::<RouteFile>(&self.paths.routes_file())? {
            Some(f) => Ok(f),
            None => Ok(RouteFile {
                schema_version: ROUTES_SCHEMA_VERSION,
                default_alias: Alias::parse(DEFAULT_ALIAS)?,
                routes: Vec::new(),
            }),
        }
    }

    /// `$STATE/routes.json`, atomically.
    pub fn save_routes(&self, f: &RouteFile) -> Result<()> {
        self.write_json(&self.paths.routes_file(), f)
    }

    /// `$STATE/backends.json`, minus credentials.
    pub fn load_backends(&self) -> Result<Vec<Backend>> {
        Ok(self
            .read_json::<Vec<Backend>>(&self.paths.backends_file())?
            .unwrap_or_default())
    }

    /// `$STATE/backends.json`, atomically.
    pub fn save_backends(&self, b: &[Backend]) -> Result<()> {
        self.write_json(&self.paths.backends_file(), &b)
    }

    /// `$STATE/tunnels.json`.
    pub fn load_tunnels(&self) -> Result<Vec<TunnelStatus>> {
        Ok(self
            .read_json::<Vec<TunnelStatus>>(&self.paths.tunnels_file())?
            .unwrap_or_default())
    }

    /// `$STATE/tunnels.json`, atomically.
    pub fn save_tunnels(&self, t: &[TunnelStatus]) -> Result<()> {
        self.write_json(&self.paths.tunnels_file(), &t)
    }

    /// `$STATE/services.json`. Absent file = no studio services yet, not an error.
    pub fn load_services(&self) -> Result<Vec<ServiceRecord>> {
        Ok(self
            .read_json::<Vec<ServiceRecord>>(&self.paths.services_file())?
            .unwrap_or_default())
    }

    /// `$STATE/services.json`, atomically.
    pub fn save_services(&self, s: &[ServiceRecord]) -> Result<()> {
        self.write_json(&self.paths.services_file(), &s)
    }

    /// `$STATE/studio.json`. Absent file = no active studio, not an error.
    pub fn load_studio(&self) -> Result<Option<StudioRecord>> {
        self.read_json::<StudioRecord>(&self.paths.studio_file())
    }

    /// `$STATE/studio.json`, atomically. Writing `None` removes the file.
    pub fn save_studio(&self, studio: Option<&StudioRecord>) -> Result<()> {
        match studio {
            Some(s) => self.write_json(&self.paths.studio_file(), s),
            None => {
                let path = self.paths.studio_file();
                match fs::remove_file(&path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(io_err(&path, e)),
                }
            }
        }
    }

    /// `$STATE/favorites.json`. Absent file = no verdicts, not an error.
    pub fn load_favorites(&self) -> Result<Vec<FavoriteHost>> {
        Ok(self
            .read_json::<Vec<FavoriteHost>>(&self.paths.favorites_file())?
            .unwrap_or_default())
    }

    /// `$STATE/favorites.json`, atomically.
    pub fn save_favorites(&self, f: &[FavoriteHost]) -> Result<()> {
        self.write_json(&self.paths.favorites_file(), &f)
    }

    /// The path set this store is bound to.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }
}

/// A temp name that is hidden, unique across threads *and* processes, and cannot be
/// mistaken for a state file by `list_endpoints` (its extension is never `json`).
fn tmp_name(dest: &Path) -> String {
    let stem = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".to_string());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    format!(".{}.tmp.{}.{}.{}", stem, std::process::id(), seq, nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{
        BackendKind, BackendLimits, CredentialSource, DesiredState, EndpointSpec, Health,
        InstanceId, NodeSpec, Protocol, Provenance, TunnelSpec,
    };
    use std::sync::{Arc, Mutex, OnceLock};

    /// `Paths::resolve()` reads process-global environment, so tests that redirect it
    /// serialise here. Held only across `set_var` + `resolve`.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// A `Store` rooted in a fresh tempdir. Nothing here ever touches the real `$HOME`.
    ///
    /// `Paths::resolve()` is the only public constructor, so redirecting it means touching
    /// the process environment. The previous value is put back before the lock is dropped,
    /// so the perturbation lasts exactly one `resolve()` and no other module's tests can
    /// observe a `$STATE` that is about to be deleted.
    fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = {
            let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var_os("APEXROUTER_HOME");
            std::env::set_var("APEXROUTER_HOME", dir.path());
            let p = Paths::resolve();
            match prev {
                Some(v) => std::env::set_var("APEXROUTER_HOME", v),
                None => std::env::remove_var("APEXROUTER_HOME"),
            }
            drop(guard);
            p.expect("Paths::resolve")
        };
        assert!(
            paths.state().starts_with(dir.path()),
            "state {} escaped the tempdir {}",
            paths.state().display(),
            dir.path().display()
        );
        (dir, Store::new(paths))
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o7777
    }

    fn endpoint(id: &str) -> EndpointRecord {
        EndpointRecord {
            id: BackendId::parse(id).expect("id"),
            spec: EndpointSpec::Node(NodeSpec {
                base_url: "http://127.0.0.1:8081".to_string(),
                credential: CredentialSource::None,
                label: format!("node {id}"),
                declared_models: vec!["qwen3".to_string()],
                protocol: Protocol::OpenAi,
            }),
            desired: DesiredState::Running,
            proc: None,
            port: Some(8081),
            log_path: None,
            started_at_unix: 1_785_412_331,
            fit: None,
            adopted: false,
            alias_bindings: Vec::new(),
        }
    }

    fn backend(id: &str) -> Backend {
        Backend {
            id: BackendId::parse(id).expect("id"),
            kind: BackendKind::Node,
            protocol: Protocol::OpenAi,
            label: "lan box".to_string(),
            base_url: "http://192.168.1.20:8080".to_string(),
            credential: CredentialSource::Env {
                var: "NODE_KEY".to_string(),
            },
            tags: vec!["local".to_string()],
            models: Vec::new(),
            limits: BackendLimits {
                max_concurrent: 4,
                queue_depth: 8,
                ctx: Some(32768),
                slots_total: Some(4),
            },
            price: None,
            health: Health::Unknown,
            provenance: Provenance::Manual,
            endpoint: None,
            enabled: true,
            devices: Vec::new(),
            last_error: None,
        }
    }

    fn tunnel(local_port: u16) -> TunnelStatus {
        TunnelStatus {
            spec: TunnelSpec {
                instance_id: InstanceId(4242),
                local_port,
                remote_port: 8000,
                ssh_host: "ssh7.vast.ai".to_string(),
                ssh_port: 12345,
            },
            up: true,
            proc: None,
            since_unix: Some(1_785_412_331),
            restarts: 0,
            last_error: None,
        }
    }

    #[test]
    fn write_atomic_writes_bytes_at_the_requested_mode() {
        let (_dir, store) = test_store();
        let p = store.paths().state().join("nested/deep/file.bin");
        store.write_atomic(&p, b"hello", 0o600).expect("write");
        assert_eq!(fs::read(&p).expect("read"), b"hello");
        assert_eq!(mode_of(&p), 0o600);
    }

    #[test]
    fn write_atomic_leaves_no_temp_file_behind() {
        let (_dir, store) = test_store();
        let d = store.paths().state().join("tmpcheck");
        let p = d.join("f.json");
        for i in 0..8u32 {
            store
                .write_atomic(&p, format!("{i}").as_bytes(), 0o600)
                .expect("write");
        }
        let names: Vec<_> = fs::read_dir(&d)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names.len(), 1, "leftovers: {names:?}");
        assert_eq!(fs::read(&p).expect("read"), b"7");
    }

    #[test]
    fn write_atomic_rejects_a_path_without_a_parent() {
        let (_dir, store) = test_store();
        let err = store
            .write_atomic(Path::new("/"), b"x", 0o600)
            .expect_err("root has no parent");
        assert!(matches!(err, Error::Invalid { .. }), "got {err:?}");
    }

    #[test]
    fn read_json_of_a_missing_file_is_none() {
        let (_dir, store) = test_store();
        let got: Option<Vec<String>> = store
            .read_json(&store.paths().state().join("absent.json"))
            .expect("read");
        assert!(got.is_none());
    }

    #[test]
    fn read_json_of_an_empty_file_is_none() {
        let (_dir, store) = test_store();
        let p = store.paths().state().join("empty.json");
        store.write_atomic(&p, b"", 0o600).expect("write");
        let got: Option<Vec<String>> = store.read_json(&p).expect("read");
        assert!(got.is_none());
    }

    #[test]
    fn read_json_of_garbage_is_an_error() {
        let (_dir, store) = test_store();
        let p = store.paths().state().join("garbage.json");
        store.write_atomic(&p, b"{ not json", 0o600).expect("write");
        let got: Result<Option<Vec<String>>> = store.read_json(&p);
        assert!(
            matches!(got, Err(Error::Json(_))),
            "corruption must be loud"
        );
    }

    #[test]
    fn write_json_round_trips_and_is_private() {
        let (_dir, store) = test_store();
        let p = store.paths().state().join("v.json");
        let v = vec!["a".to_string(), "b".to_string()];
        store.write_json(&p, &v).expect("write");
        assert_eq!(mode_of(&p), 0o600);
        let got: Option<Vec<String>> = store.read_json(&p).expect("read");
        assert_eq!(got, Some(v));
    }

    #[test]
    fn every_state_file_we_write_is_0600() {
        let (_dir, store) = test_store();
        store.put_endpoint(&endpoint("ep-a")).expect("endpoint");
        let routes = store.load_routes().expect("routes");
        store.save_routes(&routes).expect("save routes");
        store.save_backends(&[backend("be-a")]).expect("backends");
        store.save_tunnels(&[tunnel(8800)]).expect("tunnels");
        store
            .with_state_lock_exclusive(|| Ok(()))
            .expect("state lock");

        for p in [
            store
                .paths()
                .endpoint_file(&BackendId::parse("ep-a").expect("id")),
            store.paths().routes_file(),
            store.paths().backends_file(),
            store.paths().tunnels_file(),
            store.paths().state_lock(),
        ] {
            assert_eq!(mode_of(&p), 0o600, "{} is not 0600", p.display());
        }
    }

    #[test]
    fn endpoints_round_trip_and_remove_is_idempotent() {
        let (_dir, store) = test_store();
        assert!(store.list_endpoints().expect("empty").is_empty());

        store.put_endpoint(&endpoint("zeta")).expect("put");
        store.put_endpoint(&endpoint("alpha")).expect("put");
        let listed = store.list_endpoints().expect("list");
        assert_eq!(
            listed.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"],
            "list_endpoints must be sorted by id"
        );
        assert_eq!(listed[0], endpoint("alpha"));

        let id = BackendId::parse("alpha").expect("id");
        store.remove_endpoint(&id).expect("remove");
        store.remove_endpoint(&id).expect("remove again is fine");
        assert_eq!(store.list_endpoints().expect("list").len(), 1);
    }

    #[test]
    fn an_unparseable_endpoint_record_is_skipped_not_fatal() {
        let (_dir, store) = test_store();
        store.put_endpoint(&endpoint("good")).expect("put");
        let junk = store.paths().endpoints_dir().join("broken.json");
        store
            .write_atomic(&junk, b"{\"id\":", 0o600)
            .expect("write");
        // A non-json sibling (a stray note, a temp name) is ignored outright.
        store
            .write_atomic(
                &store.paths().endpoints_dir().join("notes.txt"),
                b"hi",
                0o600,
            )
            .expect("write");

        let listed = store.list_endpoints().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.as_str(), "good");
    }

    #[test]
    fn routes_backends_and_tunnels_default_when_absent() {
        let (_dir, store) = test_store();
        let routes = store.load_routes().expect("routes");
        assert_eq!(routes.schema_version, ROUTES_SCHEMA_VERSION);
        assert_eq!(routes.default_alias.as_str(), DEFAULT_ALIAS);
        assert!(routes.routes.is_empty());
        assert!(store.load_backends().expect("backends").is_empty());
        assert!(store.load_tunnels().expect("tunnels").is_empty());
    }

    #[test]
    fn routes_backends_and_tunnels_round_trip() {
        let (_dir, store) = test_store();
        let mut routes = store.load_routes().expect("routes");
        routes.default_alias = Alias::parse("code").expect("alias");
        store.save_routes(&routes).expect("save");
        assert_eq!(store.load_routes().expect("reload"), routes);

        let backends = vec![backend("be-a"), backend("be-b")];
        store.save_backends(&backends).expect("save");
        assert_eq!(store.load_backends().expect("reload"), backends);

        let tunnels = vec![tunnel(8800), tunnel(8801)];
        store.save_tunnels(&tunnels).expect("save");
        assert_eq!(store.load_tunnels().expect("reload"), tunnels);
    }

    #[test]
    fn services_and_studio_default_when_absent_and_round_trip() {
        use apexrouter_protocol::{
            DesiredState, ServiceHealthProbe, ServiceId, ServiceRecord, ServiceRuntime,
            StudioRecord, STUDIO_VIDEO_PORT,
        };

        let (_dir, store) = test_store();
        // Pre-studio state dir: absent files are empty / None, never an error.
        assert!(store.load_services().expect("services").is_empty());
        assert!(store.load_studio().expect("studio").is_none());

        let svc = ServiceRecord {
            id: ServiceId::parse("studio-video").expect("id"),
            instance_id: InstanceId(140_330),
            name: "video".into(),
            runtime: ServiceRuntime::ComfyUi,
            remote_port: 8188,
            local_port: STUDIO_VIDEO_PORT,
            health: ServiceHealthProbe::ComfySystemStats,
            devices: vec!["0".into()],
            reserved_mb: 23_000,
            desired: DesiredState::Running,
            started_at_unix: 1_780_000_000,
        };
        store.save_services(&[svc.clone()]).expect("save services");
        assert_eq!(store.load_services().expect("reload"), vec![svc]);

        let studio = StudioRecord {
            instance_id: InstanceId(140_330),
            machine_id: Some(140_330),
            recipe_id: None,
            profile_id: None,
            service_ids: vec![ServiceId::parse("studio-video").expect("id")],
            endpoint_ids: vec![],
            created_at_unix: 1_780_000_000,
            updated_at_unix: 1_780_000_000,
        };
        store.save_studio(Some(&studio)).expect("save studio");
        assert_eq!(store.load_studio().expect("reload"), Some(studio));

        store.save_studio(None).expect("clear studio");
        assert!(store.load_studio().expect("gone").is_none());
        // Clearing twice is fine.
        store.save_studio(None).expect("clear again");
    }

    #[test]
    fn shared_locks_do_not_exclude_each_other() {
        let (_dir, store) = test_store();
        let store = Arc::new(store);
        let inner = Arc::clone(&store);
        // A shared lock held while a second shared lock is taken must not deadlock.
        let got = store
            .with_state_lock_shared(|| {
                let t = std::thread::spawn(move || inner.with_state_lock_shared(|| Ok(7)));
                t.join().expect("join")
            })
            .expect("shared locks");
        assert_eq!(got, 7);
    }

    /// The acceptance test: 16 threads × 100 read-modify-write updates under the exclusive
    /// state lock leave a parseable file with **no lost update**.
    #[test]
    fn sixteen_threads_times_one_hundred_updates_lose_nothing() {
        let (_dir, store) = test_store();
        let store = Arc::new(store);
        let counter = store.paths().state().join("counter.json");
        store.write_json(&counter, &0u64).expect("seed");

        let mut handles = Vec::new();
        for _ in 0..16 {
            let store = Arc::clone(&store);
            let counter = counter.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    store
                        .with_state_lock_exclusive(|| {
                            let n: u64 = store.read_json(&counter)?.unwrap_or(0);
                            store.write_json(&counter, &(n + 1))
                        })
                        .expect("update");
                }
            }));
        }
        for h in handles {
            h.join().expect("thread panicked");
        }

        let n: Option<u64> = store.read_json(&counter).expect("parseable");
        assert_eq!(n, Some(1600), "an update was lost");
    }

    /// A reader racing a writer sees whole files only — never a prefix, never zero bytes.
    #[test]
    fn a_reader_during_a_rename_never_sees_a_partial_file() {
        let (_dir, store) = test_store();
        let store = Arc::new(store);
        let path = store.paths().state().join("race.json");

        // Big enough that a non-atomic write would span many `write(2)` calls.
        let a: Vec<String> = (0..2000).map(|i| format!("alpha-{i}")).collect();
        let b: Vec<String> = (0..2000).map(|i| format!("bravo-{i}")).collect();
        store.write_json(&path, &a).expect("seed");

        let writer = {
            let store = Arc::clone(&store);
            let path = path.clone();
            let (a, b) = (a.clone(), b.clone());
            std::thread::spawn(move || {
                for i in 0..150 {
                    let v = if i % 2 == 0 { &a } else { &b };
                    store.write_json(&path, v).expect("write");
                }
            })
        };

        let mut reads = 0u32;
        while !writer.is_finished() || reads < 150 {
            let got: Vec<String> = store
                .read_json(&path)
                .expect("a partial file would fail to parse")
                .expect("the file always exists once seeded");
            assert!(got == a || got == b, "torn read of {} items", got.len());
            reads += 1;
        }
        writer.join().expect("writer panicked");
        assert!(reads >= 150);
    }
}
