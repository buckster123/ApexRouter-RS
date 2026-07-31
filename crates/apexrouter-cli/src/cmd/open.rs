//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter open` — ensure the daemon, then `xdg-open` the web UI.
//!
//! The verb is `Mutate` for one reason: opening a browser at a control plane that is not
//! listening produces a connection-refused page, not a dashboard. So the daemon is started
//! (or the `--no-autostart` refusal is reported) *before* anything is launched.
//!
//! House rule 5 is why the opener is an argv vector: `exec::run` takes `&[&str]` and there
//! is no shell anywhere on this path, so a control URL is never re-parsed by `sh`.

use crate::cmd::{url, Ctx};
use crate::daemon::Need;
use crate::render;
use apexrouter_core::exec;
use std::path::Path;
use std::time::Duration;

/// How long the opener gets. `xdg-open` normally forks a browser and returns at once; a
/// desktop with no handler registered can sit there, and a hung CLI is worse than a message.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// The openers tried, in order. `xdg-open` is the freedesktop standard; the other two are
/// what a GNOME or KDE box has when `xdg-utils` is not installed.
const OPENERS: [&str; 3] = ["xdg-open", "gio", "kde-open"];

/// Run `apexrouter open`.
///
/// # Errors
/// A daemon that will not start, or a machine with no usable opener — in which case the URL
/// is printed, because "here is the address" is a better failure than "could not open".
pub async fn run(ctx: &Ctx) -> anyhow::Result<()> {
    // Resolving `Mutate` is what autostarts (or refuses to autostart) the daemon.
    ctx.serving(Need::Mutate).await?.into_daemon()?;
    let (base, _) = url::control_base(ctx)?;

    render::print_line(&base);
    match open_url(&base).await {
        Ok(program) => {
            tracing::debug!(program, url = %base, "opened the web UI");
            Ok(())
        }
        Err(e) => {
            // Not fatal: the URL is on stdout already, and a headless box is a normal place
            // to run this by accident.
            render::print_line(&format!("(could not launch a browser: {e})"));
            Ok(())
        }
    }
}

/// Hand `url` to the first opener that exists and exits 0. Returns the one that worked.
///
/// # Errors
/// When every opener is missing or fails, with the last failure's detail attached.
pub async fn open_url(url: &str) -> anyhow::Result<&'static str> {
    let mut last = String::new();
    for program in OPENERS {
        let args = argv_for(program, url);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match exec::run(Path::new(program), &refs, OPEN_TIMEOUT).await {
            Ok(out) if out.status == 0 => return Ok(program),
            Ok(out) => last = format!("{program} exited {}: {}", out.status, out.stderr.trim()),
            Err(e) => last = format!("{program}: {e}"),
        }
    }
    anyhow::bail!(
        "no desktop opener worked ({}); open the URL yourself",
        if last.is_empty() {
            "none were found".to_string()
        } else {
            last
        }
    )
}

/// The argv one opener needs for a URL. `gio` wants a subcommand; the other two do not.
fn argv_for(program: &str, url: &str) -> Vec<String> {
    match program {
        "gio" => vec!["open".to_string(), url.to_string()],
        _ => vec![url.to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gio_takes_a_subcommand_and_the_others_do_not() {
        assert_eq!(argv_for("gio", "http://x"), ["open", "http://x"]);
        assert_eq!(argv_for("xdg-open", "http://x"), ["http://x"]);
        assert_eq!(argv_for("kde-open", "http://x"), ["http://x"]);
    }

    #[test]
    fn the_url_is_never_concatenated_into_a_command_string() {
        // House rule 5, as a test: every element is a separate argv slot, so a URL with a
        // `;` in it cannot become a second command.
        let argv = argv_for("xdg-open", "http://127.0.0.1:2739/?a=1;rm -rf /");
        assert_eq!(argv.len(), 1);
        assert!(argv[0].contains(';'), "the argument is passed verbatim");
    }
}
