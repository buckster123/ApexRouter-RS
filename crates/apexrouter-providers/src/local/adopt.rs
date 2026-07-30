//! OWNER: unit P-01 (providers/src/local/**). Do not edit outside that unit.
//!
//! Re-adopting children across a daemon restart.
//!
//! `Adoption::Foreign` is **never signalled**. `POST /v1/endpoints/{id}/adopt` exists, but
//! it requires `/props` (or `/v1/models`) to match the spec's model path before it will
//! record `adopted: true`.
