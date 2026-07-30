//! OWNER: unit P-08 (providers/src/{checks,smoke,compare}.rs). Do not edit outside that
//! unit.
//!
//! Provider-specific `Check`s: `creds.{vast,hf,together}`, `ssh.controlmaster`,
//! `ssh.binary`, `vast.credit`, `vast.orphans`, `together.ratelimits`, `net.stall`, plus the
//! four deep SSH probes and the RX sample used by
//! `GET /v1/vast/instances/{id}/diagnose`.
//!
//! These live here rather than in `core::checks` because they need vast/together/ssh
//! clients; `apexrouter-server` registers them at startup, and `CheckCtx::ext` is how they
//! get their clients without `core` ever depending on this crate.
