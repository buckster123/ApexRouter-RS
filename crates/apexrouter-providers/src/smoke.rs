//! OWNER: unit P-08 (providers/src/{checks,smoke,compare}.rs). Do not edit outside that
//! unit.
//!
//! `smoke.sh`, reimplemented natively: `smoke.models`, `smoke.warmup` (80 tokens),
//! `smoke.tools` (a `get_weather` probe with `tool_choice: auto`) and `smoke.throughput`
//! (200 tokens).
//!
//! Two deliberate improvements over the shell script: TTFT and tok/s are read from the
//! **`timings` object**, not a stopwatch, and the probe uses **the resolved route's model
//! id** rather than the hardcoded `"model":"x"` that 400s on every managed provider.
