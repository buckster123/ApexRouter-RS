//! OWNER: unit CL-01 (client/src/lib.rs, client/src/ws.rs). Do not edit outside that unit.
//!
//! The WebSocket half of [`crate::NodeClient`]: connect, decode `Event`s, and reconnect with
//! exponential backoff (1 s → ×2 → cap 15 s), re-emitting a full snapshot on reconnect so a
//! surface never renders a stale picture after a blip.
