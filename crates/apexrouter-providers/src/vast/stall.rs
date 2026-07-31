//! OWNER: unit P-04 (providers/src/vast/{rent,boot,stall}.rs). Do not edit outside that
//! unit.
//!
//! Download-stall detection **and recovery** — the inventory marks this
//! "genuinely valuable — keep it", so mk1 keeps both halves.
//!
//! Detection is a 4-second `/proc/net/dev` eth0 RX delta over ssh: `< 1000 bytes` is
//! stalled, `< 50 Mbps` is slow. Recovery kills `launch.sh` + `hf download`, recovers the
//! environment from `/proc/<pid>/environ`, **forces `HOST=127.0.0.1`**, and re-execs with
//! `>>` (append), never `>`.
//!
//! ## Why the sample is one ssh command
//!
//! Both `/proc/net/dev` reads and the `sleep 4` between them ride a single connection, so
//! the window really is four seconds — two round trips would make it "four seconds plus
//! whatever the RTT was", and the `< 1000 bytes` threshold is defined on a four-second
//! window. The counters are parsed **here**, not by a remote `awk`: this crate takes argv
//! vectors, and the less remote shell there is the less there is to get wrong.
//!
//! ## Why recovery never uses a `pkill` pattern that matches its own command line
//!
//! `pkill -f 'bash /app/launch.sh'` sent over ssh matches the login shell running *that very
//! command*, because sshd hands the whole string to `$SHELL -c`. It would kill itself before
//! killing the download. Every pattern here therefore uses the classic bracket idiom —
//! `bash /app/launch[.]sh` — which matches the real process and cannot match the command
//! that carries it, and the kill is issued **by pid** in a separate call from the re-exec.
//!
//! ## Why the environment comes from `/proc`
//!
//! LocalRouter fell back to a hardcoded `MODEL_REPO`/`MODEL_QUANT` when it could not read
//! `launch.sh`'s environ — i.e. it could silently restart a *different model* than the one
//! the operator rented. Here the fallback is `/proc/1/environ`, the container's own
//! environment as vast set it at create time. It is always present, always right, and never
//! invents a model. `HOST` is then overwritten with `127.0.0.1` regardless of what was
//! recovered, because `launch.sh`'s own default is `0.0.0.0` and a vast direct port is
//! plaintext HTTP on a shared public IP (`ARCHITECTURE.md` §9.5).

use apexrouter_core::error::{Error, Result};
use apexrouter_core::exec::{self, Output, SshOpts};
use apexrouter_protocol::{DownloadHealth, StallVerdict};
use std::collections::BTreeMap;
use std::time::Duration;

/// The sample window, seconds. The `< 1000 bytes` threshold is defined on exactly this.
pub const SAMPLE_SECS: u64 = 4;

/// Less than this many bytes over the window and the download is not moving at all.
pub const STALLED_BYTES: u64 = 1_000;

/// Below this rate the download is moving, but slowly enough to be worth flagging.
pub const SLOW_MBPS: f32 = 50.0;

/// Printed between the two `/proc/net/dev` dumps so they can be split apart locally.
const MARKER: &str = "--apexrouter-rx-sample--";

/// The launch script every `vastai-gguf` image runs. Also the `pgrep` target.
const LAUNCH_SH: &str = "/app/launch.sh";

/// Where `launch.sh` logs. Recovery **appends** here; only first boot truncates.
const LAUNCH_LOG: &str = "/var/log/launch.log";

/// `pgrep -f` pattern for the launch script, in the bracket idiom so it cannot match the
/// remote login shell that is carrying it.
const LAUNCH_PATTERN: &str = "bash /app/launch[.]sh";

/// `pgrep -f` pattern for the HuggingFace downloader, same idiom.
const HF_PATTERN: &str = "hf downloa[d]";

/// Environment keys worth recovering, matched as substrings of the variable name exactly as
/// LocalRouter's `grep -E` did, so `N_CTX` and `THINKING_MODE` come along too.
const KEEP_ENV: &[&str] = &[
    "MODEL_", "CTX", "KV_TYPE", "MODE", "PARALLEL", "MMPROJ", "HF_TOKEN", "HOST",
];

/// Names whose *values* must never reach a log line or an event.
const SECRET_ENV: &[&str] = &["TOKEN", "KEY", "SECRET", "PASSWORD"];

/// Sample the instance's inbound traffic over 4 seconds.
///
/// One ssh round trip: read `/proc/net/dev`, sleep four seconds, read it again, and take the
/// `eth0` RX delta. `< 1000 bytes` is [`StallVerdict::Stalled`], `< 50 Mbps` is
/// [`StallVerdict::Slow`], anything else is [`StallVerdict::Active`].
///
/// A counter that went *backwards* (an interface reset) yields a delta of zero and therefore
/// a `Stalled` verdict — the conservative direction, since the only thing a `Stalled` verdict
/// does is offer the operator a restart button.
pub async fn sample_download(ssh: &SshOpts, host: &str, port: u16) -> Result<DownloadHealth> {
    let out = exec::ssh(
        host,
        port,
        ssh,
        &sample_argv(),
        Duration::from_secs(SAMPLE_SECS + 15),
    )
    .await?;
    let stdout = ok_stdout(&out, "sampling /proc/net/dev")?;

    let (before, after) = stdout
        .split_once(MARKER)
        .ok_or_else(|| stall_error("the sample returned only one /proc/net/dev snapshot"))?;
    let first = rx_bytes(before)
        .ok_or_else(|| stall_error("no usable interface in the first /proc/net/dev snapshot"))?;
    let second = rx_bytes(after)
        .ok_or_else(|| stall_error("no usable interface in the second /proc/net/dev snapshot"))?;

    Ok(health(second.saturating_sub(first)))
}

/// The one-click recovery behind the stall banner.
///
/// Kills `launch.sh` and any `hf download` **by pid**, waits two seconds, and re-execs
/// `launch.sh` with the environment recovered from `/proc/<pid>/environ` (falling back to
/// `/proc/1/environ`, the container's own environment) with `HOST` forced to `127.0.0.1`.
/// Output is **appended** to `/var/log/launch.log`: truncating it would throw away the log of
/// the very failure being recovered from, and `hf` resumes from the `.incomplete` file, so
/// the restart continues the download rather than starting it over.
pub async fn restart_download(ssh: &SshOpts, host: &str, port: u16) -> Result<()> {
    let launch_pids = pids_of(ssh, host, port, LAUNCH_PATTERN).await?;
    let hf_pids = pids_of(ssh, host, port, HF_PATTERN).await?;

    let env = container_env_from(ssh, host, port, launch_pids.first().copied()).await?;
    let env = hardened(env);
    tracing::info!(
        host,
        port,
        launch_pids = ?launch_pids,
        hf_pids = ?hf_pids,
        recovered = %redacted(&env),
        "restarting a stalled download"
    );

    // Kill by pid, in its own call: a pattern-based kill in the same command line as the
    // re-exec is how you kill the shell that is about to do the re-exec.
    let mut pids: Vec<u32> = launch_pids.into_iter().chain(hf_pids).collect();
    pids.sort_unstable();
    pids.dedup();
    if !pids.is_empty() {
        let argv = kill_argv(&pids);
        let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
        let out = exec::ssh(host, port, ssh, &borrowed, Duration::from_secs(30)).await?;
        // A process that exited between the pgrep and the kill is not an error.
        if out.timed_out {
            return Err(Error::Timeout { ms: 30_000 });
        }
    }

    let argv = relaunch_argv(&env);
    let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
    let out = exec::ssh(host, port, ssh, &borrowed, Duration::from_secs(45)).await?;
    ok_stdout(&out, "re-execing launch.sh")?;
    Ok(())
}

/// The container's environment, as `launch.sh` sees it.
///
/// Reads `/proc/<launch-pid>/environ` when `launch.sh` is running, and `/proc/1/environ`
/// otherwise — the container's own environment, which vast set from
/// `ContainerLaunch::env` at create time. **Never** a hardcoded model.
pub async fn container_env(
    ssh: &SshOpts,
    host: &str,
    port: u16,
) -> Result<BTreeMap<String, String>> {
    let pid = pids_of(ssh, host, port, LAUNCH_PATTERN)
        .await?
        .first()
        .copied();
    container_env_from(ssh, host, port, pid).await
}

/// [`container_env`] with the pid already resolved, so recovery does not `pgrep` twice.
async fn container_env_from(
    ssh: &SshOpts,
    host: &str,
    port: u16,
    pid: Option<u32>,
) -> Result<BTreeMap<String, String>> {
    // pid 1 is the container itself: always there, and holding exactly the variables the
    // create call set.
    let path = format!("/proc/{}/environ", pid.unwrap_or(1));
    let out = exec::ssh(host, port, ssh, &["cat", &path], Duration::from_secs(12)).await?;
    if out.timed_out {
        return Err(Error::Timeout { ms: 12_000 });
    }
    if out.status != 0 {
        // An unreadable environ is not fatal on its own: the caller still forces HOST and
        // `launch.sh` still has its own defaults.
        tracing::warn!(
            path,
            status = out.status,
            "could not read the container environ"
        );
        return Ok(BTreeMap::new());
    }
    Ok(parse_environ(&out.stdout))
}

/// `pgrep -f <pattern>` → pids. No match (`rc 1`) is an empty list, not an error.
async fn pids_of(ssh: &SshOpts, host: &str, port: u16, pattern: &str) -> Result<Vec<u32>> {
    let quoted = shell_quote(pattern);
    let out = exec::ssh(
        host,
        port,
        ssh,
        &["pgrep", "-f", &quoted],
        Duration::from_secs(12),
    )
    .await?;
    if out.timed_out {
        return Err(Error::Timeout { ms: 12_000 });
    }
    Ok(parse_pids(&out.stdout))
}

/// The remote argv for one 4-second RX sample.
///
/// Joined by `exec::ssh` into `cat /proc/net/dev ; echo <marker> ; sleep 4 ; cat
/// /proc/net/dev` — one connection, one genuine four-second window, and no remote `awk`.
fn sample_argv() -> Vec<&'static str> {
    vec![
        "cat",
        "/proc/net/dev",
        ";",
        "echo",
        MARKER,
        ";",
        "sleep",
        "4",
        ";",
        "cat",
        "/proc/net/dev",
    ]
}

/// `kill <pids> ; sleep 2` — SIGTERM, then the two seconds LocalRouter waited before the
/// re-exec so the killed process has released its `.incomplete` file.
fn kill_argv(pids: &[u32]) -> Vec<String> {
    let mut argv: Vec<String> = vec!["kill".to_owned()];
    argv.extend(pids.iter().map(u32::to_string));
    argv.push(";".to_owned());
    argv.push("sleep".to_owned());
    argv.push("2".to_owned());
    argv
}

/// The re-exec: `nohup env K=V … bash /app/launch.sh >> /var/log/launch.log 2>&1 < /dev/null &`.
///
/// `>>` and never `>`. `env` rather than a shell assignment prefix so the argv is explicit,
/// `nohup` and `< /dev/null` so closing the ssh channel does not take the download with it.
fn relaunch_argv(env: &BTreeMap<String, String>) -> Vec<String> {
    let mut argv: Vec<String> = vec!["nohup".to_owned(), "env".to_owned()];
    for (k, v) in env {
        argv.push(shell_quote(&format!("{k}={v}")));
    }
    argv.push("bash".to_owned());
    argv.push(LAUNCH_SH.to_owned());
    argv.push(">>".to_owned());
    argv.push(LAUNCH_LOG.to_owned());
    argv.push("2>&1".to_owned());
    argv.push("<".to_owned());
    argv.push("/dev/null".to_owned());
    argv.push("&".to_owned());
    argv
}

/// Force the tunnel-only posture on every restart, whatever was recovered.
fn hardened(mut env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.insert("HOST".to_owned(), "127.0.0.1".to_owned());
    env
}

/// Turn a recovered RX delta into the verdict the UI banners on.
fn health(rx_bytes_4s: u64) -> DownloadHealth {
    let bits = (rx_bytes_4s as f64) * 8.0;
    let mbps = (bits / (SAMPLE_SECS as f64 * 1_000_000.0)) as f32;
    let verdict = if rx_bytes_4s < STALLED_BYTES {
        StallVerdict::Stalled
    } else if mbps < SLOW_MBPS {
        StallVerdict::Slow
    } else {
        StallVerdict::Active
    };
    DownloadHealth {
        sampled_at_unix: chrono::Utc::now().timestamp(),
        rx_bytes_4s,
        mbps,
        verdict,
    }
}

/// RX bytes for the interface a download would arrive on, from one `/proc/net/dev` dump.
///
/// `eth0` wins when present, then any other `eth*`; otherwise the busiest non-loopback
/// interface, because vast hosts do not all name the container's NIC `eth0`.
fn rx_bytes(dump: &str) -> Option<u64> {
    let mut best: Option<(String, u64)> = None;
    for line in dump.lines() {
        let (name, rest) = match line.split_once(':') {
            Some(pair) => pair,
            None => continue,
        };
        let name = name.trim();
        if name.is_empty() || name == "lo" || name.contains(char::is_whitespace) {
            continue;
        }
        let rx: u64 = match rest.split_whitespace().next().and_then(|f| f.parse().ok()) {
            Some(rx) => rx,
            None => continue,
        };
        if name == "eth0" {
            return Some(rx);
        }
        let better = match &best {
            None => true,
            Some((best_name, best_rx)) => {
                (name.starts_with("eth") && !best_name.starts_with("eth")) || rx > *best_rx
            }
        };
        if better {
            best = Some((name.to_owned(), rx));
        }
    }
    best.map(|(_, rx)| rx)
}

/// Parse a NUL-separated `/proc/<pid>/environ` into the variables worth carrying forward.
fn parse_environ(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for entry in raw.split('\0') {
        let entry = entry.trim_matches(|c: char| c == '\n' || c == '\r');
        if entry.is_empty() {
            continue;
        }
        if let Some((k, v)) = entry.split_once('=') {
            if !k.is_empty() && KEEP_ENV.iter().any(|keep| k.contains(keep)) {
                out.insert(k.to_owned(), v.to_owned());
            }
        }
    }
    out
}

/// `pgrep` output → pids, in the order it reported them.
fn parse_pids(raw: &str) -> Vec<u32> {
    raw.lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

/// Quote one word for the **remote** shell. ssh always hands its command to a login shell;
/// this is what stops a space or a `;` in a value becoming a second command.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// True when this variable's value must never be logged.
fn is_secret(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SECRET_ENV.iter().any(|s| upper.contains(s))
}

/// The recovered environment, rendered for a log line. `HF_TOKEN` is the whole reason this
/// exists: it is genuinely needed on the box and must never appear in our own logs.
fn redacted(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(k, v)| {
            if is_secret(k) {
                format!("{k}=<redacted>")
            } else {
                format!("{k}={v}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// stdout of a remote command that has to have worked.
fn ok_stdout<'a>(out: &'a Output, what: &str) -> Result<&'a str> {
    if out.timed_out {
        return Err(Error::Other(format!("{what}: the ssh call timed out")));
    }
    if out.status != 0 {
        return Err(Error::Other(format!(
            "{what}: ssh exited {} ({})",
            out.status,
            out.stderr.trim()
        )));
    }
    Ok(&out.stdout)
}

/// A stall-sampling failure that names what could not be read.
fn stall_error(why: &str) -> Error {
    Error::Invalid {
        what: "download sample".to_owned(),
        why: why.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str =
        "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets
    lo:  123456     900    0    0    0     0          0         0   123456     900
  eth0: 4000000000  90210    0    0    0     0          0         0  1234567    4242
";

    #[test]
    fn the_sample_is_one_connection_with_a_real_four_second_window() {
        let argv = sample_argv();
        assert_eq!(argv.first(), Some(&"cat"));
        assert!(argv.contains(&"/proc/net/dev"));
        assert!(argv.contains(&"sleep"));
        assert!(argv.contains(&"4"), "the window is exactly four seconds");
        assert_eq!(
            argv.iter().filter(|a| **a == "/proc/net/dev").count(),
            2,
            "two snapshots, one connection"
        );
        // No remote awk, and nothing that needs quoting.
        assert!(!argv.iter().any(|a| a.contains("awk")));
    }

    #[test]
    fn eth0_wins_and_loopback_is_never_counted() {
        assert_eq!(rx_bytes(DUMP), Some(4_000_000_000));
        // No eth0: the busiest non-loopback interface stands in.
        let no_eth = "    lo: 999999 1 0\n  ens3: 4242 1 0\n  docker0: 17 1 0\n";
        assert_eq!(rx_bytes(no_eth), Some(4_242));
        // Only loopback: nothing usable.
        assert_eq!(rx_bytes("    lo: 999999 1 0\n"), None);
        assert_eq!(rx_bytes(""), None);
    }

    #[test]
    fn the_thresholds_are_the_documented_ones() {
        // < 1000 bytes in 4 s is stalled.
        assert_eq!(health(0).verdict, StallVerdict::Stalled);
        assert_eq!(health(999).verdict, StallVerdict::Stalled);
        // 1000 bytes is not stalled, but it is not fast either.
        assert_eq!(health(1_000).verdict, StallVerdict::Slow);
        // 50 Mbps over 4 s = 25_000_000 bytes exactly.
        assert_eq!(health(24_999_999).verdict, StallVerdict::Slow);
        assert_eq!(health(25_000_000).verdict, StallVerdict::Active);

        let h = health(25_000_000);
        assert!((h.mbps - 50.0).abs() < 0.01, "{}", h.mbps);
        assert_eq!(h.rx_bytes_4s, 25_000_000);
        assert!(h.sampled_at_unix > 1_700_000_000);
    }

    #[test]
    fn a_counter_that_went_backwards_is_read_as_stalled_not_as_a_negative_rate() {
        // saturating_sub is what sample_download applies; assert the arithmetic it relies on.
        let delta = 5u64.saturating_sub(9);
        assert_eq!(delta, 0);
        assert_eq!(health(delta).verdict, StallVerdict::Stalled);
    }

    #[test]
    fn the_environ_reader_keeps_the_launch_contract_and_drops_the_noise() {
        let raw = "MODEL_REPO=unsloth/Qwen3-8B-GGUF\0MODEL_QUANT=UD-Q5_K_XL\0CTX=131072\0\
                   KV_TYPE=q8_0\0MODE=thinking\0PARALLEL=1\0HOST=0.0.0.0\0PATH=/usr/bin\0\
                   HF_TOKEN=hf_secret\0PWD=/root\0";
        let env = parse_environ(raw);
        assert_eq!(
            env.get("MODEL_REPO").map(String::as_str),
            Some("unsloth/Qwen3-8B-GGUF")
        );
        assert_eq!(env.get("CTX").map(String::as_str), Some("131072"));
        assert_eq!(env.get("HF_TOKEN").map(String::as_str), Some("hf_secret"));
        assert!(!env.contains_key("PATH"), "{env:?}");
        assert!(!env.contains_key("PWD"), "{env:?}");
        // A value containing '=' survives intact.
        assert_eq!(
            parse_environ("MODEL_ARGS=a=b\0")
                .get("MODEL_ARGS")
                .map(String::as_str),
            Some("a=b")
        );
    }

    #[test]
    fn host_is_forced_to_loopback_whatever_the_box_was_running() {
        let mut env = BTreeMap::new();
        env.insert("HOST".to_owned(), "0.0.0.0".to_owned());
        assert_eq!(
            hardened(env).get("HOST").map(String::as_str),
            Some("127.0.0.1")
        );
        // …and it is set even when nothing was recovered at all.
        assert_eq!(
            hardened(BTreeMap::new()).get("HOST").map(String::as_str),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn the_relaunch_appends_and_never_truncates() {
        let env = hardened(parse_environ("MODEL_REPO=a/b\0CTX=4096\0HOST=0.0.0.0\0"));
        let argv = relaunch_argv(&env);
        let line = argv.join(" ");

        assert!(line.contains(">> /var/log/launch.log"), "{line}");
        assert!(
            !line.contains(" > "),
            "a single `>` would truncate the log of the failure being recovered: {line}"
        );
        assert!(
            argv.iter().all(|a| a != ">"),
            "no argv word may be a truncating redirect: {argv:?}"
        );
        assert!(line.contains("'HOST=127.0.0.1'"), "{line}");
        assert!(!line.contains("'HOST=0.0.0.0'"), "{line}");
        assert!(line.contains("bash /app/launch.sh"), "{line}");
        assert!(line.starts_with("nohup env "), "{line}");
        assert!(line.ends_with('&'), "{line}");
        assert!(line.contains("< /dev/null"), "{line}");
    }

    #[test]
    fn a_value_with_spaces_or_metacharacters_cannot_become_a_second_command() {
        let mut env = BTreeMap::new();
        env.insert(
            "MODEL_ARGS".to_owned(),
            "--flag a b ; rm -rf / ; echo".to_owned(),
        );
        let line = relaunch_argv(&hardened(env)).join(" ");
        assert!(
            line.contains("'MODEL_ARGS=--flag a b ; rm -rf / ; echo'"),
            "{line}"
        );
        // Exactly one un-quoted `;` may never appear: the value is one shell word.
        assert_eq!(
            line.matches("; rm -rf /").count(),
            1,
            "and it is inside the quotes: {line}"
        );

        // A single quote in the value is escaped rather than closing the quoting.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn no_kill_pattern_can_match_the_command_that_carries_it() {
        // This is the whole reason for the bracket idiom: sshd runs our command through a
        // login shell, so the shell's own cmdline contains every pattern we send.
        let carrier = format!("pgrep -f {} ; pgrep -f {}", LAUNCH_PATTERN, HF_PATTERN);
        assert!(
            !carrier.contains("bash /app/launch.sh"),
            "the carrier would match its own pgrep: {carrier}"
        );
        assert!(
            !carrier.contains("hf download"),
            "the carrier would match its own pgrep: {carrier}"
        );
        // …and the patterns really do describe the real processes.
        assert_eq!(LAUNCH_PATTERN.replace("[.]", "."), "bash /app/launch.sh");
        assert_eq!(HF_PATTERN.replace("[d]", "d"), "hf download");
    }

    #[test]
    fn the_kill_is_by_pid_and_waits_before_the_re_exec() {
        let argv = kill_argv(&[1234, 5678]);
        assert_eq!(
            argv,
            vec!["kill", "1234", "5678", ";", "sleep", "2"]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        );
        // Never a pattern: `pkill -f` in the same breath as the re-exec kills the re-exec.
        assert!(!argv.iter().any(|a| a.contains("pkill")));
    }

    #[test]
    fn pgrep_output_parses_and_an_empty_result_is_not_an_error() {
        assert_eq!(parse_pids("123\n456\n"), vec![123, 456]);
        assert_eq!(parse_pids(""), Vec::<u32>::new());
        assert_eq!(parse_pids("\n  \n"), Vec::<u32>::new());
    }

    #[test]
    fn the_recovered_token_reaches_the_box_but_never_the_log() {
        assert!(is_secret("HF_TOKEN"));
        assert!(is_secret("OPENAI_API_KEY"));
        assert!(is_secret("some_secret"));
        assert!(!is_secret("MODEL_REPO"));
        assert!(!is_secret("CTX"));

        let env = hardened(parse_environ("MODEL_REPO=a/b\0HF_TOKEN=hf_secret_value\0"));
        let log = redacted(&env);
        assert!(log.contains("MODEL_REPO=a/b"), "{log}");
        assert!(log.contains("HF_TOKEN=<redacted>"), "{log}");
        assert!(!log.contains("hf_secret_value"), "{log}");

        // It does have to reach the container, or a gated repo cannot resume.
        let line = relaunch_argv(&env).join(" ");
        assert!(line.contains("'HF_TOKEN=hf_secret_value'"), "{line}");
    }

    #[test]
    fn a_failed_remote_command_names_what_it_was_doing() {
        let out = Output {
            status: 255,
            stdout: String::new(),
            stderr: "ssh: connect to host port 22: Connection refused".to_owned(),
            timed_out: false,
        };
        let e = ok_stdout(&out, "sampling /proc/net/dev").expect_err("rc 255");
        let msg = e.to_string();
        assert!(msg.contains("sampling /proc/net/dev"), "{msg}");
        assert!(msg.contains("Connection refused"), "{msg}");

        let out = Output {
            timed_out: true,
            ..Default::default()
        };
        assert!(ok_stdout(&out, "x")
            .expect_err("timeout")
            .to_string()
            .contains("timed out"));
    }
}
