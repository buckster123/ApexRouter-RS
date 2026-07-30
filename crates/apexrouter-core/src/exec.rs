//! OWNER: unit C-07 (core/exec.rs). Do not edit outside that unit.
//!
//! Running an external command. **argv vectors only.**
//!
//! There is deliberately no variant that takes a shell string, and no API that merges
//! stderr into stdout. A timeout is a required parameter, and a timeout is reported as
//! [`Output::timed_out`] rather than as an exit code somebody has to remember.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::{ChildStderr, ChildStdout, Command};

/// How long we still wait for the output pipes once the child is gone.
///
/// A child can leave a grandchild holding the write end of the pipe — `ssh` with
/// `ControlPersist` is the canonical case — and that would otherwise turn a bounded call
/// into an unbounded read. When the grace expires the read is abandoned and the
/// corresponding stream comes back empty; the exit status and `timed_out` are still true.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// `ControlPersist` for a reused master, matching the tunnel supervisor's setting.
const CONTROL_PERSIST: &str = "ControlPersist=5m";

/// What a command produced. stdout and stderr are **separate in every path**.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// Exit status, or `-1` when the process was signalled.
    pub status: i32,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// True when the deadline expired. Never surfaces as `rc 124`.
    pub timed_out: bool,
}

/// Run a program with an argv vector and a required timeout.
///
/// `program` is spawned directly — no shell, no word splitting, no globbing — so an
/// argument containing spaces, quotes or `;` is one argument and nothing else. stdin is
/// `/dev/null`, so a child that asks a question gets EOF instead of the operator's
/// terminal.
///
/// A child that outlives `timeout` is killed and the call returns `timed_out: true` with
/// whatever it had already written; it is not an `Err`, because "it ran and was too slow"
/// is an answer, not a failure to run. Only failing to *start* the program is an `Err`.
pub async fn run(program: &Path, args: &[&str], timeout: Duration) -> Result<Output> {
    run_env(program, args, &[], timeout).await
}

/// As [`run`], with environment additions.
///
/// `env` is *added to* the inherited environment, entry by entry; nothing is cleared. Never
/// pass key material here for a program that would then re-export it — see
/// `ARCHITECTURE.md` §9.2 for where secrets are allowed to travel.
pub async fn run_env(
    program: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    timeout: Duration,
) -> Result<Output> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If this future is dropped (a caller-side cancellation), the child dies with it.
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|source| Error::Io {
        path: program.display().to_string(),
        source,
    })?;

    let out_task = tokio::spawn(drain_out(child.stdout.take()));
    let err_task = tokio::spawn(drain_err(child.stderr.take()));

    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(exited) => (exited?, false),
        Err(_) => {
            // start_kill() is SIGKILL; the wait() reaps it so we do not leave a zombie.
            let _ = child.start_kill();
            (child.wait().await?, true)
        }
    };

    let collected = tokio::time::timeout(PIPE_GRACE, async {
        (
            out_task.await.unwrap_or_default(),
            err_task.await.unwrap_or_default(),
        )
    })
    .await;
    let (stdout, stderr) = collected.unwrap_or_default();

    Ok(Output {
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        timed_out,
    })
}

/// Run an argv vector on a remote host over ssh.
///
/// Always emits `-o StrictHostKeyChecking=accept-new` and
/// `-o UserKnownHostsFile=<known_hosts>`; the dedicated `known_hosts` exists because vast
/// recycles `sshN.vast.ai` hostnames.
///
/// `host` is the ssh destination verbatim, so it may carry a user (`root@ssh5.vast.ai`).
/// `BatchMode=yes` is always set: this call captures output, so a password or passphrase
/// prompt could only ever hang, and failing fast beats burning the timeout.
///
/// `remote_argv` is passed to ssh as separate arguments — no local shell is involved — but
/// ssh itself joins them and hands the result to the *remote* login shell, which is the
/// protocol and cannot be avoided. Callers that need a remote word to survive spaces or
/// metacharacters must quote it for that remote shell.
pub async fn ssh(
    host: &str,
    port: u16,
    opts: &SshOpts,
    remote_argv: &[&str],
    timeout: Duration,
) -> Result<Output> {
    let argv = ssh_argv(host, port, opts, remote_argv);
    let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
    run(Path::new("ssh"), &borrowed, timeout).await
}

/// ssh connection options.
#[derive(Clone, Debug)]
pub struct SshOpts {
    /// A dedicated known_hosts file.
    pub known_hosts: PathBuf,
    /// ControlMaster socket. Worth a measured ~500 ms → RTT win for agentic tool loops.
    pub control_path: Option<PathBuf>,
    /// `-o ConnectTimeout=`.
    pub connect_timeout: u32,
    /// Extra `-o` options, already formatted.
    pub extra: Vec<String>,
}

/// The exact argv handed to `ssh`, split out so it can be asserted on without a network.
fn ssh_argv(host: &str, port: u16, opts: &SshOpts, remote_argv: &[&str]) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "-p".into(),
        port.to_string(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        format!("UserKnownHostsFile={}", opts.known_hosts.display()),
        "-o".into(),
        format!("ConnectTimeout={}", opts.connect_timeout),
    ];
    if let Some(cp) = &opts.control_path {
        argv.push("-o".into());
        argv.push("ControlMaster=auto".into());
        argv.push("-o".into());
        argv.push(format!("ControlPath={}", cp.display()));
        argv.push("-o".into());
        argv.push(CONTROL_PERSIST.into());
    }
    for extra in &opts.extra {
        argv.push("-o".into());
        argv.push(extra.clone());
    }
    argv.push(host.to_string());
    argv.extend(remote_argv.iter().map(|a| (*a).to_string()));
    argv
}

/// Read a child's stdout to EOF. A read error yields what was read so far.
async fn drain_out(pipe: Option<ChildStdout>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buf).await;
    }
    buf
}

/// Read a child's stderr to EOF. A read error yields what was read so far.
async fn drain_err(pipe: Option<ChildStderr>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buf).await;
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn opts() -> SshOpts {
        SshOpts {
            known_hosts: PathBuf::from("/home/x/.local/state/apexrouter/ssh/known_hosts"),
            control_path: None,
            connect_timeout: 10,
            extra: Vec::new(),
        }
    }

    /// `-o NAME=VALUE` pairs, as `["NAME=VALUE", ...]`, in order.
    fn o_flags(argv: &[String]) -> Vec<String> {
        argv.windows(2)
            .filter(|w| w[0] == "-o")
            .map(|w| w[1].clone())
            .collect()
    }

    /// Retry past `ETXTBSY`.
    ///
    /// A just-written script can briefly be "text file busy" because a *concurrent* test
    /// forked while our write fd was still open, and the fd only closes at that fork's
    /// `exec`. It is a property of running tests in parallel, not of the code under test.
    async fn without_etxtbsy<F, Fut>(mut attempt: F) -> Output
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<Output>>,
    {
        for _ in 0..20 {
            match attempt().await {
                Err(Error::Io { source, .. }) if source.raw_os_error() == Some(26) => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                other => return other.unwrap(),
            }
        }
        panic!("still ETXTBSY after 20 attempts");
    }

    /// Write an executable script into `dir` and return its path.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        drop(f);
        let mut perm = std::fs::metadata(&path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&path, perm).unwrap();
        path
    }

    #[tokio::test]
    async fn stdout_only_on_stdout() {
        let out = run(
            Path::new("/bin/echo"),
            &["hello world"],
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(out.status, 0);
        assert_eq!(out.stdout.trim_end(), "hello world");
        assert_eq!(out.stderr, "");
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn stderr_is_never_merged_into_stdout() {
        let out = run(
            Path::new("/bin/ls"),
            &["/definitely/not/a/real/path/for/apexrouter"],
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_ne!(out.status, 0);
        assert!(out.stdout.is_empty(), "stdout was {:?}", out.stdout);
        assert!(!out.stderr.is_empty());
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn both_streams_are_captured_separately() {
        let dir = tempfile::tempdir().unwrap();
        let sh = script(
            dir.path(),
            "both.sh",
            "#!/bin/sh\nprintf 'to-out'\nprintf 'to-err' 1>&2\nexit 3\n",
        );
        let out = without_etxtbsy(|| run(&sh, &[], Duration::from_secs(10))).await;
        assert_eq!(out.status, 3);
        assert_eq!(out.stdout, "to-out");
        assert_eq!(out.stderr, "to-err");
    }

    #[tokio::test]
    async fn argv_is_not_word_split() {
        // A shell would turn this into three arguments and expand the glob.
        let out = run(
            Path::new("/bin/echo"),
            &["a b; rm -rf *"],
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(out.stdout.trim_end(), "a b; rm -rf *");
    }

    #[tokio::test]
    async fn timeout_sets_the_flag_and_not_rc_124() {
        let out = run(Path::new("/bin/sleep"), &["60"], Duration::from_millis(150))
            .await
            .unwrap();
        assert!(out.timed_out);
        assert_ne!(out.status, 124, "a timeout must never masquerade as rc 124");
        assert_eq!(out.status, -1, "a killed child reports -1");
        assert!(out.stdout.is_empty());
    }

    #[tokio::test]
    async fn a_fast_command_does_not_time_out() {
        let out = run(Path::new("/bin/echo"), &["ok"], Duration::from_secs(30))
            .await
            .unwrap();
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn run_env_adds_to_the_environment() {
        let out = run_env(
            Path::new("/usr/bin/env"),
            &[],
            &[("APEXROUTER_TEST_VAR", "unit-c-07")],
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert!(
            out.stdout
                .lines()
                .any(|l| l == "APEXROUTER_TEST_VAR=unit-c-07"),
            "env output was {:?}",
            out.stdout
        );
        assert!(out.stderr.is_empty());
    }

    #[tokio::test]
    async fn a_missing_program_is_an_error_not_an_output() {
        let err = run(
            Path::new("/no/such/program/apexrouter"),
            &[],
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::Io { .. }));
    }

    #[test]
    fn ssh_always_emits_accept_new_and_known_hosts() {
        let argv = ssh_argv(
            "root@ssh5.vast.ai",
            41231,
            &opts(),
            &["cat", "/proc/uptime"],
        );
        let flags = o_flags(&argv);
        assert!(flags
            .iter()
            .any(|f| f == "StrictHostKeyChecking=accept-new"));
        assert!(flags
            .iter()
            .any(|f| f == "UserKnownHostsFile=/home/x/.local/state/apexrouter/ssh/known_hosts"));
        assert!(flags.iter().any(|f| f == "ConnectTimeout=10"));
        assert!(flags.iter().any(|f| f == "BatchMode=yes"));
        assert_eq!(argv[0], "-p");
        assert_eq!(argv[1], "41231");
        // Destination, then the remote argv, and nothing after it.
        let dest = argv.iter().position(|a| a == "root@ssh5.vast.ai").unwrap();
        assert_eq!(&argv[dest + 1..], &["cat", "/proc/uptime"]);
    }

    #[test]
    fn ssh_control_master_only_when_a_control_path_is_given() {
        let mut o = opts();
        assert!(!o_flags(&ssh_argv("h", 22, &o, &[]))
            .iter()
            .any(|f| f.starts_with("ControlPath=")));

        o.control_path = Some(PathBuf::from("/state/ssh/cm-1234"));
        let flags = o_flags(&ssh_argv("h", 22, &o, &[]));
        assert!(flags.iter().any(|f| f == "ControlMaster=auto"));
        assert!(flags.iter().any(|f| f == "ControlPath=/state/ssh/cm-1234"));
        assert!(flags.iter().any(|f| f == CONTROL_PERSIST));
    }

    #[test]
    fn ssh_extra_options_each_get_their_own_dash_o() {
        let mut o = opts();
        o.extra = vec![
            "ServerAliveInterval=30".into(),
            "ExitOnForwardFailure=yes".into(),
        ];
        let flags = o_flags(&ssh_argv("h", 22, &o, &[]));
        assert!(flags.iter().any(|f| f == "ServerAliveInterval=30"));
        assert!(flags.iter().any(|f| f == "ExitOnForwardFailure=yes"));
    }

    /// End-to-end proof that [`ssh`] hands those flags to a real `ssh` process: a stand-in
    /// binary earlier on `PATH` prints the argv it was called with.
    #[tokio::test]
    async fn ssh_passes_the_flags_to_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        script(dir.path(), "ssh", "#!/bin/sh\nprintf '%s\\n' \"$@\"\n");
        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old}", dir.path().display()));

        let o = opts();
        let out =
            without_etxtbsy(|| ssh("root@h", 2222, &o, &["true"], Duration::from_secs(10))).await;

        std::env::set_var("PATH", old);
        let lines: Vec<&str> = out.stdout.lines().collect();
        assert!(
            lines.contains(&"StrictHostKeyChecking=accept-new"),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"UserKnownHostsFile=/home/x/.local/state/apexrouter/ssh/known_hosts"),
            "{lines:?}"
        );
        assert!(lines.contains(&"root@h"), "{lines:?}");
        assert!(lines.contains(&"true"), "{lines:?}");
        assert!(out.stderr.is_empty());
    }
}
