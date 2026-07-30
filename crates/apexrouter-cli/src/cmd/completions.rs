//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter completions <bash|zsh|fish>`, via `clap_complete`.
//!
//! Class `Pure`, and deliberately more than that: it touches **nothing on disk**, not even
//! `Paths::resolve()`, so a packaging script can generate completions in a sandbox with no
//! `$HOME`. The script is derived from the same clap tree the binary parses with, so it can
//! never drift from the real surface.

use crate::cli::{Cli, CompletionsArgs};
use crate::render;
use clap::CommandFactory;

/// Run `apexrouter completions <shell>`.
///
/// # Errors
/// When the generated script is not valid UTF-8, which would mean `clap_complete` itself
/// misbehaved.
pub fn run(args: &CompletionsArgs) -> anyhow::Result<()> {
    render::print_line(&script(args.shell)?);
    Ok(())
}

/// The completion script for one shell.
///
/// # Errors
/// Non-UTF-8 output from `clap_complete`.
fn script(shell: clap_complete::Shell) -> anyhow::Result<String> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    let mut out: Vec<u8> = Vec::new();
    clap_complete::generate(shell, &mut cmd, name, &mut out);
    Ok(String::from_utf8(out)?.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_complete::Shell;

    #[test]
    fn every_documented_shell_generates_a_script_naming_the_binary() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let s = script(shell).expect("script");
            assert!(!s.is_empty(), "{shell} produced nothing");
            assert!(s.contains("apexrouter"), "{shell} did not name the binary");
        }
    }

    #[test]
    fn the_script_covers_the_verbs_this_unit_owns() {
        let s = script(Shell::Bash).expect("script");
        for verb in [
            "status",
            "serve",
            "url",
            "version",
            "completions",
            "config",
            "rig",
            "models",
            "fit",
            "endpoint",
            "route",
            "switch",
        ] {
            assert!(s.contains(verb), "bash completions are missing `{verb}`");
        }
    }
}
