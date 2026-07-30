//! OWNER: unit P-08 (providers/src/{checks,smoke,compare}.rs). Do not edit outside that
//! unit.
//!
//! `POST /v1/compare` — one prompt across N aliases.
//!
//! Runs them **in parallel** (LocalRouter ran them serially) and reports the **real**
//! `prompt_tokens` from the response; LocalRouter fabricated `word_count * 1.3` while
//! discarding the number the provider had actually sent.
