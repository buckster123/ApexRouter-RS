//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter url [--json]` — prints `http://127.0.0.1:8888/v1` and nothing else.
//!
//! "Nothing else" is literal: no offline notice, no heading. This is the string a human
//! pastes into `OPENAI_BASE_URL`, and a stray line of prose in it is a broken agent config.
//!
//! A running daemon's own `proxy_url` wins over the configured bind, because `$PROXY_PORT`
//! may have moved it after this config was written.

use crate::cli::JsonFlag;
use crate::cmd::Ctx;
use crate::daemon;
use crate::render;
use apexrouter_protocol::ServedBy;
use std::net::SocketAddr;
use std::time::Duration;

/// Run `apexrouter url`.
///
/// # Errors
/// A lock-file failure. Never "no daemon": the configured bind is a complete answer.
pub async fn run(ctx: &Ctx, args: &JsonFlag) -> anyhow::Result<()> {
    let (base, served_by) = proxy_base(ctx)?;
    let url = format!("{base}/v1");
    if args.json {
        return render::print_json(
            served_by,
            render::now_unix(),
            false,
            &serde_json::json!({ "url": url, "proxy_url": base }),
        );
    }
    render::print_line(&url);
    Ok(())
}

/// The data plane's base URL — from the running daemon's owner record when there is one,
/// from `[server] proxy_bind` when there is not.
///
/// # Errors
/// A lock-file failure that is not "no daemon".
pub fn proxy_base(ctx: &Ctx) -> anyhow::Result<(String, ServedBy)> {
    if let Some(rec) = daemon::owner_record(&ctx.paths, Duration::ZERO)? {
        return Ok((
            rec.proxy_url.trim_end_matches('/').to_string(),
            ServedBy::Daemon,
        ));
    }
    Ok((url_for(ctx.cfg.proxy_bind()), ServedBy::Offline))
}

/// The control plane's base URL, resolved the same way.
///
/// # Errors
/// A lock-file failure that is not "no daemon".
pub fn control_base(ctx: &Ctx) -> anyhow::Result<(String, ServedBy)> {
    if let Some(rec) = daemon::owner_record(&ctx.paths, Duration::ZERO)? {
        return Ok((
            rec.control_url.trim_end_matches('/').to_string(),
            ServedBy::Daemon,
        ));
    }
    Ok((url_for(ctx.cfg.control_bind()), ServedBy::Offline))
}

/// A bind address as a URL a client can actually use.
///
/// `0.0.0.0` and `[::]` are *bind* addresses, not destinations: printing them would hand
/// the user a URL that resolves to nothing. They render as loopback, which is where a
/// wildcard bind is also listening.
pub fn url_for(addr: SocketAddr) -> String {
    if addr.ip().is_unspecified() {
        return format!("http://127.0.0.1:{}", addr.port());
    }
    if addr.is_ipv6() {
        return format!("http://[{}]:{}", addr.ip(), addr.port());
    }
    format!("http://{}:{}", addr.ip(), addr.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_bind_renders_as_loopback_not_as_zeros() {
        assert_eq!(
            url_for("0.0.0.0:8888".parse().expect("addr")),
            "http://127.0.0.1:8888"
        );
        assert_eq!(
            url_for("[::]:2739".parse().expect("addr")),
            "http://127.0.0.1:2739"
        );
    }

    #[test]
    fn a_real_address_renders_verbatim_and_ipv6_is_bracketed() {
        assert_eq!(
            url_for("127.0.0.1:8888".parse().expect("addr")),
            "http://127.0.0.1:8888"
        );
        assert_eq!(
            url_for("192.168.1.9:8888".parse().expect("addr")),
            "http://192.168.1.9:8888"
        );
        assert_eq!(
            url_for("[::1]:8888".parse().expect("addr")),
            "http://[::1]:8888"
        );
    }
}
