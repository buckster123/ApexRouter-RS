//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs and the core `cmd/*` modules).
//! Do not edit outside that unit.
//!
//! Daemon resolution — the answer to "daemon-first is annoying".
//!
//! | Class | Commands | Daemon down → |
//! |---|---|---|
//! | `Pure` | `version`, `config path/show`, `fit`, `completions` | runs; no daemon involved |
//! | `ReadState` | `status`, `rig`, `models ls`, `endpoint ls`, `route ls`, `usage`, `doctor`, … | serves from `$STATE` under `LOCK_SH`, tagged `served_by: "offline"` |
//! | `Mutate` | everything else | **autostart** (default), poll `/health` for 5 s, proceed |

use apexrouter_client::NodeClient;
use apexrouter_core::config::Config;
use apexrouter_core::paths::Paths;
use apexrouter_core::store::Store;

/// What a subcommand needs in order to answer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Need {
    /// Nothing but this process.
    Pure,
    /// A picture of the world, which `$STATE` can supply when no daemon is running.
    ReadState,
    /// A daemon, because the operation changes something.
    Mutate,
}

/// Where this invocation's answers will come from.
pub enum Serving {
    /// A daemon answered.
    Daemon(NodeClient),
    /// Read from `$STATE` under `LOCK_SH`. Output is tagged `served_by: "offline"`.
    Offline(Store),
    /// Neither was possible, and here is why.
    None(anyhow::Error),
}

/// Resolve how this invocation will be served, autostarting when the need is `Mutate` and
/// `autostart` is on. Two racing autostarts converge on one daemon.
pub async fn resolve_serving(
    need: Need,
    cfg: &Config,
    paths: &Paths,
    autostart: bool,
) -> anyhow::Result<Serving> {
    todo!("S-06: resolve_serving")
}
