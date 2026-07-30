// OWNER: unit U-01 (ui-web/{index.html,app.js,style.css}). Do not edit outside that unit.
//
// Stage 0 placeholder, so rust-embed's #[folder = "../../ui-web"] compiles.
//
// U-01 builds: module-level state, el(tag, cls, text) + $(id) helpers,
// container.replaceChildren() re-render, WebSocket first with a REST first-paint fallback
// and 1 s -> x2 -> 15 s reconnect backoff, setInterval(render, 60_000) to keep relative
// timestamps honest, search inputs debounced 250 ms with a monotonic seq guard, and every
// fetch wrapped in try/catch with the connection dot as the single failure reporter.

"use strict";
