//! What the fake `llama-server` was launched with, written to disk and read back.
//!
//! This is the artefact that proves the fit plan reached argv. The fake writes one
//! [`LaunchRecord`] per launch; [`Records`] reads them back, keyed by the port the
//! supervisor happened to allocate.
//!
//! **Environment values are redacted by default.** A record captures the *inherited*
//! environment as well as the two variables the argv builder adds, and a developer's shell
//! has `TOGETHER_API_KEY` in it. Names matching a credential pattern are replaced with
//! `<redacted>` and listed in [`LaunchRecord::env_redacted`]; set
//! `APEX_FAKE_LLAMA_RECORD_ENV=all` when a test genuinely needs the values.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The file every launch appends one JSON line to, inside a records directory.
pub const LAUNCHES_JSONL: &str = "launches.jsonl";

/// Substrings that make an environment variable's *value* a secret.
const SECRET_MARKERS: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PRIVATE",
];

/// Prefixes that are never redacted however they read: these are the variables tests
/// actually assert on, and `GGML_VK_VISIBLE_DEVICES` should not vanish because somebody
/// widened the secret list.
const NEVER_REDACT: &[&str] = &[
    "APEX", "CARGO", "CUDA_", "GGML_", "HIP_", "HOME", "LD_", "PATH", "PWD", "RUST", "TMPDIR",
    "VK_",
];

/// One launch of the fake `llama-server`, exactly as the kernel received it.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LaunchRecord {
    /// `argv[0]` — the path the supervisor actually exec'd.
    pub argv0: String,
    /// Every argument after `argv[0]`, in order, unmodified.
    pub argv: Vec<String>,
    /// The environment as the child saw it, secrets redacted.
    pub env: BTreeMap<String, String>,
    /// Names whose values were replaced with `<redacted>`.
    pub env_redacted: Vec<String>,
    /// The working directory the child was given.
    pub cwd: String,
    /// The child's pid.
    pub pid: u32,
    /// Its parent — the process that spawned it.
    pub ppid: u32,
    /// When it started.
    pub started_at_unix: i64,
    /// `--port`, parsed.
    pub port: Option<u16>,
    /// `--host`, parsed.
    pub host: Option<String>,
    /// `-m`, parsed.
    pub model: Option<String>,
    /// `-a`, parsed.
    pub alias: Option<String>,
    /// Every `-flag value` pair, so a test can ask for `-c` instead of counting indices.
    /// A flag that appears twice keeps the **last** value, matching how llama.cpp parses.
    pub flags: BTreeMap<String, String>,
    /// Every flag that carried no value: `--props`, `--metrics`, `--slots`, `--no-jinja`.
    pub switches: Vec<String>,
}

impl LaunchRecord {
    /// Capture the current process.
    pub fn from_process() -> LaunchRecord {
        let all: Vec<String> = std::env::args().collect();
        let argv0 = all.first().cloned().unwrap_or_default();
        let argv: Vec<String> = all.into_iter().skip(1).collect();
        let redact = !matches!(
            std::env::var("APEX_FAKE_LLAMA_RECORD_ENV").as_deref(),
            Ok("all")
        );

        let mut env = BTreeMap::new();
        let mut env_redacted = Vec::new();
        for (k, v) in std::env::vars() {
            if redact && is_secret_name(&k) {
                env_redacted.push(k.clone());
                env.insert(k, "<redacted>".to_owned());
            } else {
                env.insert(k, v);
            }
        }

        let (flags, switches) = split_flags(&argv);
        LaunchRecord {
            port: flags.get("--port").and_then(|p| p.parse().ok()),
            host: flags.get("--host").cloned(),
            model: flags.get("-m").or_else(|| flags.get("--model")).cloned(),
            alias: flags.get("-a").or_else(|| flags.get("--alias")).cloned(),
            argv0,
            argv,
            env,
            env_redacted,
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            pid: std::process::id(),
            ppid: parent_pid(),
            started_at_unix: now_unix(),
            flags,
            switches,
        }
    }

    /// A record for a server that was never exec'd — the in-process stub. Only `argv` and
    /// the fields derived from it are populated.
    pub fn synthetic(args: &[&str]) -> LaunchRecord {
        let argv: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let (flags, switches) = split_flags(&argv);
        LaunchRecord {
            argv0: "in-process-stub".to_owned(),
            port: flags.get("--port").and_then(|p| p.parse().ok()),
            host: flags.get("--host").cloned(),
            model: flags.get("-m").or_else(|| flags.get("--model")).cloned(),
            alias: flags.get("-a").or_else(|| flags.get("--alias")).cloned(),
            argv,
            pid: std::process::id(),
            started_at_unix: now_unix(),
            flags,
            switches,
            ..LaunchRecord::default()
        }
    }

    /// The value of a `-flag value` pair, e.g. `record.flag("-c")` -> `Some("32768")`.
    pub fn flag(&self, name: &str) -> Option<&str> {
        self.flags.get(name).map(String::as_str)
    }

    /// The value of a pair, parsed. `record.flag_as::<u32>("-c")`.
    pub fn flag_as<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.flag(name).and_then(|v| v.parse().ok())
    }

    /// Whether a bare switch was passed: `record.has("--props")`.
    pub fn has(&self, flag: &str) -> bool {
        self.switches.iter().any(|s| s == flag) || self.flags.contains_key(flag)
    }

    /// An environment variable as the child saw it.
    pub fn env_var(&self, name: &str) -> Option<&str> {
        self.env.get(name).map(String::as_str)
    }

    /// Whether this exact token appears anywhere in argv — the blunt assertion, for when
    /// the shape of the pair is what is under test.
    pub fn argv_contains(&self, token: &str) -> bool {
        self.argv.iter().any(|a| a == token)
    }

    /// argv rendered for an assertion message. **Not** a shell command: this is display
    /// only, and the whole codebase spawns argv vectors.
    pub fn argv_line(&self) -> String {
        let mut out = String::from(&self.argv0);
        for a in &self.argv {
            out.push(' ');
            out.push_str(a);
        }
        out
    }

    /// Write this record to `dest`.
    ///
    /// A directory (or a path that does not exist but has no `.json` suffix) gets
    /// `port-<port>.json` plus one appended line in [`LAUNCHES_JSONL`]; any other path is
    /// written as a single file. Failure is reported on stderr and never fatal — a fake
    /// server that refuses to start because it could not write a test artefact is worse
    /// than one that starts.
    pub fn write_to(&self, dest: &Path) {
        let as_dir = dest.is_dir() || dest.extension().is_none();
        let Ok(line) = serde_json::to_string(self) else {
            eprintln!("apex-fake: could not serialise the launch record");
            return;
        };
        if !as_dir {
            report(dest, fs::write(dest, format!("{line}\n")));
            return;
        }
        if let Err(e) = fs::create_dir_all(dest) {
            report(dest, Err(e));
            return;
        }
        let name = match self.port {
            Some(p) => format!("port-{p}.json"),
            None => format!("pid-{}.json", self.pid),
        };
        report(dest, fs::write(dest.join(name), format!("{line}\n")));

        // One append per launch, so `Records::all()` keeps launch order even when two
        // endpoints were given the same port by two different tempdirs.
        let jsonl = dest.join(LAUNCHES_JSONL);
        let appended = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&jsonl)
            .and_then(|mut f| f.write_all(format!("{line}\n").as_bytes()));
        report(&jsonl, appended);
    }
}

/// A directory of [`LaunchRecord`]s.
///
/// Reading is always fresh: the fake writes a record at exec time, which is *before* the
/// health gate passes, so a test that reads after `up()` returns is guaranteed to see it.
#[derive(Clone, Debug)]
pub struct Records {
    dir: PathBuf,
}

impl Records {
    /// Read records out of `dir`. The directory is created if it does not exist, so the
    /// fake's default "write only if the directory is there" rule is satisfied.
    pub fn at(dir: impl Into<PathBuf>) -> Records {
        let dir = dir.into();
        let _ = fs::create_dir_all(&dir);
        Records { dir }
    }

    /// The directory records land in. Pass it as `APEX_FAKE_LLAMA_RECORD`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every launch, oldest first.
    pub fn all(&self) -> Vec<LaunchRecord> {
        let Ok(raw) = fs::read_to_string(self.dir.join(LAUNCHES_JSONL)) else {
            return Vec::new();
        };
        raw.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// The most recent launch.
    pub fn latest(&self) -> Option<LaunchRecord> {
        self.all().pop()
    }

    /// The launch that took `port` — the lookup a supervisor test wants, because the port
    /// is the one thing it knows about a child the allocator placed.
    pub fn for_port(&self, port: u16) -> Option<LaunchRecord> {
        let raw = fs::read_to_string(self.dir.join(format!("port-{port}.json"))).ok()?;
        serde_json::from_str(raw.trim()).ok()
    }

    /// Wait for the record of a launch on `port`.
    ///
    /// Only needed when the launch is racing the assertion; after `Provisioner::up`
    /// returns, [`Records::for_port`] is already populated.
    pub fn wait_for_port(&self, port: u16, within: Duration) -> Option<LaunchRecord> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(r) = self.for_port(port) {
                return Some(r);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Forget every recorded launch.
    pub fn clear(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }
}

// -------------------------------------------------------------------------------------
// internals
// -------------------------------------------------------------------------------------

/// Split argv into `-flag value` pairs and bare switches.
///
/// A token starting with `-` takes the next token as its value unless that token is itself
/// a flag. `-ngl -1` is a pair, not two switches: a value that parses as a number is a
/// value however it starts.
fn split_flags(argv: &[String]) -> (BTreeMap<String, String>, Vec<String>) {
    let mut flags = BTreeMap::new();
    let mut switches = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let token = &argv[i];
        if !token.starts_with('-') {
            i += 1;
            continue;
        }
        let next = argv.get(i + 1);
        let takes_value = match next {
            None => false,
            Some(v) => !v.starts_with('-') || v.parse::<f64>().is_ok(),
        };
        if takes_value {
            if let Some(v) = next {
                flags.insert(token.clone(), v.clone());
            }
            i += 2;
        } else {
            switches.push(token.clone());
            i += 1;
        }
    }
    (flags, switches)
}

/// True when this variable's value should not be written to a file in `/tmp`.
fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if NEVER_REDACT.iter().any(|p| upper.starts_with(p)) {
        return false;
    }
    SECRET_MARKERS.iter().any(|m| upper.contains(m))
}

/// Field 4 of `/proc/self/stat`, read after the last `)` because `comm` contains parens.
fn parent_pid() -> u32 {
    let Ok(raw) = fs::read_to_string("/proc/self/stat") else {
        return 0;
    };
    let Some(after) = raw.rfind(')').map(|i| &raw[i + 1..]) else {
        return 0;
    };
    // After `)` the fields are: state, ppid, ...
    after
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Seconds since the epoch.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Say what went wrong on stderr, which is the child's log, and carry on.
fn report(path: &Path, r: std::io::Result<()>) {
    if let Err(e) = r {
        eprintln!("apex-fake: could not write {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_and_switches_are_separated_and_a_negative_number_is_a_value() {
        let argv: Vec<String> = [
            "-m",
            "/models/x.gguf",
            "--port",
            "8100",
            "-ngl",
            "-1",
            "--props",
            "--slots",
            "-c",
            "32768",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let (flags, switches) = split_flags(&argv);
        assert_eq!(flags.get("-m").map(String::as_str), Some("/models/x.gguf"));
        assert_eq!(flags.get("-ngl").map(String::as_str), Some("-1"));
        assert_eq!(flags.get("-c").map(String::as_str), Some("32768"));
        assert_eq!(switches, vec!["--props".to_owned(), "--slots".to_owned()]);
    }

    #[test]
    fn a_credential_shaped_variable_is_redacted_and_a_device_mask_is_not() {
        assert!(is_secret_name("TOGETHER_API_KEY"));
        assert!(is_secret_name("HF_TOKEN"));
        assert!(is_secret_name("vast_api_key"));
        assert!(!is_secret_name("GGML_VK_VISIBLE_DEVICES"));
        assert!(!is_secret_name("LD_LIBRARY_PATH"));
        assert!(!is_secret_name("PATH"));
        assert!(!is_secret_name("APEX_FAKE_LLAMA_RECORD"));
    }

    #[test]
    fn a_record_round_trips_through_a_directory_and_is_found_by_port() {
        let dir = tempfile::tempdir().expect("tempdir");
        let records = Records::at(dir.path());
        let mut rec = LaunchRecord::from_process();
        rec.port = Some(8123);
        rec.flags.insert("-c".to_owned(), "4096".to_owned());
        rec.write_to(records.dir());

        let back = records.for_port(8123).expect("record by port");
        assert_eq!(back.flag("-c"), Some("4096"));
        assert_eq!(back.flag_as::<u32>("-c"), Some(4096));
        assert_eq!(records.all().len(), 1);
        assert!(records.latest().is_some());
        assert!(records.for_port(9999).is_none());
    }
}
