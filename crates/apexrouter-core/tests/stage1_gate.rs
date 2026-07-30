//! Stage 1 gate: the cross-unit invariants no single work unit could check alone.
//!
//! Every assertion here spans a *seam* between two units:
//!
//! * C-01 (`paths.rs`) resolves the config path; C-02 (`config.rs`) deliberately resolves
//!   its own copy of the same chain so that loading stays testable without a `Paths`. Two
//!   independent implementations of one chain is exactly the shape that drifts, and the
//!   failure is silent and nasty: `Config::save` writes through `paths.config_file()` while
//!   `Config::load` reads through `resolve_config_path()`, so a divergence means the daemon
//!   saves to one file and reads another forever.
//! * Invariant 5 (ARCHITECTURE §0.1) — "nothing is ever written into a repo directory" — is
//!   a property of the *pair*. C-01 hardened `Paths` against relative and empty `$XDG_*`
//!   values (a relative value resolves against the cwd, which under `cargo test` is this
//!   repo); this file proves C-02's separate chain got the same hardening.
//!
//! **Why an integration test and not a unit test.** These assertions have to mutate the
//! process environment, and `cargo test` runs a crate's unit tests as threads in one
//! process — C-03 and C-04 both recorded that they could not test their `Paths`-taking
//! entry points for exactly this reason. An integration test is its own binary, so the
//! environment is ours. Everything lives in **one** `#[test]` so no second thread in this
//! binary can observe a half-applied environment.

use apexrouter_core::{Config, Paths};
use std::path::{Path, PathBuf};

/// Every variable that can steer path resolution in either unit.
const STEERING: &[&str] = &[
    "APEXROUTER_CONFIG",
    "APEXROUTER_HOME",
    "APEXROUTER_LOCALROUTER_DIR",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "HF_TOKEN_PATH",
    "HF_HOME",
    "PROXY_PORT",
    "HOME",
];

/// Serialises the tests in this binary. `libtest` runs `#[test]` functions as threads in
/// one process, so two environment-mutating tests would interleave and read each other's
/// half-applied state — which is the same hazard the module doc describes, one level in.
/// The lock is *inside* [`EnvGuard`] so it cannot be forgotten by a test added later.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds the environment lock and restores every steering variable when the test ends,
/// however it ends.
struct EnvGuard {
    saved: Vec<(String, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn capture() -> EnvGuard {
        // A panicking test poisons the mutex; recovering the inner guard keeps the *next*
        // test's failure honest instead of masking it behind a poison error.
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        EnvGuard {
            saved: STEERING
                .iter()
                .map(|k| ((*k).to_owned(), std::env::var_os(k)))
                .collect(),
            _lock: lock,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }
}

/// Clear every steering variable, then apply `set`. `HOME` is always given a value because
/// `Paths::resolve()` is documented to fail without one.
fn env(set: &[(&str, &Path)]) {
    for k in STEERING {
        std::env::remove_var(k);
    }
    for (k, v) in set {
        std::env::set_var(k, v);
    }
}

/// Write a config file whose `[router] default_alias` names the file, so that whichever
/// file `Config::load()` actually opened is visible in the value it returns.
fn marker_at(path: &Path, marker: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the config file's parent");
    }
    std::fs::write(path, format!("[router]\ndefault_alias = \"{marker}\"\n"))
        .expect("write the marker config");
}

/// Assert that both units agree the config lives at `want`, and that `Config::load()`
/// really opens it.
fn both_units_agree(case: &str, want: &Path, marker: &str) {
    let paths = Paths::resolve().expect("Paths::resolve");
    assert_eq!(
        paths.config_file(),
        want,
        "{case}: C-01 Paths::config_file() disagrees with the expected chain"
    );

    marker_at(want, marker);
    let cfg = Config::load().expect("Config::load");
    assert_eq!(
        cfg.router.default_alias,
        marker,
        "{case}: C-02 Config::load() opened a DIFFERENT file than C-01 Paths::config_file() \
         ({}). save() and load() would target different files.",
        want.display()
    );
}

#[test]
fn the_stage_one_cross_unit_invariants_hold() {
    let _guard = EnvGuard::capture();
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("create the fake home");

    // ---- 1. the four branches of the config chain, from both sides -----------------
    // $APEXROUTER_CONFIG wins outright.
    let explicit = root.join("explicit").join("apex.toml");
    env(&[("HOME", &home), ("APEXROUTER_CONFIG", &explicit)]);
    both_units_agree("APEXROUTER_CONFIG", &explicit, "from-apexrouter-config");

    // $APEXROUTER_HOME/config.toml is next.
    let apex_home = root.join("apexhome");
    env(&[("HOME", &home), ("APEXROUTER_HOME", &apex_home)]);
    both_units_agree(
        "APEXROUTER_HOME",
        &apex_home.join("config.toml"),
        "from-apexrouter-home",
    );

    // then $XDG_CONFIG_HOME/apexrouter/config.toml.
    let xdg = root.join("xdgconfig");
    env(&[("HOME", &home), ("XDG_CONFIG_HOME", &xdg)]);
    both_units_agree(
        "XDG_CONFIG_HOME",
        &xdg.join("apexrouter").join("config.toml"),
        "from-xdg-config-home",
    );

    // and finally ~/.config/apexrouter/config.toml.
    env(&[("HOME", &home)]);
    both_units_agree(
        "HOME only",
        &home.join(".config").join("apexrouter").join("config.toml"),
        "from-home",
    );

    // ---- 2. invariant 5: no relative env value may resolve against the cwd ----------
    // The cwd during `cargo test` is this crate's directory, i.e. inside the repo. A
    // relative $XDG_* must therefore be ignored by BOTH units, per the XDG spec.
    let repo_cwd = std::env::current_dir().expect("cwd");
    env(&[("HOME", &home)]);
    std::env::set_var("XDG_CONFIG_HOME", "relative-config");
    std::env::set_var("XDG_STATE_HOME", "relative-state");
    std::env::set_var("XDG_CACHE_HOME", "relative-cache");
    both_units_agree(
        "relative XDG_CONFIG_HOME is ignored",
        &home.join(".config").join("apexrouter").join("config.toml"),
        "relative-xdg-ignored",
    );
    let paths = Paths::resolve().expect("Paths::resolve with relative XDG");
    assert!(
        paths.state().starts_with(&home) && paths.cache().starts_with(&home),
        "a relative $XDG_*_HOME resolved against the cwd: state={}, cache={}",
        paths.state().display(),
        paths.cache().display()
    );

    // An empty value must count as unset, not as "the cwd".
    env(&[("HOME", &home)]);
    for k in ["APEXROUTER_HOME", "APEXROUTER_CONFIG", "XDG_CONFIG_HOME"] {
        std::env::set_var(k, "");
    }
    both_units_agree(
        "empty env values count as unset",
        &home.join(".config").join("apexrouter").join("config.toml"),
        "empty-is-unset",
    );

    // ---- 3. nothing any accessor returns lands inside the repo ----------------------
    env(&[("HOME", &home)]);
    let paths = Paths::resolve().expect("Paths::resolve");
    let derived: Vec<(&str, PathBuf)> = vec![
        ("config_file", paths.config_file()),
        ("state", paths.state().to_path_buf()),
        ("cache", paths.cache().to_path_buf()),
        ("routes_file", paths.routes_file()),
        ("backends_file", paths.backends_file()),
        ("tunnels_file", paths.tunnels_file()),
        ("catalog_file", paths.catalog_file()),
        ("credentials_file", paths.credentials_file()),
        ("ledger", paths.ledger()),
        ("usage_log", paths.usage_log()),
        ("endpoints_dir", paths.endpoints_dir()),
        ("jobs_dir", paths.jobs_dir()),
        ("approvals_dir", paths.approvals_dir()),
        ("logs_dir", paths.logs_dir()),
        ("ssh_dir", paths.ssh_dir()),
        ("known_hosts", paths.known_hosts()),
        ("daemon_lock", paths.daemon_lock()),
        ("state_lock", paths.state_lock()),
    ];
    for (name, p) in &derived {
        assert!(p.is_absolute(), "{name} is not absolute: {}", p.display());
        assert!(
            !p.starts_with(&repo_cwd),
            "invariant 5 broken: {name} resolves inside the repo: {}",
            p.display()
        );
    }

    // ---- 4. save() writes where load() reads ----------------------------------------
    // The seam that matters operationally: `save` goes through `paths.config_file()`,
    // `load` through C-02's own chain. If those ever split, a saved setting silently
    // vanishes on the next read.
    let save_home = root.join("savehome");
    env(&[("HOME", &home), ("APEXROUTER_HOME", &save_home)]);
    let paths = Paths::resolve().expect("Paths::resolve");
    let mut cfg = Config::load().expect("Config::load");
    cfg.router.default_alias = "saved-then-loaded".to_owned();
    cfg.save(&paths).expect("Config::save");
    assert!(
        save_home.join("config.toml").is_file(),
        "save() did not write to paths.config_file()"
    );
    let reloaded = Config::load().expect("reload after save");
    assert_eq!(
        reloaded.router.default_alias, "saved-then-loaded",
        "a saved setting did not survive a reload — save() and load() target different files"
    );

    // ---- 5. ensure_layout() creates only what is under $STATE and $CACHE -------------
    let layout_home = root.join("layout");
    env(&[("HOME", &home), ("APEXROUTER_HOME", &layout_home)]);
    let paths = Paths::resolve().expect("Paths::resolve");
    paths.ensure_layout().expect("ensure_layout");
    for dir in [
        paths.state().to_path_buf(),
        paths.endpoints_dir(),
        paths.jobs_dir(),
        paths.approvals_dir(),
        paths.logs_dir(),
        paths.ssh_dir(),
    ] {
        assert!(dir.is_dir(), "ensure_layout skipped {}", dir.display());
        assert!(
            !dir.starts_with(&repo_cwd),
            "ensure_layout created a directory inside the repo: {}",
            dir.display()
        );
    }
}

/// The library half of BUILD-PLAN §0's Stage-1 gate line, "`apexrouter config show` on a
/// bare machine".
///
/// The verb itself cannot run yet — `apexrouter-cli`'s `main()` is still the Stage-0 skeleton
/// and the whole clap dispatch belongs to unit S-06 in Stage 4 — so this asserts the thing
/// the verb would be printing: on a machine with **no config file and an empty state
/// directory**, the whole C-01 + C-02 path comes up clean and hands back the documented
/// defaults. A zero-config install is a supported install (ARCHITECTURE §1.3 step 3).
#[test]
fn a_bare_machine_configures_itself() {
    let _guard = EnvGuard::capture();
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let bare = tmp.path().join("bare-state");
    std::fs::create_dir_all(&home).expect("create the fake home");
    env(&[("HOME", &home), ("APEXROUTER_HOME", &bare)]);

    let paths = Paths::resolve().expect("Paths::resolve on a bare machine");
    paths
        .ensure_layout()
        .expect("ensure_layout on a bare machine");
    assert!(
        !paths.config_file().exists(),
        "the bare-machine fixture is not bare: {} exists",
        paths.config_file().display()
    );

    // Absence must not be an error, and must not differ from the shipped example.
    let cfg = Config::load().expect("a missing config file is a working zero-config install");
    assert_eq!(
        cfg,
        Config::default(),
        "a missing config file did not yield the default Config"
    );

    // The two numbers the user actually sees (ARCHITECTURE §1.1 and §5.2).
    assert_eq!(cfg.proxy_bind().to_string(), "127.0.0.1:8888");
    assert_eq!(cfg.control_bind().to_string(), "127.0.0.1:2739");

    // And `config init` then lands a real file at the resolved path, at 0600.
    let written = Config::init_file(&paths, false).expect("Config::init_file");
    assert_eq!(written, paths.config_file());
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&written)
            .expect("stat the initialised config")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode, 0o600, "config.toml was written world-readable");
    assert_eq!(
        Config::load().expect("reload the initialised config"),
        Config::default(),
        "the file `config init` writes does not parse back to the defaults"
    );
}
