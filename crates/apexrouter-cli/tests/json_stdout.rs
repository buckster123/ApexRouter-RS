//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,
//! compare,backend,swap,up,approvals,token,open,env,migrate}.rs, cli/tests/json_stdout.rs).
//! Do not edit outside that unit.
//!
//! House rule 5, asserted by a JSON parser: **`--json` puts the envelope on stdout and
//! nothing else.**
//!
//! # Why this test exists
//!
//! `apexrouter smoke --alias A --json` printed `Self-hosted (tunnel/localhost)  http://…`
//! on line one, unconditionally, *before* the `--json` branch. Every consumer of that
//! stream is `| jq`, and `jq` failed. The bug was one line long, invisible in review, and
//! survived a full acceptance pass because every other `--json` verb happened to be right —
//! which is exactly the shape of defect a per-verb assertion catches and a code review does
//! not.
//!
//! `render.rs` already proves that **only** `render` writes to stdout, by walking the
//! crate's own sources. That is a necessary condition and not a sufficient one: `smoke` went
//! through `render::print_line`, so the source walk was green while the output was garbage.
//! This test asks the other question — not *who* wrote to stdout, but *what came out*.
//!
//! # What it covers, and what it deliberately does not
//!
//! Every `--json` verb that answers **without a daemon**: `Need::Pure` and
//! `Need::ReadState`, served from `$STATE` under a shared lock. The `Need::Mutate` verbs
//! (`provider ls`, `vast …`, `hf …`, `compare`) would need a live daemon and, for the vast
//! ones, an account — they are out of scope here and covered by the server suite.
//!
//! `smoke --base-url` is in scope and is the regression guard proper: it is the one probing
//! verb that needs no daemon at all, which is also why it is the verb an operator reaches
//! for when something is already wrong.
//!
//! Hermetic: a cleared environment, a `TempDir` for `$HOME`/`$APEXROUTER_HOME`, empty model
//! and build roots, `[providers.together]` pointed at a **closed loopback port**, and the
//! only address any probe dials is `127.0.0.1:9`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A disposable machine: a fake `$HOME`, an `$APEXROUTER_HOME` with a config in it, and
/// empty roots so `rig`/`models` scan nothing.
struct World {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    config: PathBuf,
}

impl World {
    fn new() -> World {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let (home, state) = (root.join("home"), root.join("state"));
        let (models, builds) = (root.join("models"), root.join("builds"));
        for d in [&home, &state, &models, &builds] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        let config = state.join("config.toml");
        // Everything is defaulted in code, so this file only says what must not be the
        // default here: no autostart (a daemon would defeat the point), roots that hold
        // nothing, no read of another tool's state directory, and a provider base URL on a
        // closed loopback port — the house hermeticity rule, belt and braces.
        std::fs::write(
            &config,
            format!(
                "[server]\nautostart = false\n\
                 [endpoints]\nmodel_roots = [\"{m}\"]\nbuild_roots = [\"{b}\"]\n\
                 [providers.together]\nbase_url = \"http://127.0.0.1:1/v1\"\n\
                 [compat]\nread_legacy_state = false\n",
                m = models.display(),
                b = builds.display(),
            ),
        )
        .expect("write config");
        World {
            _tmp: tmp,
            home,
            state,
            config,
        }
    }

    /// Run `apexrouter …` with a cleared environment pinned at this world.
    fn run(&self, args: &[&str]) -> Out {
        let out = Command::new(env!("CARGO_BIN_EXE_apexrouter"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("APEXROUTER_HOME", &self.state)
            .env("APEXROUTER_CONFIG", &self.config)
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .args(args)
            .arg("--no-autostart")
            .output()
            .expect("spawn apexrouter");
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Every `--json` verb that answers with no daemon running.
///
/// Spelled as argv rather than derived from clap: a hand-written list is what makes a *new*
/// `--json` verb an omission somebody has to notice, and deriving it from the parser would
/// re-import the assumption under test.
const JSON_VERBS: &[&[&str]] = &[
    &["status", "--json"],
    &["url", "--json"],
    &["version", "--json"],
    &["env", "--json"],
    &["config", "show", "--json"],
    &["config", "path", "--json"],
    &["rig", "--json"],
    &["models", "ls", "--json"],
    &["route", "ls", "--json"],
    &["backend", "ls", "--json"],
    &["endpoint", "ls", "--json"],
    &["recipe", "ls", "--json"],
    &["profile", "ls", "--json"],
    &["tunnel", "status", "--json"],
    &["usage", "--json"],
    // The D4 regression guard. A closed loopback port, so all four probes fail fast —
    // a failing probe is the *answer*, not an error, and it still has to be JSON.
    &["smoke", "--base-url", "http://127.0.0.1:9", "--json"],
];

#[test]
fn every_json_verb_puts_json_and_only_json_on_stdout() {
    let w = World::new();
    let mut failures: Vec<String> = Vec::new();

    for verb in JSON_VERBS {
        let out = w.run(verb);
        let name = verb.join(" ");
        if out.code != 0 {
            failures.push(format!(
                "`{name}` exited {}: {}",
                out.code,
                out.stderr.trim()
            ));
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(&out.stdout) {
            Ok(v) => {
                // The envelope contract (`render::envelope_value`): three keys ride on
                // every `--json` answer so a script can tell where it came from.
                for key in ["served_by", "as_of_unix", "stale"] {
                    if v.get(key).is_none() {
                        failures.push(format!("`{name}` has no `{key}` in its envelope"));
                    }
                }
            }
            Err(e) => failures.push(format!(
                "`{name}` did not emit JSON ({e}); stdout began: {:?}",
                out.stdout.chars().take(80).collect::<String>()
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "house rule 5 broken by {} verb(s):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

#[test]
fn smoke_json_is_parseable_and_carries_the_four_probes() {
    // The defect, stated as an assertion rather than as a rule: `smoke --json | jq` used to
    // fail on line one, because the target banner was printed before the `--json` branch.
    let w = World::new();
    let out = w.run(&["smoke", "--base-url", "http://127.0.0.1:9", "--json"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(
        !out.stdout.starts_with("Self-hosted"),
        "the human banner is back on stdout: {:?}",
        out.stdout.chars().take(60).collect::<String>()
    );

    let v: serde_json::Value = serde_json::from_str(&out.stdout)
        .unwrap_or_else(|e| panic!("not JSON: {e}\n{}", out.stdout));
    let probes = v
        .get("data")
        .and_then(|d| d.as_array())
        .unwrap_or_else(|| panic!("no probe array: {v}"));
    assert_eq!(probes.len(), 4, "four probes, always: {v}");
    // Against a closed port every probe fails, and that is the answer, not an error.
    assert!(
        probes.iter().all(|p| p.get("ok") == Some(&false.into())),
        "a closed port cannot pass a probe: {v}"
    );
}

#[test]
fn the_human_form_still_names_the_target_it_probed() {
    // The fix must not have thrown away the banner — without it, human output is four rows
    // of numbers with nothing saying what was measured.
    let w = World::new();
    let out = w.run(&["smoke", "--base-url", "http://127.0.0.1:9"]);
    assert_eq!(out.code, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("http://127.0.0.1:9"),
        "the target is missing from human output: {}",
        out.stdout
    );
    assert!(out.stdout.contains("PROBE"), "{}", out.stdout);
}

#[test]
fn nothing_was_written_into_the_repository() {
    // House rule: nothing is ever written into a repo directory at runtime. The verbs above
    // ran with `$APEXROUTER_HOME` in a TempDir; this asserts the obvious consequence rather
    // than assuming it.
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let w = World::new();
    assert!(
        !w.state.starts_with(&repo),
        "the test's own state dir is inside the repo: {}",
        w.state.display()
    );
    for verb in JSON_VERBS {
        w.run(verb);
    }
    for stray in [
        "routes.json",
        "backends.json",
        "apexrouterd.lock",
        "state.lock",
    ] {
        assert!(
            !repo.join(stray).exists(),
            "`{stray}` appeared in the repository root"
        );
    }
}
