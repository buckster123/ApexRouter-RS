//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs and the core `cmd/*` modules).
//! Do not edit outside that unit.
//!
//! The `clap` derive tree: noun-grouped, house verb vocabulary, `--json` **per subcommand**.
//!
//! Global flags `--config` and `--home` are pushed into the process env **before**
//! `Config::load()`, so env vars stay the single resolution mechanism.
