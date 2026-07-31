//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter env` — prints `export OPENAI_BASE_URL=… ; OPENAI_API_KEY=not-needed`.
//!
//! Like [`crate::cmd::url`], "nothing else" is literal: this is output a human pipes into
//! `eval` or pastes into a `.envrc`, so a line of prose in it is a broken shell.
//!
//! `OPENAI_API_KEY` is exported as `not-needed` rather than omitted, because an SDK that
//! finds the variable absent raises "no api key configured" before it ever reaches the
//! proxy — and the proxy is what holds the real credential.

use crate::cli::JsonFlag;
use crate::cmd::{url, Ctx};
use crate::render;

/// The placeholder key. The proxy holds the real one; the client must still send something.
pub const PLACEHOLDER_KEY: &str = "not-needed";

/// Run `apexrouter env`.
///
/// # Errors
/// A lock-file failure. Never "no daemon": the configured bind is a complete answer.
pub async fn run(ctx: &Ctx, args: &JsonFlag) -> anyhow::Result<()> {
    let (base, served_by) = url::proxy_base(ctx)?;
    if args.json {
        return render::print_json(
            served_by,
            render::now_unix(),
            false,
            &serde_json::json!({
                "OPENAI_BASE_URL": format!("{base}/v1"),
                "OPENAI_API_KEY": PLACEHOLDER_KEY,
                "ANTHROPIC_BASE_URL": base,
            }),
        );
    }
    for line in exports(&base) {
        render::print_line(&line);
    }
    Ok(())
}

/// The export lines, in the order a human reads them.
///
/// `ANTHROPIC_BASE_URL` carries the **bare** proxy base, not `/v1`: the Claude Code harness
/// appends `/v1/messages` itself, and a doubled `/v1` is a 404 nobody enjoys diagnosing.
pub fn exports(proxy_base: &str) -> Vec<String> {
    vec![
        format!("export OPENAI_BASE_URL={proxy_base}/v1"),
        format!("export OPENAI_API_KEY={PLACEHOLDER_KEY}"),
        format!("export ANTHROPIC_BASE_URL={proxy_base}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exports_are_eval_safe_and_carry_no_prose() {
        let lines = exports("http://127.0.0.1:8888");
        assert_eq!(lines[0], "export OPENAI_BASE_URL=http://127.0.0.1:8888/v1");
        assert_eq!(lines[1], "export OPENAI_API_KEY=not-needed");
        assert_eq!(lines[2], "export ANTHROPIC_BASE_URL=http://127.0.0.1:8888");
        for l in &lines {
            assert!(l.starts_with("export "), "{l}");
            assert!(!l.contains('#'), "a comment would break `eval`: {l}");
        }
    }

    #[test]
    fn the_anthropic_base_never_carries_a_v1_segment() {
        let lines = exports("http://127.0.0.1:8888");
        assert!(!lines[2].ends_with("/v1"), "{}", lines[2]);
    }
}
