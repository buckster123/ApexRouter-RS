//! A fake `llama-server`, argv-compatible with llama.cpp b9199.
//!
//! The file name is the contract: `core::discover::builds` only ever looks for a binary
//! called `llama-server`, and labels the build after the directory two levels above it.
//!
//! It answers the three probes discovery makes (`--help`, `--list-devices`, `--version`),
//! **records the argv and environment it was launched with**, then serves the subset of
//! the b9199 HTTP surface the supervisor's health gate and the router actually use.
//!
//! ```text
//! llama-server -m /models/x.gguf --host 127.0.0.1 --port 8100 -a fake-9b --props --slots
//!              [--apex-behavior load_ms=500,chunks=8] [--apex-record /path]
//! ```

use apexrouter_tests_support::fake::{
    behavior_for, devices_text, help_text, record_dest, version_text,
};
use apexrouter_tests_support::record::LaunchRecord;
use apexrouter_tests_support::server::{Config, Server};
use std::io::Write;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let argv0 = std::env::args().next().unwrap_or_default();

    // ---- the three discovery probes ---------------------------------------------------
    // Each exits before anything is recorded: a probe is not a launch.
    if argv
        .iter()
        .any(|a| a == "--help" || a == "-h" || a == "--usage")
    {
        print!("{}", help_text());
        return;
    }
    if argv.iter().any(|a| a == "--version") {
        print!("{}", version_text());
        return;
    }
    if argv.iter().any(|a| a == "--list-devices") {
        print!("{}", devices_text());
        return;
    }

    // ---- the launch record, written BEFORE anything can go wrong -----------------------
    let record = LaunchRecord::from_process();
    if let Some(dest) = record_dest(&argv, &argv0) {
        record.write_to(&dest);
    }

    let behavior = behavior_for(&argv, &argv0);
    let host = record
        .host
        .clone()
        .unwrap_or_else(|| "127.0.0.1".to_owned());
    let port = record.port.unwrap_or(0);
    let model = record.model.clone().unwrap_or_default();

    say(&format!(
        "build: {} with GNU 15.2.0 for x86_64-linux-gnu",
        apexrouter_tests_support::BUILD_INFO
    ));
    say(&format!(
        "system info: n_threads = 12, LD_LIBRARY_PATH = {}",
        record.env_var("LD_LIBRARY_PATH").unwrap_or("<unset>")
    ));

    // ---- the two failure modes that happen before a socket exists ----------------------
    if behavior.refuse_start {
        say(&format!(
            "common_init_from_params: failed to load model '{model}'"
        ));
        say("srv    load_model: failed to load model, terminating");
        std::process::exit(behavior.exit_code);
    }
    if behavior.stall {
        // Alive, never listening: connection refused is not progress, so the health gate's
        // deadline is allowed to expire. This is `--fake-never-healthy`.
        say("apex-fake: not binding a port, on purpose (stall)");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    // ---- bind, then load ----------------------------------------------------------------
    // llama.cpp binds before the weights are read, which is why `/health` can answer
    // `503 {"status":"loading model"}` at all.
    let server = match Server::bind(Config {
        host,
        port,
        record,
        behavior,
        log: Box::new(say),
        // A subprocess dying on demand is a behaviour under test.
        allow_process_exit: true,
    }) {
        Ok(s) => s,
        Err(e) => {
            say(&format!("srv    main: failed to bind port {port}: {e}"));
            std::process::exit(1);
        }
    };
    server.start_loading();
    server.serve();
}

/// llama.cpp writes its log to stdout, and the supervisor has redirected that into the
/// endpoint's log file — which is where the health gate reads boot progress from.
fn say(line: &str) {
    println!("{line}");
    let _ = std::io::stdout().flush();
}
