//! OWNER: unit C-01 (core/paths.rs, core/error.rs). Do not edit outside that unit.
//!
//! Path resolution. **Nothing is ever written into a repo directory** — invariant 5.
//!
//! ```text
//! $APEXROUTER_CONFIG  ->  $APEXROUTER_HOME/config.toml  ->  $XDG_CONFIG_HOME/apexrouter/config.toml
//! $APEXROUTER_HOME    ->  $XDG_STATE_HOME/apexrouter/     (state)
//!                         $XDG_CACHE_HOME/apexrouter/     (HF metadata, --help probes, offers)
//! ```
//!
//! Global CLI flags (`--config`, `--home`) are pushed into the process env **before**
//! `Config::load()`, so env vars stay the single resolution mechanism.
//!
//! Every path is resolved **once**, at startup, into an immutable [`Paths`]. Nothing on a
//! request path ever recomputes one, and no other module is allowed to join `$STATE`
//! by hand — if a file lives under the state dir it gets an accessor here.

use crate::error::{Error, Result};
use apexrouter_protocol::{BackendId, InstanceId};
use std::ffi::OsString;
use std::fs::DirBuilder;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Directory mode for everything we create: owner-only. `credentials.toml`, minted
/// `--api-key-file`s and the ssh ControlMaster sockets all live under here.
const DIR_MODE: u32 = 0o700;

/// Read an environment variable, treating the empty string as "unset".
///
/// `APEXROUTER_HOME=` in a systemd unit is a mistake, not a request to use `""` as a
/// directory, and silently rooting the state tree at the process cwd would violate
/// invariant 5 in the worst possible way.
fn env_path(get: &dyn Fn(&str) -> Option<OsString>, key: &str) -> Option<PathBuf> {
    let v = get(key)?;
    if v.is_empty() {
        return None;
    }
    Some(PathBuf::from(v))
}

/// Same as [`env_path`], but honours the XDG rule that a **relative** value is invalid
/// and must be ignored rather than resolved against the cwd.
fn xdg_path(get: &dyn Fn(&str) -> Option<OsString>, key: &str) -> Option<PathBuf> {
    env_path(get, key).filter(|p| p.is_absolute())
}

/// Every path the daemon and the offline readers use. Resolved once, at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// `config.toml`, wherever the three-step chain landed.
    config_file: PathBuf,
    /// The state root. Everything mutable and durable lives under it.
    state: PathBuf,
    /// The cache root. Everything under it is reconstructible.
    cache: PathBuf,
    /// Where the previous generation kept its things.
    legacy: LegacyPaths,
}

impl Paths {
    /// Resolve from the environment. **Never** `exit()`s: a library returns errors.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] when no home directory can be determined and none of
    /// `$APEXROUTER_HOME` / `$XDG_STATE_HOME` / `$XDG_CACHE_HOME` supply one either.
    pub fn resolve() -> Result<Paths> {
        Paths::from_env(&|k| std::env::var_os(k))
    }

    /// The whole of [`resolve`](Self::resolve), with the environment injected.
    ///
    /// Private on purpose: env vars stay the single resolution mechanism for callers.
    /// Tests use this instead of mutating the process environment, which is global,
    /// racy under `cargo test`'s thread pool, and would make two units' tests collide.
    fn from_env(get: &dyn Fn(&str) -> Option<OsString>) -> Result<Paths> {
        // `dirs::home_dir()` reads $HOME first and falls back to the passwd database, so a
        // daemon started with a scrubbed environment still resolves. The injected lookup
        // wins so tests are hermetic.
        let home = env_path(get, "HOME")
            .or_else(dirs::home_dir)
            .ok_or_else(|| Error::Invalid {
                what: "environment".into(),
                why: "no home directory: set $HOME, or $APEXROUTER_HOME to an absolute path".into(),
            })?;

        let apex_home = env_path(get, "APEXROUTER_HOME");

        // ---- state: $APEXROUTER_HOME -> $XDG_STATE_HOME/apexrouter -> ~/.local/state/apexrouter
        let state = match apex_home.clone() {
            Some(h) => h,
            None => match xdg_path(get, "XDG_STATE_HOME") {
                Some(x) => x.join("apexrouter"),
                None => home.join(".local/state/apexrouter"),
            },
        };

        // ---- cache: $XDG_CACHE_HOME/apexrouter -> ~/.cache/apexrouter
        let cache_home = xdg_path(get, "XDG_CACHE_HOME").unwrap_or_else(|| home.join(".cache"));
        let cache = cache_home.join("apexrouter");

        // ---- config: $APEXROUTER_CONFIG -> $APEXROUTER_HOME/config.toml
        //              -> $XDG_CONFIG_HOME/apexrouter/config.toml -> ~/.config/apexrouter/…
        let config_file = match env_path(get, "APEXROUTER_CONFIG") {
            Some(f) => f,
            None => match apex_home {
                Some(h) => h.join("config.toml"),
                None => xdg_path(get, "XDG_CONFIG_HOME")
                    .unwrap_or_else(|| home.join(".config"))
                    .join("apexrouter")
                    .join("config.toml"),
            },
        };

        let legacy = LegacyPaths::resolve(get, &home, &cache_home);

        Ok(Paths {
            config_file,
            state,
            cache,
            legacy,
        })
    }

    /// `config.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.config_file.clone()
    }

    /// The state directory. Everything mutable lives under it.
    pub fn state(&self) -> &Path {
        &self.state
    }

    /// The cache directory. Everything here is reconstructible.
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// `$STATE/routes.json`.
    pub fn routes_file(&self) -> PathBuf {
        self.state.join("routes.json")
    }

    /// `$STATE/backends.json`.
    pub fn backends_file(&self) -> PathBuf {
        self.state.join("backends.json")
    }

    /// `$STATE/tunnels.json`.
    pub fn tunnels_file(&self) -> PathBuf {
        self.state.join("tunnels.json")
    }

    /// `$STATE/catalog.toml`.
    pub fn catalog_file(&self) -> PathBuf {
        self.state.join("catalog.toml")
    }

    /// `$STATE/credentials.toml`, mode 0600.
    pub fn credentials_file(&self) -> PathBuf {
        self.state.join("credentials.toml")
    }

    /// `$STATE/ledger.jsonl`.
    pub fn ledger(&self) -> PathBuf {
        self.state.join("ledger.jsonl")
    }

    /// `$STATE/usage.jsonl`.
    pub fn usage_log(&self) -> PathBuf {
        self.state.join("usage.jsonl")
    }

    /// `$STATE/endpoints/`.
    pub fn endpoints_dir(&self) -> PathBuf {
        self.state.join("endpoints")
    }

    /// `$STATE/endpoints/<id>.json`.
    ///
    /// [`BackendId`] is validated against `^[a-z0-9][a-z0-9._-]{0,63}$` and cannot contain
    /// `/` or `..`, so this join can never escape the state directory.
    pub fn endpoint_file(&self, id: &BackendId) -> PathBuf {
        self.endpoints_dir().join(format!("{}.json", id.as_str()))
    }

    /// `$STATE/jobs/`.
    pub fn jobs_dir(&self) -> PathBuf {
        self.state.join("jobs")
    }

    /// `$STATE/approvals/`.
    pub fn approvals_dir(&self) -> PathBuf {
        self.state.join("approvals")
    }

    /// `$STATE/logs/`. **Never watched** — children write to it continuously.
    pub fn logs_dir(&self) -> PathBuf {
        self.state.join("logs")
    }

    /// `$STATE/logs/<backend-id>.log`.
    pub fn log_file(&self, id: &BackendId) -> PathBuf {
        self.logs_dir().join(format!("{}.log", id.as_str()))
    }

    /// `$STATE/ssh/`.
    pub fn ssh_dir(&self) -> PathBuf {
        self.state.join("ssh")
    }

    /// A dedicated `known_hosts`, because vast recycles `sshN.vast.ai` hostnames.
    pub fn known_hosts(&self) -> PathBuf {
        self.ssh_dir().join("known_hosts")
    }

    /// `$STATE/ssh/cm-<instance-id>` — the ControlMaster socket.
    pub fn control_path(&self, id: InstanceId) -> PathBuf {
        self.ssh_dir().join(format!("cm-{}", id.0))
    }

    /// `$STATE/apexrouterd.lock`. **Only the daemon ever touches this file.**
    pub fn daemon_lock(&self) -> PathBuf {
        self.state.join("apexrouterd.lock")
    }

    /// `$STATE/state.lock`, for offline read-modify-write coordination.
    pub fn state_lock(&self) -> PathBuf {
        self.state.join("state.lock")
    }

    /// Where LocalRouter and `~/.vastai-gguf` live.
    pub fn legacy(&self) -> &LegacyPaths {
        &self.legacy
    }

    /// Create every directory at `0700`. Idempotent.
    ///
    /// A directory that already exists with looser permissions is tightened; if that
    /// `chmod` fails (someone pointed `$APEXROUTER_HOME` at a directory they do not own)
    /// it is a warning, not a failure — the files we write set their own mode at
    /// `OpenOptions` time.
    ///
    /// # Errors
    /// Returns [`Error::Io`] naming the directory that could not be created.
    pub fn ensure_layout(&self) -> Result<()> {
        let mut dirs: Vec<PathBuf> = vec![
            self.state.clone(),
            self.cache.clone(),
            self.endpoints_dir(),
            self.jobs_dir(),
            self.approvals_dir(),
            self.logs_dir(),
            self.ssh_dir(),
        ];
        // The config file may live anywhere the user pointed $APEXROUTER_CONFIG; its parent
        // has to exist before `config init` can write it.
        if let Some(parent) = self.config_file.parent() {
            dirs.push(parent.to_path_buf());
        }

        for dir in dirs {
            ensure_dir(&dir)?;
        }
        Ok(())
    }
}

/// `mkdir -p` at [`DIR_MODE`], tightening an existing directory best-effort.
fn ensure_dir(dir: &Path) -> Result<()> {
    let existed = dir.is_dir();
    if !existed {
        DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(dir)
            .map_err(|source| Error::Io {
                path: dir.display().to_string(),
                source,
            })?;
    }

    let meta = std::fs::metadata(dir).map_err(|source| Error::Io {
        path: dir.display().to_string(),
        source,
    })?;
    if !meta.is_dir() {
        return Err(Error::Invalid {
            what: dir.display().to_string(),
            why: "exists but is not a directory".into(),
        });
    }
    if meta.permissions().mode() & 0o777 != DIR_MODE {
        let res = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(DIR_MODE));
        if let Err(e) = res {
            if existed {
                tracing::warn!(dir = %dir.display(), error = %e, "could not tighten directory to 0700");
            } else {
                return Err(Error::Io {
                    path: dir.display().to_string(),
                    source: e,
                });
            }
        }
    }
    Ok(())
}

/// Where the previous generation kept its things. Read-only unless migration says otherwise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyPaths {
    /// `~/.vastai-gguf`.
    pub vastai_gguf: PathBuf,
    /// The LocalRouter checkout, when we can find one.
    pub localrouter_dir: Option<PathBuf>,
    /// `~/.config/vastai/vast_api_key`.
    pub vast_key: PathBuf,
    /// `~/.cache/huggingface/token`.
    pub hf_token: PathBuf,
}

impl LegacyPaths {
    /// Resolve the legacy locations.
    ///
    /// The two third-party credential paths follow **their owners' conventions**, not ours:
    /// the `vastai` CLI hard-codes `~/.config/vastai/vast_api_key`, while `huggingface_hub`
    /// honours `$HF_TOKEN_PATH`, then `$HF_HOME/token`, then `<cache home>/huggingface/token`.
    fn resolve(
        get: &dyn Fn(&str) -> Option<OsString>,
        home: &Path,
        cache_home: &Path,
    ) -> LegacyPaths {
        let hf_token = env_path(get, "HF_TOKEN_PATH").unwrap_or_else(|| {
            env_path(get, "HF_HOME")
                .unwrap_or_else(|| cache_home.join("huggingface"))
                .join("token")
        });

        LegacyPaths {
            vastai_gguf: home.join(".vastai-gguf"),
            localrouter_dir: find_localrouter(get, home),
            vast_key: home.join(".config/vastai/vast_api_key"),
            hf_token,
        }
    }
}

/// Locate a LocalRouter checkout, if one is lying around.
///
/// `$APEXROUTER_LOCALROUTER_DIR` is the escape hatch and is trusted when it is a directory;
/// otherwise the usual spots are probed and must carry a marker file, so a directory that
/// merely shares the name is not mistaken for the real thing. Absent is the normal case —
/// migration reads `~/.vastai-gguf` regardless, and the checkout only adds the four
/// repo-directory state files (`.active_endpoint`, `.last_instance`, `.instance_history`,
/// `.hf_pin`) plus `recipes.toml`.
fn find_localrouter(get: &dyn Fn(&str) -> Option<OsString>, home: &Path) -> Option<PathBuf> {
    if let Some(explicit) = env_path(get, "APEXROUTER_LOCALROUTER_DIR") {
        return explicit.is_dir().then_some(explicit);
    }
    const CANDIDATES: [&str; 4] = [
        "Projects/Inference/tools/LocalRouter",
        "Projects/LocalRouter",
        "LocalRouter",
        "src/LocalRouter",
    ];
    CANDIDATES
        .iter()
        .map(|c| home.join(c))
        .find(|d| d.join("endpoint_proxy.py").is_file() || d.join("recipes.toml").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a hermetic environment lookup. Nothing here touches the process env, so these
    /// tests cannot race a sibling unit's tests inside the same test binary.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(*v)))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn s(p: &Path) -> String {
        p.display().to_string()
    }

    // ---- config resolution: branch 1 of 3 -------------------------------------------------

    #[test]
    fn config_branch_explicit_env_wins() {
        let home = tmp();
        let cfg = home.path().join("elsewhere/apex.toml");
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("APEXROUTER_CONFIG", &s(&cfg)),
            // set too, and must lose
            ("APEXROUTER_HOME", &s(&home.path().join("state"))),
        ]))
        .expect("resolve");
        assert_eq!(p.config_file(), cfg);
        // …while $APEXROUTER_HOME still decides the state dir.
        assert_eq!(p.state(), home.path().join("state"));
    }

    // ---- config resolution: branch 2 of 3 -------------------------------------------------

    #[test]
    fn config_branch_apexrouter_home() {
        let home = tmp();
        let apex = home.path().join("apexhome");
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("APEXROUTER_HOME", &s(&apex)),
            // must be ignored for the config file, because $APEXROUTER_HOME is set
            ("XDG_CONFIG_HOME", &s(&home.path().join("xdgcfg"))),
        ]))
        .expect("resolve");
        assert_eq!(p.config_file(), apex.join("config.toml"));
    }

    // ---- config resolution: branch 3 of 3 -------------------------------------------------

    #[test]
    fn config_branch_xdg_config_home() {
        let home = tmp();
        let xdg = home.path().join("xdgcfg");
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("XDG_CONFIG_HOME", &s(&xdg)),
        ]))
        .expect("resolve");
        assert_eq!(p.config_file(), xdg.join("apexrouter/config.toml"));
    }

    #[test]
    fn config_branch_xdg_falls_back_to_dot_config() {
        let home = tmp();
        let p = Paths::from_env(&env(&[("HOME", &s(home.path()))])).expect("resolve");
        assert_eq!(
            p.config_file(),
            home.path().join(".config/apexrouter/config.toml")
        );
    }

    // ---- state resolution: both branches --------------------------------------------------

    #[test]
    fn state_branch_apexrouter_home() {
        let home = tmp();
        let apex = home.path().join("apexhome");
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("APEXROUTER_HOME", &s(&apex)),
            // must lose to $APEXROUTER_HOME
            ("XDG_STATE_HOME", &s(&home.path().join("xdgstate"))),
        ]))
        .expect("resolve");
        assert_eq!(p.state(), apex);
    }

    #[test]
    fn state_branch_xdg_state_home() {
        let home = tmp();
        let xdg = home.path().join("xdgstate");
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("XDG_STATE_HOME", &s(&xdg)),
        ]))
        .expect("resolve");
        assert_eq!(p.state(), xdg.join("apexrouter"));
    }

    #[test]
    fn state_branch_xdg_falls_back_to_local_state() {
        let home = tmp();
        let p = Paths::from_env(&env(&[("HOME", &s(home.path()))])).expect("resolve");
        assert_eq!(p.state(), home.path().join(".local/state/apexrouter"));
        assert_eq!(p.cache(), home.path().join(".cache/apexrouter"));
    }

    #[test]
    fn cache_honours_xdg_cache_home() {
        let home = tmp();
        let xdg = home.path().join("xdgcache");
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("XDG_CACHE_HOME", &s(&xdg)),
        ]))
        .expect("resolve");
        assert_eq!(p.cache(), xdg.join("apexrouter"));
    }

    // ---- env hygiene ----------------------------------------------------------------------

    #[test]
    fn empty_env_values_are_treated_as_unset() {
        let home = tmp();
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("APEXROUTER_HOME", ""),
            ("APEXROUTER_CONFIG", ""),
            ("XDG_STATE_HOME", ""),
        ]))
        .expect("resolve");
        assert_eq!(p.state(), home.path().join(".local/state/apexrouter"));
        assert_eq!(
            p.config_file(),
            home.path().join(".config/apexrouter/config.toml")
        );
    }

    #[test]
    fn relative_xdg_values_are_ignored() {
        // XDG spec: a relative $XDG_*_HOME is invalid and must be ignored. Honouring one
        // would root the state tree at the cwd — which, for `cargo test`, IS the repo.
        let home = tmp();
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("XDG_STATE_HOME", "relative/state"),
            ("XDG_CACHE_HOME", "relative/cache"),
            ("XDG_CONFIG_HOME", "relative/config"),
        ]))
        .expect("resolve");
        assert_eq!(p.state(), home.path().join(".local/state/apexrouter"));
        assert_eq!(p.cache(), home.path().join(".cache/apexrouter"));
        assert_eq!(
            p.config_file(),
            home.path().join(".config/apexrouter/config.toml")
        );
    }

    // ---- derived paths --------------------------------------------------------------------

    #[test]
    fn every_derived_path_lives_under_the_state_dir() {
        let home = tmp();
        let apex = home.path().join("apexhome");
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("APEXROUTER_HOME", &s(&apex)),
        ]))
        .expect("resolve");
        let id = BackendId::parse("local-carnice").expect("id");

        for path in [
            p.routes_file(),
            p.backends_file(),
            p.tunnels_file(),
            p.catalog_file(),
            p.credentials_file(),
            p.ledger(),
            p.usage_log(),
            p.endpoints_dir(),
            p.endpoint_file(&id),
            p.jobs_dir(),
            p.approvals_dir(),
            p.logs_dir(),
            p.log_file(&id),
            p.ssh_dir(),
            p.known_hosts(),
            p.control_path(InstanceId(28_675_431)),
            p.daemon_lock(),
            p.state_lock(),
        ] {
            assert!(path.starts_with(&apex), "{} escaped $STATE", path.display());
        }

        assert_eq!(
            p.endpoint_file(&id),
            apex.join("endpoints/local-carnice.json")
        );
        assert_eq!(p.log_file(&id), apex.join("logs/local-carnice.log"));
        assert_eq!(
            p.control_path(InstanceId(28_675_431)),
            apex.join("ssh/cm-28675431")
        );
        assert_eq!(p.known_hosts(), apex.join("ssh/known_hosts"));
        assert_eq!(p.daemon_lock(), apex.join("apexrouterd.lock"));
        assert_eq!(p.state_lock(), apex.join("state.lock"));
    }

    // ---- legacy ---------------------------------------------------------------------------

    #[test]
    fn legacy_paths_follow_their_owners_conventions() {
        let home = tmp();
        let p = Paths::from_env(&env(&[("HOME", &s(home.path()))])).expect("resolve");
        let l = p.legacy();
        assert_eq!(l.vastai_gguf, home.path().join(".vastai-gguf"));
        assert_eq!(l.vast_key, home.path().join(".config/vastai/vast_api_key"));
        assert_eq!(l.hf_token, home.path().join(".cache/huggingface/token"));
        assert_eq!(l.localrouter_dir, None);
    }

    #[test]
    fn hf_token_honours_hf_home_and_xdg_cache_home() {
        let home = tmp();
        let hf = home.path().join("hf");
        let p = Paths::from_env(&env(&[("HOME", &s(home.path())), ("HF_HOME", &s(&hf))]))
            .expect("resolve");
        assert_eq!(p.legacy().hf_token, hf.join("token"));

        let xdg = home.path().join("xdgcache");
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("XDG_CACHE_HOME", &s(&xdg)),
        ]))
        .expect("resolve");
        assert_eq!(p.legacy().hf_token, xdg.join("huggingface/token"));
    }

    #[test]
    fn localrouter_is_found_by_marker_file_only() {
        let home = tmp();
        let checkout = home.path().join("Projects/Inference/tools/LocalRouter");
        std::fs::create_dir_all(&checkout).expect("mkdir");

        // A directory with the right name but no marker is not a checkout.
        let p = Paths::from_env(&env(&[("HOME", &s(home.path()))])).expect("resolve");
        assert_eq!(p.legacy().localrouter_dir, None);

        std::fs::write(checkout.join("endpoint_proxy.py"), b"# legacy\n").expect("write");
        let p = Paths::from_env(&env(&[("HOME", &s(home.path()))])).expect("resolve");
        assert_eq!(p.legacy().localrouter_dir, Some(checkout));
    }

    #[test]
    fn localrouter_env_override_is_trusted_when_it_is_a_directory() {
        let home = tmp();
        let explicit = home.path().join("somewhere/else");
        std::fs::create_dir_all(&explicit).expect("mkdir");
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("APEXROUTER_LOCALROUTER_DIR", &s(&explicit)),
        ]))
        .expect("resolve");
        assert_eq!(p.legacy().localrouter_dir, Some(explicit));

        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("APEXROUTER_LOCALROUTER_DIR", &s(&home.path().join("nope"))),
        ]))
        .expect("resolve");
        assert_eq!(p.legacy().localrouter_dir, None);
    }

    // ---- ensure_layout --------------------------------------------------------------------

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p)
            .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn ensure_layout_creates_every_dir_at_0700_and_is_idempotent() {
        let home = tmp();
        let apex = home.path().join("apexhome");
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("APEXROUTER_HOME", &s(&apex)),
        ]))
        .expect("resolve");

        for _ in 0..2 {
            p.ensure_layout().expect("ensure_layout");
            for dir in [
                p.state().to_path_buf(),
                p.cache().to_path_buf(),
                p.endpoints_dir(),
                p.jobs_dir(),
                p.approvals_dir(),
                p.logs_dir(),
                p.ssh_dir(),
                p.config_file().parent().expect("parent").to_path_buf(),
            ] {
                assert!(dir.is_dir(), "{} was not created", dir.display());
                assert_eq!(mode_of(&dir), 0o700, "{} is not 0700", dir.display());
            }
        }
    }

    #[test]
    fn ensure_layout_tightens_a_pre_existing_loose_directory() {
        let home = tmp();
        let apex = home.path().join("apexhome");
        std::fs::create_dir_all(&apex).expect("mkdir");
        std::fs::set_permissions(&apex, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("APEXROUTER_HOME", &s(&apex)),
        ]))
        .expect("resolve");
        p.ensure_layout().expect("ensure_layout");
        assert_eq!(mode_of(&apex), 0o700);
    }

    #[test]
    fn ensure_layout_refuses_a_state_path_that_is_a_file() {
        let home = tmp();
        let apex = home.path().join("apexhome");
        std::fs::write(&apex, b"not a directory").expect("write");
        let p = Paths::from_env(&env(&[
            ("HOME", &s(home.path())),
            ("APEXROUTER_HOME", &s(&apex)),
        ]))
        .expect("resolve");
        assert!(p.ensure_layout().is_err());
    }

    // ---- invariant 5 ----------------------------------------------------------------------

    #[test]
    fn nothing_resolves_to_a_path_inside_the_repo() {
        // The real environment, on whatever box this runs on.
        let p = Paths::resolve().expect("resolve");
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("repo root")
            .to_path_buf();
        assert!(repo.join("Cargo.toml").is_file(), "repo root check");

        let id = BackendId::parse("local-carnice").expect("id");
        for path in [
            p.config_file(),
            p.state().to_path_buf(),
            p.cache().to_path_buf(),
            p.routes_file(),
            p.backends_file(),
            p.tunnels_file(),
            p.catalog_file(),
            p.credentials_file(),
            p.ledger(),
            p.usage_log(),
            p.endpoints_dir(),
            p.endpoint_file(&id),
            p.jobs_dir(),
            p.approvals_dir(),
            p.logs_dir(),
            p.log_file(&id),
            p.ssh_dir(),
            p.known_hosts(),
            p.control_path(InstanceId(1)),
            p.daemon_lock(),
            p.state_lock(),
            p.legacy().vastai_gguf.clone(),
            p.legacy().vast_key.clone(),
            p.legacy().hf_token.clone(),
        ] {
            assert!(
                path.is_absolute(),
                "{} is not absolute — a relative path resolves against the cwd, which during \
                 a test IS the repo",
                path.display()
            );
            assert!(
                !path.starts_with(&repo),
                "{} is inside the repo — invariant 5",
                path.display()
            );
        }
    }
}
