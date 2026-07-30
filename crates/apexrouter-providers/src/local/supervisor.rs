//! OWNER: unit P-01 (providers/src/local/**). Do not edit outside that unit.
//!
//! Spawning, health-gating, log rotation and stopping a local `llama-server`.
//!
//! * `setsid` + facts-on-disk **before** the spawn function returns, so a crash, a
//!   `systemctl --user restart` or a `cargo install` never evicts a model that took 90
//!   seconds and 6 GB to load.
//! * The health gate has a real wall-clock deadline that **resets on observed progress** —
//!   a 503 `{"status":"loading model"}` or a recognised load line means it is alive and
//!   working.
//! * Log rotation uses **copytruncate** semantics: an adopted child holds an fd to that
//!   inode, so renaming would send its output into a deleted file. Children we did not spawn
//!   are never rotated.
