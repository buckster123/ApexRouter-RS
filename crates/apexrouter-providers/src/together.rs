//! OWNER: unit P-06 (providers/src/together.rs). Do not edit outside that unit.
//!
//! together.ai. Four measured facts this must honour:
//!
//! * `GET /v1/models` returns a **bare array**, not a `{"data":[]}` envelope.
//! * `pricing` hangs off each model object, and the pricing **unit assumption is recorded in
//!   the `CostEstimate::Approximate.assumption` string**, never silently applied.
//! * `finish_reason` is always a `String` — Together emits `eos`, which no enum covers.
//! * A 429 reads `x-ratelimit-reset`; `x-ratelimit-remaining` is **not** relied upon.
//!
//! The base URL comes from config or the legacy file and `api.together.xyz` is **never**
//! rewritten to `.ai`.
