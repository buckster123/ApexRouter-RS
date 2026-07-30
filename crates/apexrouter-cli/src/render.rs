//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs and the core `cmd/*` modules).
//! Do not edit outside that unit.
//!
//! Output. No colour crate, no emoji, and **nothing but the JSON on stdout** in `--json`
//! mode — `tracing` goes to stderr in every subcommand.
//!
//! `served_by`, `as_of_unix` and `stale` ride on **every** `--json` envelope, so a script
//! can tell where its answer came from without parsing prose. Human output prints one dim
//! line `(offline — apexrouterd is not running)` before the table.
//!
//! This module is the **only** place in the binary allowed to write to stdout. Everything
//! else calls [`print_line`], [`print_table`] or [`print_json`], and the test
//! `stdout_is_written_only_by_this_module` walks the crate's own sources to keep it that
//! way — house rule 7 expressed as a test rather than as a habit.

use apexrouter_protocol::{ErrorBody, ErrorEnvelope, ServedBy};
use serde::Serialize;
use serde_json::{Map, Value};
use std::io::Write;

/// The one line human output prints when no daemon answered.
pub const OFFLINE_NOTICE: &str = "(offline — apexrouterd is not running)";

/// Print `Envelope<T>` as pretty JSON on stdout, and nothing else.
///
/// # Errors
/// Propagates a serialisation failure from `T`.
pub fn print_json<T: Serialize>(
    served_by: ServedBy,
    as_of: i64,
    stale: bool,
    v: &T,
) -> anyhow::Result<()> {
    let env = envelope_value(served_by, as_of, stale, v)?;
    print_line(&serde_json::to_string_pretty(&env)?);
    Ok(())
}

/// Build the `--json` envelope without printing it. Factored out so it can be asserted on.
///
/// [`apexrouter_protocol::Envelope`] uses `#[serde(flatten)]`, which requires `T` to
/// serialise as a map; half the CLI's payloads are arrays (`Vec<EndpointRecord>`), and
/// flattening one of those is a runtime serde failure rather than a compile error. So the
/// merge happens here: a map payload is flattened alongside the envelope keys exactly as
/// the contract says, and anything else (array, string, number) is carried under `data`.
///
/// Payload keys win on a collision, which is what lets a `Snapshot` — it carries its own
/// `served_by`/`as_of_unix`/`stale` — pass through unchanged instead of emitting the same
/// key twice.
///
/// # Errors
/// Propagates a serialisation failure from `T`.
pub fn envelope_value<T: Serialize>(
    served_by: ServedBy,
    as_of: i64,
    stale: bool,
    v: &T,
) -> anyhow::Result<Value> {
    let mut out = Map::new();
    out.insert("served_by".into(), serde_json::to_value(served_by)?);
    out.insert("as_of_unix".into(), Value::from(as_of));
    out.insert("stale".into(), Value::from(stale));
    match serde_json::to_value(v)? {
        Value::Object(map) => {
            for (k, val) in map {
                out.insert(k, val);
            }
        }
        other => {
            out.insert("data".into(), other);
        }
    }
    Ok(Value::Object(out))
}

/// Space-padded columns, `"-"` for a missing value. No colour, no box drawing.
pub fn print_table(headers: &[&str], rows: Vec<Vec<String>>) {
    let cols = headers.len();
    let mut width: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    let filled: Vec<Vec<String>> = rows
        .into_iter()
        .map(|row| {
            (0..cols)
                .map(|i| {
                    let cell = row.get(i).map(String::as_str).unwrap_or("");
                    if cell.trim().is_empty() {
                        "-".to_string()
                    } else {
                        cell.to_string()
                    }
                })
                .collect()
        })
        .collect();
    for row in &filled {
        for (i, cell) in row.iter().enumerate() {
            if let Some(w) = width.get_mut(i) {
                *w = (*w).max(cell.chars().count());
            }
        }
    }

    let mut line = String::new();
    for (i, h) in headers.iter().enumerate() {
        push_cell(
            &mut line,
            h,
            width.get(i).copied().unwrap_or(0),
            i + 1 == cols,
        );
    }
    print_line(line.trim_end());
    for row in &filled {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            push_cell(
                &mut line,
                cell,
                width.get(i).copied().unwrap_or(0),
                i + 1 == cols,
            );
        }
        print_line(line.trim_end());
    }
}

/// One padded cell plus its separator. The last column is never padded.
fn push_cell(line: &mut String, cell: &str, width: usize, last: bool) {
    line.push_str(cell);
    if !last {
        for _ in cell.chars().count()..width {
            line.push(' ');
        }
        line.push_str("  ");
    }
}

/// The `--json` failure shape: `{"error":{"kind":"…","message":"…"}}` on **stdout**, exit 1.
///
/// `kind` is the machine-readable discriminator, which is what makes inventing exit codes
/// unnecessary.
///
/// # Errors
/// Propagates a serialisation failure.
pub fn print_error_json(kind: &str, msg: &str) -> anyhow::Result<()> {
    let body = ErrorEnvelope {
        error: ErrorBody {
            kind: kind.to_string(),
            message: msg.to_string(),
            param: None,
            code: None,
        },
    };
    print_line(&serde_json::to_string_pretty(&body)?);
    Ok(())
}

/// One line on stdout.
///
/// A closed pipe is swallowed: `apexrouter endpoint ls | head -1` must not print a panic
/// over the top of the user's output.
pub fn print_line(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{s}");
}

/// A blank separator line on stdout.
pub fn print_blank() {
    print_line("");
}

/// The single dim line human output prints before an offline table.
pub fn print_offline_notice() {
    print_line(OFFLINE_NOTICE);
}

/// The machine-readable discriminator for a failure, derived from the concrete error type
/// rather than from its message.
pub fn error_kind(e: &anyhow::Error) -> &'static str {
    use apexrouter_core::error::Error as CoreError;
    if let Some(core) = e.downcast_ref::<CoreError>() {
        return match core {
            CoreError::Io { .. } | CoreError::RawIo(_) => "io",
            CoreError::Toml(_) | CoreError::TomlSer(_) | CoreError::Json(_) => "parse",
            CoreError::Reqwest(_) => "upstream_unavailable",
            CoreError::Id(_) => "invalid_id",
            CoreError::NotFound(_) => "not_found",
            CoreError::Invalid { .. } => "invalid",
            CoreError::MissingCredential(_) => "missing_credential",
            CoreError::PortInUse { .. } => "port_in_use",
            CoreError::InsufficientVram { .. } => "insufficient_vram",
            CoreError::Conflict(_) => "conflict",
            CoreError::Timeout { .. } => "timeout",
            CoreError::Other(_) => "error",
        };
    }
    if let Some(client) = e.downcast_ref::<apexrouter_client::Error>() {
        return match client {
            apexrouter_client::Error::Http(_) => "daemon_unreachable",
            apexrouter_client::Error::Status { .. } => "daemon_error",
            apexrouter_client::Error::Decode { .. } => "parse",
            apexrouter_client::Error::Ws(_) => "websocket",
            apexrouter_client::Error::Url(_) => "invalid",
        };
    }
    "error"
}

// ---------------------------------------------------------------------------------------
// formatting helpers shared by the `cmd` modules
// ---------------------------------------------------------------------------------------

/// Now, in unix seconds. One place, so every envelope agrees.
pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Home the cursor and clear the screen — but only when stdout is a terminal, so
/// `status --watch | tee` is not filled with escape sequences.
pub fn clear_screen() {
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        print_line("\x1b[2J\x1b[H");
    }
}

/// An enum variant in its **serde spelling**: `Strategy::FirstHealthy` renders as
/// `first_healthy`, exactly as it appears in `--json`, in `routes.json` and in the docs.
///
/// Derived from `Debug` rather than from `serde_json`, so it works for the borrowed
/// references the render path has and cannot fail; any payload (`Other("webgpu")`,
/// `Ready { .. }`) is dropped, since these are table cells.
pub fn variant<T: std::fmt::Debug>(v: &T) -> String {
    let dbg = format!("{v:?}");
    let name = dbg
        .split(['(', ' ', '{'])
        .next()
        .unwrap_or(&dbg)
        .trim()
        .to_string();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// `"-"` for `None`, the value otherwise.
pub fn dash<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string())
}

/// Bytes as decimal GB — the unit every model card and the acceptance transcript use.
pub fn human_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.1} kB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}

/// MiB as a human string. VRAM is always MiB — that is what llama.cpp prints.
pub fn human_mib(mib: u64) -> String {
    if mib >= 1024 {
        format!("{:.1} GiB", mib as f64 / 1024.0)
    } else {
        format!("{mib} MiB")
    }
}

/// A duration in seconds as `3d4h`, `2h30m`, `12m05s` or `41s`.
pub fn human_secs(secs: i64) -> String {
    if secs < 0 {
        return "-".to_string();
    }
    let (d, h, m, s) = (
        secs / 86_400,
        (secs % 86_400) / 3_600,
        (secs % 3_600) / 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Payload {
        alias: String,
        n: u32,
    }

    #[test]
    fn envelope_carries_served_by_as_of_and_stale() {
        let v = envelope_value(
            ServedBy::Offline,
            1_785_412_331,
            true,
            &Payload {
                alias: "auto".into(),
                n: 2,
            },
        )
        .expect("envelope");
        assert_eq!(v["served_by"], Value::from("offline"));
        assert_eq!(v["as_of_unix"], Value::from(1_785_412_331_i64));
        assert_eq!(v["stale"], Value::from(true));
        // The payload is flattened alongside the envelope keys, per the contract.
        assert_eq!(v["alias"], Value::from("auto"));
        assert_eq!(v["n"], Value::from(2));
    }

    #[test]
    fn a_non_map_payload_is_carried_under_data_rather_than_failing() {
        // `Envelope<Vec<_>>` cannot be serialised by serde's `flatten`; half the CLI's
        // payloads are lists, so this path must exist and must keep the envelope keys.
        let rows = vec!["a", "b"];
        let v = envelope_value(ServedBy::Daemon, 7, false, &rows).expect("envelope");
        assert_eq!(v["served_by"], Value::from("daemon"));
        assert_eq!(v["data"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn a_payload_key_wins_over_the_envelope_key_so_nothing_is_emitted_twice() {
        let snap = serde_json::json!({"served_by": "daemon", "as_of_unix": 99, "stale": false});
        let v = envelope_value(ServedBy::Offline, 1, true, &snap).expect("envelope");
        assert_eq!(v["served_by"], Value::from("daemon"));
        assert_eq!(v["as_of_unix"], Value::from(99));
        let text = serde_json::to_string(&v).expect("json");
        assert_eq!(text.matches("served_by").count(), 1, "{text}");
    }

    #[test]
    fn error_envelope_is_the_documented_shape() {
        let body = ErrorEnvelope {
            error: ErrorBody {
                kind: "port_in_use".into(),
                message: "port 8888 is in use".into(),
                param: None,
                code: None,
            },
        };
        let v = serde_json::to_value(&body).expect("json");
        assert_eq!(v["error"]["kind"], Value::from("port_in_use"));
        assert_eq!(v["error"]["message"], Value::from("port 8888 is in use"));
    }

    #[test]
    fn error_kind_reads_the_type_not_the_message() {
        let e = anyhow::Error::new(apexrouter_core::error::Error::PortInUse {
            port: 8888,
            held_by: Some("localrouter".into()),
        });
        assert_eq!(error_kind(&e), "port_in_use");
        let e = anyhow::Error::new(apexrouter_core::error::Error::NotFound("alias".into()));
        assert_eq!(error_kind(&e), "not_found");
        assert_eq!(error_kind(&anyhow::anyhow!("plain")), "error");
    }

    #[test]
    fn table_pads_columns_and_dashes_missing_cells() {
        let headers = ["ID", "PORT"];
        let width = "local-carnice".chars().count();
        let mut line = String::new();
        push_cell(&mut line, headers[0], width, false);
        assert_eq!(line, format!("ID{}  ", " ".repeat(width - 2)));

        let mut last = String::new();
        push_cell(&mut last, "8100", 8, true);
        assert_eq!(last, "8100", "the last column is never padded");

        // A short row must not panic, and its missing cell renders as "-".
        print_table(
            &headers,
            vec![
                vec!["local-carnice".to_string(), "8100".to_string()],
                vec!["x".to_string()],
            ],
        );
    }

    #[test]
    fn human_helpers_read_the_way_the_gate_transcript_reads() {
        assert_eq!(human_bytes(6_900_000_000), "6.9 GB");
        assert_eq!(human_mib(4_352), "4.2 GiB");
        assert_eq!(human_mib(501), "501 MiB");
        assert_eq!(human_secs(41), "41s");
        assert_eq!(human_secs(725), "12m05s");
        assert_eq!(human_secs(9_000), "2h30m");
        assert_eq!(dash(None::<u16>), "-");
        assert_eq!(dash(Some(8100)), "8100");
    }

    /// House rule 7, as a test: nothing outside this module writes to stdout.
    ///
    /// `src/mcp/**` and `src/cmd/mcp.rs` are excluded because MCP JSON-RPC *is* stdout
    /// output and belongs to unit M-01; everything else in the binary must route through
    /// [`print_line`].
    #[test]
    fn stdout_is_written_only_by_this_module() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src, &mut files);
        assert!(files.len() > 10, "found no sources under {}", src.display());

        let mut offenders = Vec::new();
        for f in &files {
            let rel = f
                .strip_prefix(&src)
                .unwrap_or(f)
                .to_string_lossy()
                .to_string();
            if rel == "render.rs" || rel.starts_with("mcp/") || rel == "cmd/mcp.rs" {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(f) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                let code = line.trim_start();
                // Test code may print freely — a `#[cfg(test)]` module never ships.
                if code.starts_with("#[cfg(test)]") {
                    break;
                }
                if code.starts_with("//") {
                    continue;
                }
                if code.contains("println!")
                    || code.contains("print!(")
                    || code.contains("stdout()")
                {
                    offenders.push(format!("{rel}:{}: {}", i + 1, code.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "stdout must be written only through render.rs (house rule 7):\n{}",
            offenders.join("\n")
        );
    }
}
