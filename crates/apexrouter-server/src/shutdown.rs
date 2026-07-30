//! OWNER: unit S-01 (server/src/{lib,state,shutdown}.rs). Do not edit outside that unit.
//!
//! Graceful shutdown.
//!
//! `tokio::signal` sets a flag; both listeners stop accepting; in-flight requests drain to
//! `drain_timeout_secs`; ledger and usage appends complete inside awaited tasks. `SIGHUP`
//! **reloads config instead of exiting**. The lock is released by process exit.
//!
//! Two things shutdown deliberately does **not** do: it never signals a `llama-server`
//! child (they are `setsid`, and `kill_children_on_exit` defaults to false), and it **never
//! destroys a vast instance, at any setting** — a crash must not delete a paid box.
