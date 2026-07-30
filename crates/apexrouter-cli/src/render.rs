//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs and the core `cmd/*` modules).
//! Do not edit outside that unit.
//!
//! Output. No colour crate, no emoji, and **nothing but the JSON on stdout** in `--json`
//! mode — `tracing` goes to stderr in every subcommand.
//!
//! `served_by`, `as_of_unix` and `stale` ride on **every** `--json` envelope, so a script
//! can tell where its answer came from without parsing prose. Human output prints one dim
//! line `(offline — apexrouterd is not running)` before the table.

use apexrouter_protocol::ServedBy;
use serde::Serialize;

/// Print `Envelope<T>` as pretty JSON on stdout, and nothing else.
pub fn print_json<T: Serialize>(
    served_by: ServedBy,
    as_of: i64,
    stale: bool,
    v: &T,
) -> anyhow::Result<()> {
    todo!("S-06: print_json")
}

/// Space-padded columns, `"-"` for a missing value. No colour, no box drawing.
pub fn print_table(headers: &[&str], rows: Vec<Vec<String>>) {
    todo!("S-06: print_table")
}

/// The `--json` failure shape: `{"error":{"kind":"…","message":"…"}}` on **stdout**, exit 1.
///
/// `kind` is the machine-readable discriminator, which is what makes inventing exit codes
/// unnecessary.
pub fn print_error_json(kind: &str, msg: &str) -> anyhow::Result<()> {
    todo!("S-06: print_error_json")
}
