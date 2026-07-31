//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter token create [--scope read|write|admin] | ls | revoke <id>`. Tokens are stored hashed and shown once at mint.
//!
//! # What mk1 actually has
//!
//! `server/src/auth.rs` accepts **one** bearer, read from the environment variable
//! `[server] token_env` names, and says so in its own comment: *"One configured token, one
//! operator: it carries every scope. Per-token scopes arrive with the `/v1/tokens` store."*
//! That store is not in this build, and `check_binds` points a human at this very command
//! (`export APEXROUTER_TOKEN=<a long random string> (or run `apexrouter token create`)`).
//!
//! So `create` **mints** and prints — it does not persist, because the only honest place to
//! put a bearer this build can use is the operator's own environment. That is also why the
//! secret never touches a file we own: writing it would be inventing a store, and a
//! half-invented credential store is worse than none. `ls` reports where the daemon looks
//! and whether it finds anything (**never** the value, §9.2), and `revoke` explains the one
//! operation that actually revokes: stop exporting it, and restart the daemon.
//!
//! The token is 256 bits read from `/dev/urandom`, hex-encoded. No new crate enters the
//! tree for 32 bytes, and `/dev/urandom` is the kernel CSPRNG — the same source
//! `getrandom(2)` draws from. A short read is an error, never a silently shorter token.

use crate::cli::{JsonFlag, ScopeArg, TokenCmd};
use crate::cmd::Ctx;
use crate::render;
use apexrouter_protocol::ServedBy;

/// Bytes of entropy in a minted token. 256 bits: a bearer on a LAN listener is the only
/// thing between a stranger and a control plane that can spend money.
const TOKEN_BYTES: usize = 32;

/// How many leading characters `ls` shows. Enough to tell two tokens apart in a shell
/// history, far too few to be worth capturing.
const FINGERPRINT_CHARS: usize = 8;

/// Run `apexrouter token …`.
///
/// # Errors
/// When the system entropy source cannot be read — the only failure that can reach here.
pub fn run(ctx: &Ctx, cmd: &TokenCmd) -> anyhow::Result<()> {
    match cmd {
        TokenCmd::Create { scope, json } => create(ctx, *scope, *json),
        TokenCmd::Ls(args) => list(ctx, args),
        TokenCmd::Revoke { id } => revoke(ctx, id),
    }
}

/// Mint one, print it **once**, and show exactly how to put it to work.
///
/// # Errors
/// A failure to read system entropy.
fn create(ctx: &Ctx, scope: ScopeArg, json: bool) -> anyhow::Result<()> {
    let token = mint()?;
    let var = ctx.cfg.server.token_env.trim();
    let var = if var.is_empty() {
        "APEXROUTER_TOKEN"
    } else {
        var
    };

    if json {
        return render::print_json(
            ServedBy::Offline,
            render::now_unix(),
            false,
            &serde_json::json!({
                "token": token,
                "scope": scope.as_str(),
                "env_var": var,
                "shown_once": true,
                "stored": false,
            }),
        );
    }
    render::print_line(&token);
    render::print_blank();
    render::print_line("This is the only time it is shown. To put it to work:");
    render::print_line(&format!("  export {var}={token}"));
    render::print_line(&format!(
        "  apexrouter serve --detach --allow-remote --token-env {var}"
    ));
    render::print_line(&format!(
        "Scope `{}`: mk1's auth accepts one configured bearer and grants it every scope, so \
         the scope you chose is a note to yourself until the /v1/tokens store lands.",
        scope.as_str()
    ));
    Ok(())
}

/// Where the daemon looks, and whether it finds anything. **Never** the value.
///
/// # Errors
/// None today; the signature matches its siblings so the dispatch arm stays uniform.
fn list(ctx: &Ctx, args: &JsonFlag) -> anyhow::Result<()> {
    let sources = sources(&ctx.cfg.server.token_env);

    if args.json {
        let rows: Vec<serde_json::Value> = sources
            .iter()
            .map(|(source, fingerprint)| {
                serde_json::json!({
                    "source": source,
                    "present": fingerprint.is_some(),
                    "fingerprint": fingerprint,
                })
            })
            .collect();
        return render::print_json(ServedBy::Offline, render::now_unix(), false, &rows);
    }
    render::print_table(
        &["SOURCE", "PRESENT", "FINGERPRINT"],
        sources
            .iter()
            .map(|(source, fingerprint)| row(source, fingerprint.as_deref()))
            .collect(),
    );
    Ok(())
}

/// Every place the daemon looks for a bearer, in resolution order, **without repeats**.
///
/// `[server] token_env` wins and `$APEXROUTER_TOKEN` is the fallback — but the shipped
/// config names `APEXROUTER_TOKEN`, so on a default install those are the same variable and
/// listing it twice would read as two independent credentials.
pub fn sources(token_env: &str) -> Vec<(String, Option<String>)> {
    const FALLBACK: &str = "APEXROUTER_TOKEN";
    let configured = token_env.trim();
    let mut out: Vec<(String, Option<String>)> = Vec::with_capacity(2);
    if !configured.is_empty() {
        out.push((format!("env:{configured}"), present(configured)));
    }
    if configured != FALLBACK {
        out.push((format!("env:{FALLBACK}"), present(FALLBACK)));
    }
    out
}

/// Say what actually revokes a bearer in this build.
///
/// # Errors
/// None; it is a message, and an honest one beats a no-op that claims success.
fn revoke(ctx: &Ctx, id: &str) -> anyhow::Result<()> {
    let var = ctx.cfg.server.token_env.trim();
    let var = if var.is_empty() {
        "APEXROUTER_TOKEN"
    } else {
        var
    };
    anyhow::bail!(
        "there is no token store in this build to revoke `{id}` from — the daemon accepts \
         one bearer, read from ${var}. Revoke it by unsetting or replacing that variable \
         and restarting: `apexrouter serve --stop`, then `export {var}=$(apexrouter token \
         create)`."
    )
}

/// 256 bits of system entropy, hex-encoded.
///
/// # Errors
/// When the system entropy source cannot be read.
pub fn mint() -> anyhow::Result<String> {
    use std::io::Read;
    let mut buf = [0u8; TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| anyhow::anyhow!("could not read system entropy from /dev/urandom: {e}"))?;
    Ok(hex(&buf))
}

/// Lowercase hex, without pulling in a crate for sixteen characters.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(nibble(b >> 4));
        out.push(nibble(b & 0x0f));
    }
    out
}

/// One hex digit.
fn nibble(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        _ => (b'a' + (v - 10)) as char,
    }
}

/// The fingerprint of the token in `var`, or `None` when it is unset or empty.
///
/// A **prefix**, never the value: §9.2 says a credential's source is publishable and its
/// content is not, and this command exists to answer "is one configured?".
fn present(var: &str) -> Option<String> {
    if var.is_empty() {
        return None;
    }
    let v = std::env::var(var).ok()?;
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    Some(format!(
        "{}…",
        v.chars().take(FINGERPRINT_CHARS).collect::<String>()
    ))
}

/// One row of the `ls` table.
fn row(source: &str, fingerprint: Option<&str>) -> Vec<String> {
    vec![
        source.to_string(),
        if fingerprint.is_some() { "yes" } else { "no" }.to_string(),
        fingerprint.unwrap_or("").to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_token_is_256_bits_of_hex_and_never_repeats() {
        let a = mint().expect("entropy");
        let b = mint().expect("entropy");
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b, "two mints must not collide");
    }

    #[test]
    fn hex_encodes_the_way_every_other_tool_does() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }

    #[test]
    fn a_fingerprint_is_a_prefix_and_an_unset_var_is_absent() {
        std::env::set_var("APEXROUTER_TEST_TOKEN_FP", "0123456789abcdef");
        assert_eq!(
            present("APEXROUTER_TEST_TOKEN_FP").as_deref(),
            Some("01234567…")
        );
        std::env::set_var("APEXROUTER_TEST_TOKEN_FP", "   ");
        assert_eq!(present("APEXROUTER_TEST_TOKEN_FP"), None);
        std::env::remove_var("APEXROUTER_TEST_TOKEN_FP");
        assert_eq!(present("APEXROUTER_TEST_TOKEN_FP"), None);
        assert_eq!(present(""), None);
    }

    #[test]
    fn the_default_config_lists_one_source_not_the_same_one_twice() {
        assert_eq!(sources("APEXROUTER_TOKEN").len(), 1);
        let two = sources("MY_TOKEN");
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].0, "env:MY_TOKEN");
        assert_eq!(two[1].0, "env:APEXROUTER_TOKEN");
        // An empty `token_env` leaves only the fallback.
        assert_eq!(sources("  ").len(), 1);
    }

    #[test]
    fn a_fingerprint_never_carries_the_whole_secret() {
        std::env::set_var("APEXROUTER_TEST_TOKEN_FP2", "supersecretvalue");
        let fp = present("APEXROUTER_TEST_TOKEN_FP2").expect("present");
        assert!(!fp.contains("value"), "{fp}");
        std::env::remove_var("APEXROUTER_TEST_TOKEN_FP2");
    }
}
