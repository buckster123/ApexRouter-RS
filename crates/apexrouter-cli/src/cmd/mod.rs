//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,
//! config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit
//! outside that unit — the remaining `cmd/*` modules belong to S-08 and `cmd/mcp.rs` to
//! M-01.
//!
//! One module per noun group. `--json` is **per subcommand, never global**, and prints
//! `serde_json::to_string_pretty` of the protocol type and **nothing else** on stdout.
//! `tracing` always goes to stderr, because `mcp` shares the binary and owns stdout.

pub mod approvals;
pub mod backend;
pub mod compare;
pub mod completions;
pub mod config;
pub mod doctor;
pub mod endpoint;
pub mod env;
pub mod fit;
pub mod hf;
pub mod mcp;
pub mod migrate;
pub mod models;
pub mod open;
pub mod profile;
pub mod provider;
pub mod recipe;
pub mod rig;
pub mod route;
pub mod serve;
pub mod smoke;
pub mod status;
pub mod swap;
pub mod switch;
pub mod token;
pub mod tunnel;
pub mod up;
pub mod url;
pub mod usage;
pub mod vast;
pub mod version;
