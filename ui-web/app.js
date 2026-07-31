// OWNER: unit U-01 (ui-web/{index.html,app.js,style.css}). Do not edit outside that unit.
//
// The whole web UI: module-level state, `el(tag, cls, text)` + `$(id)` helpers,
// `container.replaceChildren()` re-render, WebSocket first with a REST first-paint fallback
// and a 1 s -> x2 -> 15 s reconnect backoff, `setInterval(render, 60_000)` to keep relative
// timestamps honest, search inputs debounced 250 ms behind a monotonic `seq` guard, and
// every `fetch` wrapped in try/catch with the connection dot as the single failure reporter.
//
// Rules this file keeps, because they are checked by review and not by a compiler:
//   * `textContent` everywhere, `innerHTML` (and `outerHTML`, `insertAdjacentHTML`,
//     `document.write`) nowhere. Nothing here builds a DOM node out of a string;
//   * nothing is ever rendered into a fixed number of slots — the rig has as many devices as
//     it has, seen through as many backends as enumerate them, and the fleet is 0..n boxes;
//   * a control-plane route that answers 404/501/503 latches a feature-unavailable flag and
//     the panel says so, instead of the UI looking broken because a Stage-5 unit has not
//     landed yet;
//   * every money action states $/hr, the estimate and the remaining credit, and needs an
//     explicit confirmation typed by a human before the request is sent.

"use strict";

// ---------------------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------------------

/** Everything the UI knows. One object, so a re-render is a pure function of it. */
const S = {
  snap: null, // the last Snapshot (REST first paint, then WS)
  panel: "routes", // active tab
  ws: null,
  connected: false,
  backoff: 1000, // 1 s -> x2 -> cap 15 s
  reconnectTimer: null,
  lastError: null, // what the connection dot is complaining about

  requests: [], // finished RequestRecords, newest first
  inflight: new Map(), // RequestId -> {id, alias, backend, at}
  probes: {}, // alias -> SmokeProbe from POST /v1/routes/{alias}/test

  usage: { since: "24h", by: "provider", summary: null, day: null, backend: null },
  checks: [], // CheckResults, from GET /v1/checks and the diagnose stream
  smoke: [], // SmokeProbes from POST /v1/smoke
  localModels: [],
  providerModels: {}, // provider id -> UpstreamModel[]
  hf: { q: "", rows: [], files: {}, busy: false },
  offers: { rows: [], relaxations: [], sort: "dph_total", dir: 1, filter: "", busy: false },
  boots: {}, // BackendId -> {phase, line, at}
  logs: { id: null, lines: [], follow: false, filter: "", src: null },

  deviceFilter: null, // rig strip -> Backends filter (a physical device key)
  gone: {}, // feature key -> the reason it is unavailable
  seq: {}, // monotonic guards for debounced searches
  launch: null, // the Launch drawer's whole draft, built by resetLaunch()
  edit: null, // what the editor drawer is showing
  loaded: {}, // panel -> true once its lazy data has been fetched
  dirty: false,
};

/** Panels, in tab order. The hash (`#backends`) is a deep link and a screenshot handle. */
const PANELS = [
  "routes",
  "backends",
  "fleet",
  "catalog",
  "providers",
  "requests",
  "usage",
  "doctor",
];

// ---------------------------------------------------------------------------------------
// tiny helpers
// ---------------------------------------------------------------------------------------

/** `document.getElementById`, because it is used a hundred times. */
function $(id) {
  return document.getElementById(id);
}

/** One element, with an optional class list and text. Never parses HTML. */
function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined && text !== null) n.textContent = String(text);
  return n;
}

/** An SVG element — the charts are hand-rolled, so there is no CDN in this app. */
function svgEl(tag, attrs) {
  const n = document.createElementNS("http://www.w3.org/2000/svg", tag);
  for (const k of Object.keys(attrs || {})) n.setAttribute(k, String(attrs[k]));
  return n;
}

/** Append many children at once, skipping nulls so callers can write conditionals inline. */
function add(parent, ...kids) {
  for (const k of kids) if (k) parent.appendChild(k);
  return parent;
}

/** A button with an icon and a label — never colour alone. */
function btn(label, icon, cls, onClick) {
  const b = el("button", "btn " + (cls || ""));
  b.type = "button";
  if (icon) add(b, el("span", "badge-ico", icon));
  add(b, el("span", null, label));
  if (onClick) b.addEventListener("click", onClick);
  return b;
}

/** A badge: icon + label, always both. `tone` is only ever a health tone. */
function badge(label, icon, tone, title) {
  const b = el("span", "badge" + (tone ? " badge-" + tone : ""));
  if (icon) add(b, el("span", "badge-ico", icon));
  add(b, el("span", null, label));
  if (title) b.title = title;
  return b;
}

/** A labelled field wrapper. */
function field(label, control, hint) {
  const f = el("label", "field");
  add(f, el("span", null, label), control);
  if (hint) add(f, el("span", "hint", hint));
  return f;
}

/** `<select>` from `[value, label]` pairs. */
function select(options, value, onChange) {
  const s = el("select");
  for (const o of options) {
    const opt = el("option", null, o[1]);
    opt.value = o[0];
    if (String(o[0]) === String(value)) opt.selected = true;
    s.appendChild(opt);
  }
  if (onChange) s.addEventListener("change", () => onChange(s.value));
  return s;
}

/** A text/number input. */
function input(type, value, onInput, attrs) {
  const i = el("input");
  i.type = type;
  if (value !== undefined && value !== null) i.value = String(value);
  for (const k of Object.keys(attrs || {})) i.setAttribute(k, String(attrs[k]));
  if (onInput) i.addEventListener("input", () => onInput(i.value, i));
  return i;
}

/** A definition list from `[term, value]` pairs; `null` values are dropped. */
function kv(pairs) {
  const d = el("dl", "kv");
  for (const p of pairs) {
    if (p[1] === null || p[1] === undefined) continue;
    add(d, el("dt", null, p[0]));
    const dd = el("dd");
    if (p[1] instanceof Node) dd.appendChild(p[1]);
    else dd.textContent = String(p[1]);
    add(d, dd);
  }
  return d;
}

/** Debounce that also carries a monotonic sequence number, so a stale answer is dropped. */
function debounced(key, ms, fn) {
  let timer = null;
  return function (...args) {
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      S.seq[key] = (S.seq[key] || 0) + 1;
      fn(S.seq[key], ...args);
    }, ms);
  };
}

/** True when `n` is still the newest issued sequence number for `key`. */
function fresh(key, n) {
  return S.seq[key] === n;
}

// ---------------------------------------------------------------------------------------
// formatting — every number the daemon is honest about stays honest here
// ---------------------------------------------------------------------------------------

/** Micro-USD integer -> `$3.34`, keeping six digits when a sub-cent price would round to 0. */
function money(micro) {
  if (micro === null || micro === undefined) return "—";
  const neg = micro < 0 ? "-" : "";
  const abs = Math.abs(micro);
  if (abs % 10000 === 0) {
    return neg + "$" + Math.floor(abs / 1e6) + "." + String(Math.floor((abs % 1e6) / 1e4)).padStart(2, "0");
  }
  return neg + "$" + Math.floor(abs / 1e6) + "." + String(abs % 1e6).padStart(6, "0");
}

/** Plain dollars (a float, as vast.ai reports them). */
function usd(v, digits) {
  if (v === null || v === undefined || !isFinite(v)) return "—";
  return "$" + Number(v).toFixed(digits === undefined ? 2 : digits);
}

/** A `CostEstimate` as a node: the amount plus a metered/approximate badge with its why. */
function costNode(c) {
  const wrap = el("span", "split");
  if (!c || c.kind === "unknown") {
    add(wrap, el("span", "muted", "—"), badge("unknown", "?", "unknown", "no price is known; nothing is invented"));
    return wrap;
  }
  add(wrap, el("span", "mono", money(c.usd)));
  if (c.kind === "metered") {
    add(wrap, badge("metered", "✓", null, "source: " + (c.source || "")));
  } else {
    add(wrap, badge("approx", "≈", null, c.assumption || "derived under an assumption"));
  }
  return wrap;
}

/** The bare amount of a `CostEstimate`, for sorting and summing. */
function costUsd(c) {
  if (!c || c.kind === "unknown" || c.usd === undefined) return null;
  return c.usd / 1e6;
}

/** A `TokenCount` -> `1234` plus a badge when it was estimated rather than reported. */
function tokensNode(t) {
  if (!t) return el("span", "muted", "—");
  const wrap = el("span", "split");
  add(wrap, el("span", "mono", String(t.n)));
  if (t.kind !== "reported") add(wrap, badge("est.", "≈", null, "estimated: no upstream reported a count"));
  return wrap;
}

/** MiB -> `11.4 GB` / `812 MB`. */
function mb(v) {
  if (v === null || v === undefined) return "—";
  const n = Number(v);
  if (!isFinite(n)) return "—";
  if (Math.abs(n) >= 1024) return (n / 1024).toFixed(n >= 10240 ? 0 : 1) + " GB";
  return Math.round(n) + " MB";
}

/** Bytes -> `4.7 GB`. */
function bytes(v) {
  if (v === null || v === undefined) return "—";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let n = Number(v);
  let i = 0;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i += 1;
  }
  return n.toFixed(n >= 10 || i === 0 ? 0 : 1) + " " + u[i];
}

/** Seconds -> `2h 14m`, `3d 4h`, `41s`. */
function dur(secs) {
  if (secs === null || secs === undefined || !isFinite(secs)) return "—";
  const s = Math.max(0, Math.floor(secs));
  if (s < 60) return s + "s";
  const m = Math.floor(s / 60);
  if (m < 60) return m + "m " + (s % 60) + "s";
  const h = Math.floor(m / 60);
  if (h < 24) return h + "h " + (m % 60) + "m";
  return Math.floor(h / 24) + "d " + (h % 24) + "h";
}

/** Unix seconds -> `4m ago`. The 60 s re-render is what keeps this honest. */
function ago(unix) {
  if (!unix) return "—";
  const d = Date.now() / 1000 - unix;
  if (d < 0) return "just now";
  return dur(d) + " ago";
}

/** Unix seconds -> a local wall-clock time, for the request log. */
function clock(unix) {
  if (!unix) return "—";
  const d = new Date(unix * 1000);
  return (
    String(d.getHours()).padStart(2, "0") +
    ":" +
    String(d.getMinutes()).padStart(2, "0") +
    ":" +
    String(d.getSeconds()).padStart(2, "0")
  );
}

/** A number, or an em dash. Never `NaN`, never `0` standing in for "unknown". */
function num(v, digits) {
  if (v === null || v === undefined || !isFinite(v)) return "—";
  return Number(v).toFixed(digits === undefined ? 1 : digits);
}

/** A snake_case wire token -> `Round robin`, for display only. */
function pretty(s) {
  if (!s) return "";
  const t = String(s).replace(/_/g, " ");
  return t.charAt(0).toUpperCase() + t.slice(1);
}

/** A slug that `Alias::parse`/`RecipeId::parse` will accept. */
function slug(s, fallback) {
  let out = String(s || "")
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/-+/g, "-")
    .replace(/^[^a-z0-9]+/, "")
    .replace(/[-._]+$/, "")
    .slice(0, 64);
  if (!out) out = fallback || "item";
  return out;
}

/** `Health` -> what to draw: an icon, a label and the one place colour means something. */
function healthOf(h) {
  const st = h && h.state ? h.state : "unknown";
  switch (st) {
    case "ready":
      return { tone: "good", icon: "●", label: "Ready", detail: "since " + ago(h.since_unix) };
    case "starting":
      return {
        tone: "warn",
        icon: "◐",
        label: "Starting" + (h.phase && h.phase.phase ? " · " + pretty(h.phase.phase) : ""),
        detail: h.detail || "",
      };
    case "degraded":
      return { tone: "serious", icon: "▲", label: "Degraded", detail: h.reason || "" };
    case "down":
      return { tone: "critical", icon: "✕", label: "Down", detail: h.reason || "" };
    case "draining":
      return { tone: "warn", icon: "⇣", label: "Draining", detail: h.in_flight + " in flight" };
    default:
      return { tone: "unknown", icon: "○", label: "Unknown", detail: "not probed yet" };
  }
}

/** A `CredentialSource` -> where it lives, never what it is. */
function credentialText(c) {
  if (!c) return "none";
  switch (c.kind) {
    case "env":
      return "env:" + c.var;
    case "file":
      return "file:" + c.path;
    case "managed":
      return "managed:" + c.store;
    case "instance":
      return "per-instance";
    default:
      return "none";
  }
}

/** A `PriceModel` -> a display string. `PerHour` without a throughput hint stays honest. */
function priceText(p, tps) {
  if (!p) return null;
  if (p.kind === "free") return "free";
  if (p.kind === "per_hour") return usd(p.dph / 1e6, 3) + "/hr";
  if (p.kind === "per_token") {
    return money(Math.round((p.input + p.output) / 2)) + "/Mtok";
  }
  return null;
}

/** Blended `$/Mtok`, mirroring `PriceModel::per_mtok`: no hint, no number. */
function perMtok(backend) {
  const p = backend && backend.price;
  if (!p) return null;
  if (p.kind === "free") return { usd: 0, guess: false };
  if (p.kind === "per_token") return { usd: (p.input + p.output) / 2 / 1e6, guess: true };
  if (p.kind === "per_hour") {
    const tps = backend.health && backend.health.tps_p50;
    if (!tps || tps <= 0) return null;
    const mtokPerHour = (tps * 3600) / 1e6;
    return { usd: p.dph / 1e6 / mtokPerHour, guess: true };
  }
  return null;
}

/** The p-th percentile of a numeric array. */
function pct(values, p) {
  const v = values.filter((x) => x !== null && x !== undefined && isFinite(x)).sort((a, b) => a - b);
  if (!v.length) return null;
  const i = Math.min(v.length - 1, Math.max(0, Math.round((p / 100) * (v.length - 1))));
  return v[i];
}

// ---------------------------------------------------------------------------------------
// the API layer — one wrapper, one failure reporter
// ---------------------------------------------------------------------------------------

/** Which feature a control-plane path belongs to, for the unavailable latch. */
function featureOf(path) {
  const m = /^\/v1\/([a-z-]+)/.exec(path);
  if (!m) return "core";
  const head = m[1];
  if (head === "vast" || head === "tunnels" || head === "approvals") return "vast";
  if (head === "hf") return "hf";
  if (head === "providers") return "providers";
  if (head === "checks" || head === "diagnose" || head === "smoke") return "checks";
  if (head === "compare") return "compare";
  return "core";
}

/**
 * One `fetch`, wrapped. Returns `{ok, status, data, error}` and never throws.
 *
 * A transport failure sets the connection dot — the single failure reporter — and a
 * 404/501/503 latches the feature flag for the panel that asked, so a Stage-5 route that has
 * not landed yet reads as "not in this build" instead of as a broken page.
 */
async function api(path, opts) {
  const o = opts || {};
  const init = { method: o.method || "GET", headers: {} };
  if (o.body !== undefined) {
    init.headers["content-type"] = "application/json";
    init.body = JSON.stringify(o.body);
  }
  try {
    const res = await fetch(path, init);
    if (res.status === 404 || res.status === 501 || res.status === 503) {
      const f = featureOf(path);
      if (f !== "core") S.gone[f] = "the daemon answered " + res.status + " for " + path;
    }
    let data = null;
    const ct = res.headers.get("content-type") || "";
    if (res.status !== 204) {
      if (ct.indexOf("json") >= 0) data = await res.json().catch(() => null);
      else data = await res.text().catch(() => null);
    }
    if (!res.ok) {
      const err = data && data.error ? data.error.message || data.error.kind : String(res.status);
      return { ok: false, status: res.status, data: data, error: err };
    }
    if (path.indexOf("/v1/") === 0 && featureOf(path) !== "core") delete S.gone[featureOf(path)];
    return { ok: true, status: res.status, data: data, error: null };
  } catch (e) {
    S.lastError = String(e && e.message ? e.message : e);
    S.connected = false;
    paintConnection();
    return { ok: false, status: 0, data: null, error: S.lastError };
  }
}

/** `api()` plus a toast on failure — for buttons, where silence is the wrong answer. */
async function act(path, opts, okMsg) {
  const r = await api(path, opts);
  if (r.ok) {
    if (okMsg) toast(okMsg, "ok");
  } else {
    toast((opts && opts.method ? opts.method + " " : "") + path + " — " + (r.error || "failed"), "bad");
  }
  return r;
}

// ---------------------------------------------------------------------------------------
// toasts
// ---------------------------------------------------------------------------------------

/** A transient message. `tone` is `ok`, `bad` or `info`. */
function toast(message, tone) {
  const box = $("toasts");
  const t = el("div", "toast toast-" + (tone || "info"), message);
  box.appendChild(t);
  setTimeout(() => t.remove(), tone === "bad" ? 9000 : 4500);
}

// ---------------------------------------------------------------------------------------
// the connection: WebSocket first, REST first paint, 1 s -> x2 -> 15 s backoff
// ---------------------------------------------------------------------------------------

/** Paint the connection dot. It is the only place a transport failure is reported. */
function paintConnection() {
  const dot = $("conn-dot");
  const text = $("conn-text");
  if (!dot || !text) return;
  dot.className = "dot " + (S.connected ? "dot-good" : S.snap ? "dot-warn" : "dot-critical");
  if (S.connected) {
    text.textContent = "live";
    $("rb-conn").title = "streaming events from /ws";
  } else {
    text.textContent = S.snap ? "reconnecting…" : "offline";
    $("rb-conn").title = S.lastError || "the control plane is not answering";
  }
}

/** First paint over REST, so the page is useful before the socket is up. */
async function firstPaint() {
  const r = await api("/v1/snapshot");
  if (r.ok && r.data) {
    S.snap = r.data;
    scheduleRender();
  }
}

/** Subscribe to `/ws`. On close, reconnect with 1 s -> x2 -> cap 15 s. */
function connectWS() {
  if (S.ws) {
    try {
      S.ws.close();
    } catch (e) {
      /* already gone */
    }
    S.ws = null;
  }
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  let ws;
  try {
    ws = new WebSocket(proto + "//" + location.host + "/ws");
  } catch (e) {
    S.lastError = String(e);
    scheduleReconnect();
    return;
  }
  S.ws = ws;

  ws.addEventListener("open", () => {
    S.connected = true;
    S.backoff = 1000;
    S.lastError = null;
    paintConnection();
  });
  ws.addEventListener("message", (m) => {
    let ev = null;
    try {
      ev = JSON.parse(m.data);
    } catch (e) {
      return; // a frame we cannot parse is the daemon's bug, not a reason to die
    }
    applyEvent(ev);
  });
  ws.addEventListener("error", () => {
    S.lastError = "the event stream errored";
  });
  ws.addEventListener("close", () => {
    S.connected = false;
    paintConnection();
    scheduleReconnect();
  });
}

/** The backoff. Doubling, capped at 15 s, reset to 1 s by a successful open. */
function scheduleReconnect() {
  if (S.reconnectTimer) return;
  const wait = S.backoff;
  S.reconnectTimer = setTimeout(() => {
    S.reconnectTimer = null;
    connectWS();
    // Re-read the snapshot too: events missed while disconnected are gone for good.
    firstPaint();
  }, wait);
  S.backoff = Math.min(15000, S.backoff * 2);
}

/** Fold one `Event` into the state. The tag names are the Rust enum's, verbatim. */
function applyEvent(ev) {
  if (!ev || !ev.type) return;
  switch (ev.type) {
    case "snapshot":
      // An internally tagged newtype variant: the Snapshot's own fields are at top level.
      S.snap = ev;
      break;
    case "backend_changed": {
      if (!S.snap) break;
      const b = ev.backend;
      const i = S.snap.backends.findIndex((x) => x.id === b.id);
      if (i >= 0) S.snap.backends[i] = b;
      else S.snap.backends.push(b);
      break;
    }
    case "backend_removed":
      if (S.snap) S.snap.backends = S.snap.backends.filter((x) => x.id !== ev.id);
      break;
    case "route_table_changed":
      if (S.snap) {
        S.snap.routes = ev.routes || [];
        S.snap.proxy.table_valid = ev.valid;
        S.snap.proxy.table_error = ev.error || null;
      }
      break;
    case "rig_changed":
      if (S.snap) S.snap.rig = ev.rig;
      break;
    case "request_started":
      S.inflight.set(ev.id, { id: ev.id, alias: ev.alias, backend: ev.backend, at: Date.now() / 1000 });
      break;
    case "request_finished":
      S.inflight.delete(ev.record.id);
      S.requests.unshift(ev.record);
      if (S.requests.length > 400) S.requests.length = 400;
      break;
    case "boot_progress":
      S.boots[ev.backend] = { phase: ev.phase, line: ev.line, at: Date.now() / 1000 };
      break;
    case "log_line":
      if (S.logs.src && ev.source && ev.source.id === S.logs.src) {
        S.logs.lines.push(ev.line);
        if (S.logs.lines.length > 2000) S.logs.lines.splice(0, S.logs.lines.length - 2000);
      }
      break;
    case "vast_fleet_changed":
      if (S.snap) {
        S.snap.instances = ev.instances || [];
        if (ev.credit !== null && ev.credit !== undefined) S.snap.totals.vast_credit = ev.credit;
      }
      break;
    case "usage_tick":
      if (S.snap && ev.window) S.snap.totals.tokens_24h = ev.window.total_prompt + ev.window.total_completion;
      break;
    case "job_changed": {
      if (!S.snap) break;
      const j = ev.job;
      const i = (S.snap.jobs || []).findIndex((x) => x.id === j.id);
      if (i >= 0) S.snap.jobs[i] = j;
      else (S.snap.jobs = S.snap.jobs || []).push(j);
      if (S.launch && S.launch.job && S.launch.job.id === j.id) S.launch.job = j;
      break;
    }
    case "check_result": {
      const i = S.checks.findIndex((c) => c.id === ev.result.id);
      if (i >= 0) S.checks[i] = ev.result;
      else S.checks.push(ev.result);
      break;
    }
    case "alert": {
      if (S.snap) {
        const a = {
          id: ev.id,
          level: ev.level,
          message: ev.message,
          action: ev.action || null,
          at_unix: Math.floor(Date.now() / 1000),
        };
        const i = (S.snap.alerts || []).findIndex((x) => x.id === a.id);
        if (i >= 0) S.snap.alerts[i] = a;
        else (S.snap.alerts = S.snap.alerts || []).push(a);
      }
      if (ev.level === "critical" || ev.level === "serious") toast(ev.message, "bad");
      break;
    }
    default:
      break; // an unknown event from a newer daemon is not an error
  }
  scheduleRender();
}

// ---------------------------------------------------------------------------------------
// render scheduling
// ---------------------------------------------------------------------------------------

/** Coalesce renders to one per frame: a router at 50 rps must not drive 50 layouts. */
function scheduleRender() {
  if (S.dirty) return;
  S.dirty = true;
  requestAnimationFrame(() => {
    S.dirty = false;
    render();
  });
}

/** Re-render the bars and the visible panel. Drawers own their own re-render. */
function render() {
  paintConnection();
  renderRouterBar();
  renderRigStrip();
  renderAlerts();
  renderTabs();
  renderPanel();
  if (S.launch && !$("drawer-launch").hidden) renderLaunchSummary();
}

// ---------------------------------------------------------------------------------------
// router bar
// ---------------------------------------------------------------------------------------

/** The one string the user copies, the live rates, and the default-alias dropdown. */
function renderRouterBar() {
  const p = S.snap ? S.snap.proxy : null;
  $("rb-version").textContent = S.snap ? "v" + S.snap.version : "";
  $("rb-base-url").textContent = p ? p.base_url.replace(/\/+$/, "") + "/v1" : "…";

  const stats = $("rb-stats");
  const t = S.snap ? S.snap.totals : null;
  const tiles = [];
  tiles.push(statTile(p ? String(p.inflight) : "—", "in flight"));
  tiles.push(statTile(p ? num(p.req_per_min, 1) : "—", "req/min"));
  tiles.push(statTile(p ? num(p.tok_per_s, 1) : "—", "tok/s"));
  const spend = t ? costUsd(t.spend_24h) : null;
  tiles.push(statTile(spend === null ? "—" : usd(spend), "24 h spend"));
  if (t && t.burn_rate_usd_hr) tiles.push(statTile(money(t.burn_rate_usd_hr) + "/hr", "burn"));
  if (t && t.vast_credit !== null && t.vast_credit !== undefined) {
    tiles.push(statTile(usd(t.vast_credit), "credit"));
  }
  stats.replaceChildren(...tiles);

  const sel = $("rb-default-alias");
  if (document.activeElement !== sel) {
    const routes = S.snap ? S.snap.routes || [] : [];
    const opts = routes.map((r) => [r.alias, r.alias]);
    if (!opts.length) opts.push(["", "no routes"]);
    const cur = p ? p.default_alias : "";
    sel.replaceChildren(
      ...opts.map((o) => {
        const opt = el("option", null, o[1]);
        opt.value = o[0];
        if (o[0] === cur) opt.selected = true;
        return opt;
      }),
    );
    sel.disabled = !routes.length;
  }
}

/** One number + caption in the router bar. */
function statTile(value, caption) {
  const s = el("div", "stat");
  add(s, el("span", "stat-v", value), el("span", "stat-k", caption));
  return s;
}

/** Clipboard, with a fallback for the browser that refuses the async API. */
async function copyText(text) {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch (e) {
    /* fall through to the textarea */
  }
  const ta = el("textarea");
  ta.value = text;
  ta.setAttribute("readonly", "readonly");
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  // `execCommand("copy")` needs the document focused and the text selected; a headless or
  // background window has neither, which is why the button reports a failure instead of
  // pretending to have copied.
  window.focus();
  ta.focus();
  ta.select();
  ta.setSelectionRange(0, ta.value.length);
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch (e) {
    ok = false;
  }
  ta.remove();
  return ok;
}

/** Flash the `copied` label beside the copy buttons. */
function flashCopied(ok) {
  const c = $("rb-copied");
  c.textContent = ok ? "copied" : "copy failed — select it by hand";
  c.hidden = false;
  setTimeout(() => {
    c.hidden = true;
  }, 2000);
}

// ---------------------------------------------------------------------------------------
// rig strip — one bar per PHYSICAL device, per-backend views folded in
// ---------------------------------------------------------------------------------------

/** `rig::normalise_device_name`, in JS. */
function normaliseName(name) {
  let out = "";
  let depth = 0;
  for (const c of String(name || "")) {
    if (c === "(" || c === "[") depth += 1;
    else if (c === ")" || c === "]") depth = Math.max(0, depth - 1);
    else if (depth === 0) out += c.toLowerCase();
  }
  return out.split(/\s+/).filter(Boolean).join(" ");
}

/**
 * `rig::physical_devices`, in JS.
 *
 * One physical GPU seen by both a Vulkan and a ROCm build is ONE bar with two views — the
 * same grouping the VRAM budget now uses, so the strip cannot claim twice the memory the
 * solver will allow.
 */
function physicalDevices(gpus) {
  const counts = new Map();
  const out = [];
  for (const g of gpus || []) {
    const bucket = JSON.stringify(g.backend) + "|" + normaliseName(g.name);
    const ordinal = counts.has(bucket) ? counts.get(bucket) + 1 : 0;
    counts.set(bucket, ordinal);
    const key = g.pci_bus_id && !g.is_software ? "pci:" + g.pci_bus_id : "name:" + normaliseName(g.name) + "#" + ordinal;
    let dev = out.find((d) => d.key === key && d.is_software === g.is_software);
    if (!dev) {
      dev = { key: key, name: g.name, is_software: g.is_software, pci_bus_id: g.pci_bus_id || null, views: [] };
      out.push(dev);
    }
    dev.views.push(g);
  }
  return out;
}

/** A backend enum value (`"vulkan"` or `{"other":"…"}`) as a label. */
function backendName(b) {
  if (!b) return "?";
  if (typeof b === "string") return b === "rocm" ? "ROCm" : pretty(b);
  const k = Object.keys(b)[0];
  return String(b[k]);
}

/** The rig strip: devices, then RAM, then swap. Clicking a device filters Backends. */
function renderRigStrip() {
  const strip = $("rigstrip");
  if (!S.snap) {
    strip.replaceChildren(el("div", "hint", "waiting for the daemon…"));
    return;
  }
  const rig = S.snap.rig || { gpus: [], builds: [] };
  const kids = [];

  for (const d of physicalDevices(rig.gpus)) {
    const card = el("button", "rigcard" + (S.deviceFilter === d.key ? " is-selected" : ""));
    card.type = "button";
    const top = el("div", "rig-top");
    add(top, el("span", "rig-name truncate", d.name + (d.is_software ? " (software)" : "")));

    // Two backends can enumerate one card and disagree about its size — on this machine
    // ROCm says 11 GB and Vulkan says 20 GB of the same iGPU. The headline is therefore the
    // most conservative view, and every view is spelled out below it, because the VRAM
    // budget is solved per backend and a headline that averaged them would be fiction.
    // Never compute total - free either: ROCm reports free > total (GTT accounting).
    const overcommit = d.views.some((v) => v.vram_free_mb > v.vram_total_mb);
    const view = d.views.reduce((a, b) => (b.vram_free_mb < a.vram_free_mb ? b : a), d.views[0]);
    add(top, el("span", "rig-num", mb(view.vram_free_mb) + " free / " + mb(view.vram_total_mb)));
    add(card, top);

    const frac = view.vram_total_mb > 0 ? Math.min(1, view.vram_free_mb / view.vram_total_mb) : 0;
    const meter = el("div", "meter");
    const fill = el("span");
    fill.style.width = Math.round(frac * 100) + "%";
    if (frac < 0.15) fill.className = "is-hot";
    add(meter, fill);
    add(card, meter);

    const sub = el("div", "rig-sub");
    const multi = d.views.length > 1;
    const seen = [];
    for (const v of d.views) {
      const name = backendName(v.backend);
      if (seen.indexOf(name) >= 0) continue;
      seen.push(name);
      add(
        sub,
        badge(
          multi ? name + " " + mb(v.vram_free_mb) + " free" : name,
          "▤",
          null,
          v.device +
            ": a " +
            name +
            " build sees " +
            mb(v.vram_free_mb) +
            " free of " +
            mb(v.vram_total_mb) +
            ". A budget is solved over one backend's devices, never summed across them.",
        ),
      );
    }
    const held = [];
    for (const v of d.views) for (const h of v.held_by || []) if (held.indexOf(h) < 0) held.push(h);
    if (held.length) add(sub, badge("held by " + held.join(", "), "⇄", null, "backends occupying this device"));
    else add(sub, el("span", "muted", "free"));
    if (overcommit) add(sub, badge("GTT", "ⓘ", null, "free > total: shared-memory accounting, so used is not total − free"));
    if (view.reserved_mb) add(sub, el("span", "muted", "reserved " + mb(view.reserved_mb)));
    add(card, sub);

    card.title = d.views
      .map((v) => v.device + " · " + backendName(v.backend) + " · " + mb(v.vram_free_mb) + " free")
      .join("\n");
    card.addEventListener("click", () => {
      S.deviceFilter = S.deviceFilter === d.key ? null : d.key;
      if (S.deviceFilter) show("backends");
      scheduleRender();
    });
    kids.push(card);
  }

  kids.push(hostBar("RAM", rig.ram_total_mb - rig.ram_free_mb, rig.ram_total_mb, (rig.cpu_threads || 0) + " CPU threads"));
  kids.push(hostBar("Swap", rig.swap_used_mb, rig.swap_total_mb, "scanned " + ago(rig.scanned_at_unix)));
  strip.replaceChildren(...kids);
}

/** RAM / swap, drawn like a device but not clickable. */
function hostBar(label, used, total, note) {
  const card = el("div", "rigcard is-static");
  const top = el("div", "rig-top");
  add(top, el("span", "rig-name", label), el("span", "rig-num", mb(used) + " / " + mb(total)));
  add(card, top);
  const meter = el("div", "meter");
  const fill = el("span");
  const frac = total > 0 ? Math.min(1, used / total) : 0;
  fill.style.width = Math.round(frac * 100) + "%";
  if (frac > 0.85) fill.className = "is-hot";
  add(meter, fill);
  add(card, meter, el("div", "rig-sub", note));
  return card;
}

// ---------------------------------------------------------------------------------------
// alerts
// ---------------------------------------------------------------------------------------

/** Standing alerts, loudest first, each with the action it names as a button. */
function renderAlerts() {
  const box = $("alerts");
  const alerts = (S.snap && S.snap.alerts) || [];
  if (!alerts.length) {
    box.replaceChildren();
    box.hidden = true;
    return;
  }
  const order = { critical: 0, serious: 1, warning: 2, info: 3 };
  const rank = (lvl) => (Object.prototype.hasOwnProperty.call(order, lvl) ? order[lvl] : 9);
  const sorted = alerts.slice().sort((a, b) => rank(a.level) - rank(b.level));
  const icons = { critical: "⛔", serious: "▲", warning: "!", info: "ⓘ" };
  box.replaceChildren(
    ...sorted.map((a) => {
      const row = el("div", "alert alert-" + a.level);
      add(row, badge(pretty(a.level), icons[a.level] || "ⓘ", a.level === "info" ? null : a.level));
      add(row, el("span", "alert-msg", a.message));
      if (a.action) add(row, alertAction(a));
      add(row, el("span", "alert-when", ago(a.at_unix)));
      return row;
    }),
  );
  box.hidden = false;
}

/** Map an alert's `action` verb onto a button. Money verbs go through the typed confirm. */
function alertAction(a) {
  const verb = String(a.action);
  if (verb === "destroy") {
    const id = (/\d{4,}/.exec(a.message) || [])[0];
    return btn("Destroy…", "⛔", "btn-sm btn-danger", () => {
      if (id) confirmMoney(destroyInstancePlan(Number(id)));
      else show("fleet");
    });
  }
  if (verb === "reconcile") {
    return btn("Reconcile", "⇄", "btn-sm", () => {
      show("fleet");
      loadPanel("fleet", true);
    });
  }
  if (verb === "restart_download" || verb === "restart-download") {
    const id = (/\d{4,}/.exec(a.message) || [])[0];
    return btn("Restart download", "⟳", "btn-sm", () =>
      act("/v1/vast/instances/" + id + "/restart-download", { method: "POST" }, "download restarted"),
    );
  }
  return badge(verb, "→", null, "the daemon suggests: " + verb);
}

// ---------------------------------------------------------------------------------------
// tabs, panels, hash routing
// ---------------------------------------------------------------------------------------

/** Mark the active tab. */
function renderTabs() {
  for (const t of document.querySelectorAll(".tab")) {
    t.classList.toggle("is-active", t.dataset.panel === S.panel);
    t.setAttribute("aria-current", t.dataset.panel === S.panel ? "page" : "false");
  }
}

/** Switch panels, update the hash, and lazily fetch what the panel needs. */
function show(panel) {
  if (PANELS.indexOf(panel) < 0) panel = "routes";
  S.panel = panel;
  if (location.hash !== "#" + panel) history.replaceState(null, "", "#" + panel);
  loadPanel(panel, false);
  scheduleRender();
}

/** Render the visible panel; hide the rest. */
function renderPanel() {
  for (const p of PANELS) {
    const node = $("panel-" + p);
    if (!node) continue;
    node.hidden = p !== S.panel;
  }
  const target = $("panel-" + S.panel);
  if (!target) return;
  const fn = {
    routes: renderRoutes,
    backends: renderBackends,
    fleet: renderFleet,
    catalog: renderCatalog,
    providers: renderProviders,
    requests: renderRequests,
    usage: renderUsage,
    doctor: renderDoctor,
  }[S.panel];
  if (fn) fn(target);
}

/** A standing "this build does not have that route" note. */
function unavailableNote(feature, what) {
  const b = el("div", "banner banner-note");
  add(
    b,
    badge("unavailable", "⊘", null, S.gone[feature] || ""),
    el(
      "span",
      null,
      what + " is not answering on this daemon (" + (S.gone[feature] || "not built yet") + "). Everything else keeps working.",
    ),
  );
  return b;
}

/** A panel header with a title, an optional hint and trailing controls. */
function panelHead(title, hint, ...controls) {
  const h = el("div", "panel-head");
  add(h, el("h2", null, title));
  if (hint) add(h, el("span", "hint", hint));
  add(h, el("span", "spacer"));
  for (const c of controls) add(h, c);
  return h;
}

/** Fetch what a panel needs, once, unless `force`. Everything else rides the WS. */
async function loadPanel(panel, force) {
  if (!force && S.loaded[panel]) return;
  S.loaded[panel] = true;
  if (panel === "requests") {
    const r = await api("/v1/requests?limit=200");
    if (r.ok && Array.isArray(r.data)) S.requests = r.data;
  } else if (panel === "usage") {
    await loadUsage();
  } else if (panel === "doctor") {
    const r = await api("/v1/checks");
    if (r.ok && Array.isArray(r.data)) S.checks = r.data;
  } else if (panel === "catalog") {
    await loadLocalModels(false);
  }
  scheduleRender();
}

/** The discovered GGUFs. Shared by the Catalog panel and the Launch drawer. */
async function loadLocalModels(refresh) {
  const r = await api("/v1/models/local" + (refresh ? "?refresh=true" : ""));
  if (r.ok && Array.isArray(r.data)) S.localModels = r.data;
  return S.localModels;
}

// ---------------------------------------------------------------------------------------
// panel: Routes
// ---------------------------------------------------------------------------------------

/** Every backend a selector names, in registry order. */
function backendsFor(sel) {
  const all = (S.snap && S.snap.backends) || [];
  if (!sel) return [];
  if (sel.sel === "id") return all.filter((b) => b.id === sel.value);
  if (sel.sel === "tag") return all.filter((b) => (b.tags || []).indexOf(sel.value) >= 0);
  if (sel.sel === "glob") {
    const rx = new RegExp("^" + String(sel.value).split("*").map(escapeRx).join(".*") + "$");
    return all.filter((b) => rx.test(b.id));
  }
  return [];
}

/** Escape a literal for use inside a `RegExp`. */
function escapeRx(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** A route's health roll-up: the best state any of its targets can offer. */
function routeHealth(route) {
  let seen = 0;
  let best = null;
  const rank = { ready: 0, starting: 1, draining: 2, degraded: 3, down: 4, unknown: 5 };
  for (const t of route.targets || []) {
    for (const b of backendsFor(t.backend)) {
      if (!b.enabled) continue;
      seen += 1;
      const st = (b.health && b.health.state) || "unknown";
      if (best === null || rank[st] < rank[best]) best = st;
    }
  }
  if (!seen) return { tone: "critical", icon: "✕", label: "no targets", detail: "nothing this alias names exists" };
  return healthOf({ state: best });
}

/** p50 of a field over this session's finished requests for one alias. */
function aliasP(alias, field, p) {
  return pct(
    S.requests.filter((r) => r.alias === alias).map((r) => r[field]),
    p === undefined ? 50 : p,
  );
}

/** The cheapest blended `$/Mtok` any of a route's targets can offer. */
function routePrice(route) {
  let best = null;
  let guess = false;
  for (const t of route.targets || []) {
    for (const b of backendsFor(t.backend)) {
      const p = perMtok(b);
      if (!p) continue;
      if (best === null || p.usd < best) {
        best = p.usd;
        guess = p.guess;
      }
    }
  }
  if (best === null) return el("span", "muted", "—");
  if (best === 0) return badge("free", "✓", null, "a local model costs nothing per token");
  const wrap = el("span", "split");
  add(wrap, el("span", "mono", "$" + best.toFixed(best < 1 ? 4 : 2)));
  if (guess) add(wrap, badge("approx", "≈", null, "blended from a price model under a stated assumption"));
  return wrap;
}

/** One target chip, with reorder controls that persist immediately. */
function targetChip(route, i, onChange) {
  const t = route.targets[i];
  const c = el("span", "chip chip-drag");
  c.draggable = true;
  c.dataset.index = String(i);
  const live = backendsFor(t.backend);
  const label = (t.backend.sel === "id" ? "" : t.backend.sel + ":") + t.backend.value;
  add(c, el("span", null, label));
  if (t.model) add(c, el("span", "muted", "→ " + t.model));
  const h = live.length ? healthOf(live[0].health) : { tone: "critical", icon: "✕", label: "missing" };
  add(c, badge(h.label, h.icon, h.tone, live.length ? "" : "no backend matches this selector"));

  const up = el("button", "chip-btn", "↑");
  up.type = "button";
  up.title = "move earlier";
  up.addEventListener("click", () => {
    if (i === 0) return;
    const t2 = route.targets.splice(i, 1)[0];
    route.targets.splice(i - 1, 0, t2);
    onChange();
  });
  const down = el("button", "chip-btn", "↓");
  down.type = "button";
  down.title = "move later";
  down.addEventListener("click", () => {
    if (i >= route.targets.length - 1) return;
    const t2 = route.targets.splice(i, 1)[0];
    route.targets.splice(i + 1, 0, t2);
    onChange();
  });
  const del = el("button", "chip-btn", "✕");
  del.type = "button";
  del.title = "remove this target";
  del.addEventListener("click", () => {
    route.targets.splice(i, 1);
    onChange();
  });
  add(c, up, down, del);

  c.addEventListener("dragstart", (e) => {
    c.classList.add("is-dragging");
    if (e.dataTransfer) e.dataTransfer.setData("text/plain", String(i));
  });
  c.addEventListener("dragend", () => c.classList.remove("is-dragging"));
  c.addEventListener("dragover", (e) => e.preventDefault());
  c.addEventListener("drop", (e) => {
    e.preventDefault();
    const from = Number(e.dataTransfer ? e.dataTransfer.getData("text/plain") : NaN);
    if (!isFinite(from) || from === i) return;
    const moved = route.targets.splice(from, 1)[0];
    route.targets.splice(i, 0, moved);
    onChange();
  });
  return c;
}

/** The chip row for one route. */
function targetChips(route, onChange) {
  const wrap = el("div", "rowline");
  if (!route.targets || !route.targets.length) {
    add(wrap, el("span", "muted", "no targets"));
  } else {
    for (let i = 0; i < route.targets.length; i += 1) add(wrap, targetChip(route, i, onChange));
  }
  return wrap;
}

/** `PUT /v1/routes/{alias}` — hot, no restart. */
async function saveRoute(route, quiet) {
  const r = await api("/v1/routes/" + encodeURIComponent(route.alias), { method: "PUT", body: route });
  if (!r.ok) {
    const issues = r.data && r.data.issues ? r.data.issues.map((i) => i.field + ": " + i.message).join("; ") : r.error;
    toast("route " + route.alias + " was refused — " + issues, "bad");
    return false;
  }
  if (!quiet) toast("route " + route.alias + " saved", "ok");
  return true;
}

/** The Routes panel. */
function renderRoutes(root) {
  const kids = [];
  kids.push(
    panelHead(
      "Routes",
      "an alias is the string a client puts in \"model\"",
      btn("New route", "＋", "btn-primary", () => openRouteEditor(null)),
    ),
  );

  const p = S.snap ? S.snap.proxy : null;
  if (p && p.table_valid === false) {
    const b = el("div", "banner banner-bad");
    add(
      b,
      badge("table did not compile", "⛔", "critical"),
      el("div", "stack"),
    );
    const stack = b.lastChild;
    add(stack, el("div", null, "The on-disk routing table failed to compile. The previous table is still serving."));
    add(stack, el("code", "mono", p.table_error || "no detail"));
    kids.push(b);
  }

  const routes = (S.snap && S.snap.routes) || [];
  if (!routes.length) {
    kids.push(el("div", "empty", "No routes yet. Launch an endpoint and bind an alias, or create a route."));
    root.replaceChildren(...kids);
    return;
  }

  const wrap = el("div", "tablewrap");
  const table = el("table");
  const thead = el("thead");
  const hr = el("tr");
  for (const h of ["alias", "targets", "strategy", "health", "p50 TTFT", "p50 tok/s", "$/Mtok", ""]) {
    add(hr, el("th", h === "p50 TTFT" || h === "p50 tok/s" || h === "$/Mtok" ? "num" : null, h));
  }
  add(thead, hr);
  add(table, thead);
  const tb = el("tbody");
  for (const route of routes) {
    const tr = el("tr");
    const aliasCell = el("td", "mono");
    add(aliasCell, el("span", null, route.alias));
    if (route.is_default) add(aliasCell, el("span", null, " "), badge("default", "★", null, "unknown and legacy model names land here"));
    add(tr, aliasCell);

    const tcell = el("td", "targets");
    add(
      tcell,
      targetChips(route, async () => {
        if (await saveRoute(route, true)) scheduleRender();
      }),
    );
    add(tr, tcell);

    add(tr, el("td", null, pretty(route.strategy)));
    const h = routeHealth(route);
    const hc = el("td");
    add(hc, badge(h.label, h.icon, h.tone, h.detail));
    add(tr, hc);

    const ttft = aliasP(route.alias, "ttft_ms");
    const tps = aliasP(route.alias, "tok_per_s");
    add(tr, el("td", "num", ttft === null ? "—" : Math.round(ttft) + " ms"));
    add(tr, el("td", "num", tps === null ? "—" : num(tps, 1)));
    const pc = el("td", "num");
    add(pc, routePrice(route));
    add(tr, pc);

    const ac = el("td");
    const acts = el("div", "rowline");
    add(acts, btn("Edit", "✎", "btn-sm", () => openRouteEditor(route)));
    add(
      acts,
      btn("Test", "⏱", "btn-sm", async () => {
        const r = await act("/v1/routes/" + encodeURIComponent(route.alias) + "/test", { method: "POST" });
        if (r.ok) {
          S.probes[route.alias] = r.data;
          scheduleRender();
        }
      }),
    );
    if (!route.is_default) {
      add(
        acts,
        btn("Make default", "★", "btn-sm", async () => {
          const r = await act("/v1/routes/default", { method: "POST", body: { alias: route.alias } }, route.alias + " is now the default");
          if (r.ok && S.snap) {
            S.snap.proxy.default_alias = route.alias;
            for (const x of S.snap.routes) x.is_default = x.alias === route.alias;
          }
          scheduleRender();
        }),
      );
    }
    add(ac, acts);
    add(tr, ac);
    add(tb, tr);

    const probe = S.probes[route.alias];
    if (probe) {
      const pr = el("tr");
      const td = el("td");
      td.colSpan = 8;
      const line = el("div", "rowline");
      add(line, badge(probe.ok ? "probe ok" : "probe failed", probe.ok ? "●" : "✕", probe.ok ? "good" : "critical"));
      add(line, el("span", "mono", probe.name));
      add(line, el("span", "muted", "TTFT " + (probe.ttft_ms === null ? "—" : probe.ttft_ms + " ms")));
      add(line, el("span", "muted", num(probe.tok_per_s, 2) + " tok/s"));
      add(line, el("span", "muted", (probe.tokens || 0) + " tokens in " + probe.ms + " ms"));
      add(line, el("span", "muted truncate", probe.detail || ""));
      add(td, line);
      add(pr, td);
      add(tb, pr);
    }
  }
  add(table, tb);
  add(wrap, table);
  kids.push(wrap);
  root.replaceChildren(...kids);
}

// ---------------------------------------------------------------------------------------
// panel: Backends
// ---------------------------------------------------------------------------------------

/** Does this backend occupy the physical device the rig strip selected? */
function onDevice(backend, key) {
  if (!key || !S.snap) return true;
  const dev = physicalDevices(S.snap.rig.gpus).find((d) => d.key === key);
  if (!dev) return true;
  const tokens = dev.views.map((v) => v.device);
  return (backend.devices || []).some((d) => tokens.indexOf(d) >= 0);
}

/** The Backends panel: one uniform card grid, however many there are. */
function renderBackends(root) {
  const kids = [];
  const controls = [];
  if (S.deviceFilter) {
    const chip = el("span", "chip");
    add(chip, el("span", null, "device: " + S.deviceFilter));
    const x = el("button", "chip-btn", "✕");
    x.type = "button";
    x.addEventListener("click", () => {
      S.deviceFilter = null;
      scheduleRender();
    });
    add(chip, x);
    controls.push(chip);
  }
  controls.push(btn("Register URL…", "＋", "btn-sm", () => openNodeEditor()));
  controls.push(btn("Launch…", "▶", "btn-primary", () => openLaunch()));
  kids.push(panelHead("Backends", "every live upstream, local or rented or managed", ...controls));

  const all = ((S.snap && S.snap.backends) || []).filter((b) => onDevice(b, S.deviceFilter));
  if (!all.length) {
    kids.push(el("div", "empty", "No backends match. Launch one, or register an OpenAI-compatible URL."));
    root.replaceChildren(...kids);
    return;
  }
  const grid = el("div", "grid");
  for (const b of all) add(grid, backendCard(b));
  kids.push(grid);
  root.replaceChildren(...kids);
}

/** One backend card. */
function backendCard(b) {
  const card = el("div", "card" + (b.enabled ? "" : " is-off"));
  const head = el("div", "card-head");
  const h = healthOf(b.health);
  add(head, el("span", "dot dot-" + h.tone));
  add(head, el("span", "card-title", b.label || b.id));
  add(head, badge(pretty(b.kind), "▤", null, "backend kind"));
  add(head, badge(b.protocol === "anthropic" ? "Anthropic" : "OpenAI", "⇄", null, "wire dialect this upstream speaks"));
  if (!b.enabled) add(head, badge("disabled", "⊘", null, "never a routing candidate"));
  add(card, head);
  add(card, el("div", "card-id", b.id + " · " + b.base_url));

  const line = el("div", "rowline");
  add(line, badge(h.label, h.icon, h.tone, h.detail));
  const ready = b.health && b.health.state === "ready";
  if (ready) {
    add(line, badge("slots " + b.health.slots_busy + "/" + b.health.slots_total, "▦", null, "busy / total"));
    if (b.health.tps_p50) add(line, badge(num(b.health.tps_p50, 1) + " tok/s", "⚡", null, "median observed throughput"));
    add(line, el("span", "muted", "up " + dur(Date.now() / 1000 - b.health.since_unix)));
  }
  add(card, line);

  const mine = S.requests.filter((r) => r.backend === b.id);
  const p50 = pct(mine.map((r) => r.total_ms), 50);
  const p95 = pct(mine.map((r) => r.total_ms), 95);
  const models = (b.models || []).map((m) => m.id);
  const price = priceText(b.price);
  add(
    card,
    kv([
      ["models", models.length ? models.slice(0, 3).join(", ") + (models.length > 3 ? " +" + (models.length - 3) : "") : "—"],
      ["queue", (b.limits.queue_depth || 0) + " deep · " + (b.limits.max_concurrent || 0) + " concurrent"],
      ["latency", p50 === null ? "—" : "p50 " + Math.round(p50) + " ms · p95 " + Math.round(p95) + " ms"],
      ["price", price || "—"],
      ["devices", (b.devices || []).join(", ") || "—"],
      ["ctx", b.limits.ctx ? b.limits.ctx.toLocaleString() : "—"],
      ["credential", credentialText(b.credential)],
      ["provenance", pretty(b.provenance)],
      ["tags", (b.tags || []).join(", ") || "—"],
    ]),
  );
  if (b.last_error) {
    const e = el("div", "banner banner-warn");
    add(e, badge("last error", "!", "warn"), el("span", "truncate", b.last_error));
    add(card, e);
  }

  const acts = el("div", "actions");
  add(acts, btn("Probe", "◎", "btn-sm", () => act("/v1/backends/" + b.id + "/probe", { method: "POST" }, "probed " + b.id)));
  add(acts, btn("Drain", "⇣", "btn-sm", () => act("/v1/backends/" + b.id + "/drain", { method: "POST" }, "draining " + b.id)));
  add(
    acts,
    b.enabled
      ? btn("Disable", "⊘", "btn-sm", () => act("/v1/backends/" + b.id + "/disable", { method: "POST" }, b.id + " disabled"))
      : btn("Enable", "✓", "btn-sm", () => act("/v1/backends/" + b.id + "/enable", { method: "POST" }, b.id + " enabled")),
  );
  add(acts, btn("Bind to alias…", "⇄", "btn-sm", () => openBindEditor(b)));
  add(acts, btn("Logs", "▤", "btn-sm", () => openLogs(b.id)));
  if (b.endpoint) {
    add(acts, btn("Stop", "■", "btn-sm", () => act("/v1/endpoints/" + b.endpoint.id + "/stop", { method: "POST" }, b.id + " stopping")));
    add(
      acts,
      btn("Destroy", "⛔", "btn-sm btn-danger", () =>
        confirmAction({
          title: "Destroy " + b.endpoint.id,
          danger: true,
          message: "Stop the process and forget the endpoint record.",
          lines: [
            ["backend", b.id],
            ["base url", b.base_url],
            ["kind", pretty(b.kind)],
          ],
          note: "A rented box is destroyed from Fleet & cost, where its accrued cost and your credit are on screen.",
          confirmLabel: "Stop and forget",
          run: () => act("/v1/endpoints/" + b.endpoint.id, { method: "DELETE" }, b.endpoint.id + " removed"),
        }),
      ),
    );
  }
  add(card, acts);
  return card;
}

// ---------------------------------------------------------------------------------------
// panel: Fleet & cost
// ---------------------------------------------------------------------------------------

/** The typed-confirmation plan for destroying a rented box. */
function destroyInstancePlan(id) {
  const inst = ((S.snap && S.snap.instances) || []).find((i) => i.id === id);
  const dph = inst && inst.dph_total ? inst.dph_total : null;
  const up = inst ? inst.uptime_secs || (inst.start_date ? Date.now() / 1000 - inst.start_date : null) : null;
  return {
    title: "Destroy instance " + id,
    danger: true,
    word: "destroy",
    lines: [
      ["instance", String(id) + (inst && inst.gpu_name ? " · " + (inst.num_gpus || 1) + "× " + inst.gpu_name : "")],
      ["rate", dph === null ? "unknown" : usd(dph, 3) + "/hr"],
      ["uptime", up === null ? "unknown" : dur(up)],
      ["accrued so far", dph === null || up === null ? "unknown" : usd((dph * up) / 3600) + " (approximate: rate × uptime)"],
      ["credit now", S.snap && S.snap.totals.vast_credit !== null ? usd(S.snap.totals.vast_credit) : "unknown"],
    ],
    confirmLabel: "Destroy and stop billing",
    run: () => act("/v1/vast/instances/" + id + "?confirm=true", { method: "DELETE" }, "destroy requested for " + id),
  };
}

/** The Fleet & cost panel: what is billing, what it costs, and how long the credit lasts. */
function renderFleet(root) {
  const kids = [];
  kids.push(
    panelHead(
      "Fleet & cost",
      "rented boxes bill by the second — nothing here spends without a typed confirmation",
      btn("Refresh", "⟳", "btn-sm", () => refreshFleet()),
      btn("Rent…", "＋", "btn-primary", () => openLaunch("rent")),
    ),
  );

  const t = (S.snap && S.snap.totals) || null;
  const tiles = el("div", "tiles");
  const spend24 = t ? costUsd(t.spend_24h) : null;
  const spend7 = t ? costUsd(t.spend_7d) : null;
  add(tiles, tile("24 h spend", spend24 === null ? "—" : usd(spend24), t ? honesty(t.spend_24h) : null));
  add(tiles, tile("7 d spend", spend7 === null ? "—" : usd(spend7), t ? honesty(t.spend_7d) : null));
  add(tiles, tile("burn rate", t ? money(t.burn_rate_usd_hr) + "/hr" : "—", "across every rented box"));
  add(tiles, tile("credit", t && t.vast_credit !== null && t.vast_credit !== undefined ? usd(t.vast_credit) : "—", "vast.ai"));
  add(
    tiles,
    tile(
      "burn-down",
      t && t.burn_down_hours !== null && t.burn_down_hours !== undefined ? dur(t.burn_down_hours * 3600) : "—",
      "credit ÷ burn rate",
    ),
  );
  add(tiles, tile("tokens 24 h", t ? Number(t.tokens_24h).toLocaleString() : "—", "prompt + completion"));
  kids.push(tiles);

  if (S.gone.vast) kids.push(unavailableNote("vast", "the vast.ai control-plane surface"));

  const instances = (S.snap && S.snap.instances) || [];
  // "We have a local record of this box" means an endpoint record OR a registered backend
  // names it. Anything else is billing with nobody watching it, and says so loudly.
  const known = new Set();
  for (const e of (S.snap && S.snap.endpoints) || []) {
    if (e.spec && e.spec.instance_id) known.add(e.spec.instance_id);
  }
  for (const b of (S.snap && S.snap.backends) || []) {
    const m = /(\d{4,})/.exec(b.id);
    if (m && b.endpoint) known.add(Number(m[1]));
  }
  if (!instances.length) {
    kids.push(el("div", "empty", "Nothing is rented. Local endpoints cost nothing per hour."));
  } else {
    const grid = el("div", "grid grid-wide");
    for (const i of instances) add(grid, instanceCard(i, known.has(i.id)));
    kids.push(grid);
  }

  const tunnels = (S.snap && S.snap.tunnels) || [];
  if (tunnels.length) {
    kids.push(el("h3", null, "SSH tunnels"));
    const grid = el("div", "grid");
    for (const tn of tunnels) {
      const c = el("div", "card");
      const head = el("div", "card-head");
      add(head, el("span", "card-title", "instance " + tn.spec.instance_id));
      add(head, badge(tn.up ? "up" : "down", tn.up ? "●" : "✕", tn.up ? "good" : "critical"));
      add(c, head);
      add(
        c,
        kv([
          ["local", "127.0.0.1:" + tn.spec.local_port],
          ["remote", tn.spec.ssh_host + ":" + tn.spec.ssh_port + " → " + tn.spec.remote_port],
          ["restarts", tn.restarts],
          ["since", tn.since_unix ? ago(tn.since_unix) : "—"],
          ["last error", tn.last_error || null],
        ]),
      );
      const acts = el("div", "actions");
      add(
        acts,
        tn.up
          ? btn("Close tunnel", "✕", "btn-sm", () =>
              act("/v1/vast/instances/" + tn.spec.instance_id + "/tunnel", { method: "DELETE" }, "tunnel closed"),
            )
          : btn("Open tunnel", "⇄", "btn-sm", () =>
              act("/v1/vast/instances/" + tn.spec.instance_id + "/tunnel", { method: "POST" }, "tunnel opening"),
            ),
      );
      add(c, acts);
      add(grid, c);
    }
    kids.push(grid);
  }

  const jobs = ((S.snap && S.snap.jobs) || []).filter((j) => j.state === "running" || j.state === "pending");
  if (jobs.length) {
    kids.push(el("h3", null, "Jobs in flight"));
    const grid = el("div", "grid");
    for (const j of jobs) add(grid, jobCard(j));
    kids.push(grid);
  }
  root.replaceChildren(...kids);
}

/** A stat tile with a caption and a footnote. */
function tile(caption, value, note) {
  const t = el("div", "tile");
  add(t, el("span", "stat-k", caption), el("span", "big", value));
  if (note) add(t, el("span", "hint", note));
  return t;
}

/** "metered" / "approximate — <assumption>", for a cost tile's footnote. */
function honesty(c) {
  if (!c || c.kind === "unknown") return "no price known";
  if (c.kind === "metered") return "metered";
  return "approximate — " + (c.assumption || "derived");
}

/** One rented box. */
function instanceCard(i, adopted) {
  const card = el("div", "card");
  const head = el("div", "card-head");
  const phase = instancePhase(i);
  add(head, el("span", "dot dot-" + phase.tone));
  add(head, el("span", "card-title", (i.label || "instance") + " · " + i.id));
  add(head, badge(phase.label, phase.icon, phase.tone, i.status_msg || ""));
  if (!adopted) add(head, badge("no local record", "⚠", "critical", "this box is billing and ApexRouter has no endpoint for it"));
  add(card, head);

  const up = i.uptime_secs || (i.start_date ? Date.now() / 1000 - i.start_date : null);
  const accrued = i.dph_total && up ? (i.dph_total * up) / 3600 : null;
  add(
    card,
    kv([
      ["gpu", (i.num_gpus || 1) + "× " + (i.gpu_name || "?")],
      ["rate", i.dph_total ? usd(i.dph_total, 3) + "/hr" : "—"],
      ["uptime", up ? dur(up) : "—"],
      ["accrued", accrued === null ? "—" : usd(accrued) + " (approx: rate × uptime)"],
      ["where", i.geolocation || "—"],
      ["ssh", i.ssh_host ? i.ssh_host + ":" + (i.ssh_port || "?") : "—"],
      ["disk", i.disk_space ? num(i.disk_util || 0, 1) + " / " + num(i.disk_space, 0) + " GB" : "—"],
      ["down", i.inet_down ? num(i.inet_down, 0) + " Mbps" : "—"],
    ]),
  );

  // The daemon raises the stall as an alert; the banner is the same fact, where the box is.
  const stalled = ((S.snap && S.snap.alerts) || []).some(
    (a) => a.id.indexOf("download.stall") === 0 && a.message.indexOf(String(i.id)) >= 0,
  );
  if (phase.stalled || stalled) {
    const b = el("div", "banner banner-warn");
    add(b, badge("download stalled", "⏸", "warn"), el("span", null, "The weights stopped arriving."));
    add(
      b,
      btn("Restart download", "⟳", "btn-sm", () =>
        act("/v1/vast/instances/" + i.id + "/restart-download", { method: "POST" }, "download restarted"),
      ),
    );
    add(card, b);
  }

  const acts = el("div", "actions");
  add(acts, btn("Logs", "▤", "btn-sm", () => openInstanceLogs(i.id)));
  add(
    acts,
    btn("Diagnose", "◎", "btn-sm", async () => {
      const r = await act("/v1/vast/instances/" + i.id + "/diagnose", { method: "GET" });
      if (r.ok && Array.isArray(r.data)) {
        S.checks = r.data;
        show("doctor");
      }
    }),
  );
  add(acts, btn("Tunnel", "⇄", "btn-sm", () => act("/v1/vast/instances/" + i.id + "/tunnel", { method: "POST" }, "tunnel opening")));
  add(acts, btn("Destroy…", "⛔", "btn-sm btn-danger", () => confirmMoney(destroyInstancePlan(i.id))));
  add(card, acts);
  return card;
}

/** A `VastInstance`'s phase, as a health-toned badge. */
function instancePhase(i) {
  const st = (i.actual_status || "").toLowerCase();
  if (st === "running") return { tone: "good", icon: "●", label: "Running" };
  if (st === "loading" || st === "pulling") return { tone: "warn", icon: "◐", label: "Pulling", stalled: false };
  if (st === "created" || st === "scheduling" || st === "starting") return { tone: "warn", icon: "◐", label: "Provisioning" };
  if (st === "stopped" || st === "inactive") return { tone: "unknown", icon: "○", label: "Stopped" };
  if (st === "exited" || st === "offline" || st === "unknown") return { tone: "critical", icon: "✕", label: pretty(st) };
  return { tone: "unknown", icon: "○", label: st ? pretty(st) : "Reserved" };
}

/** A background job, with its progress. */
function jobCard(j) {
  const c = el("div", "card");
  const head = el("div", "card-head");
  add(head, el("span", "card-title", j.kind));
  const tone = j.state === "failed" ? "critical" : j.state === "succeeded" ? "good" : "warn";
  add(head, badge(pretty(j.state), j.state === "failed" ? "✕" : j.state === "succeeded" ? "●" : "◐", tone));
  add(c, head);
  add(c, el("div", "card-id", j.id));
  if (j.message) add(c, el("div", "muted truncate", j.message));
  if (j.pct !== null && j.pct !== undefined) {
    const m = el("div", "meter");
    const f = el("span");
    f.style.width = Math.max(0, Math.min(100, j.pct)) + "%";
    add(m, f);
    add(c, m, el("div", "hint", num(j.pct, 0) + "%"));
  }
  if (j.error) add(c, el("div", "muted", j.error));
  const acts = el("div", "actions");
  add(acts, btn("Cancel", "✕", "btn-sm", () => act("/v1/jobs/" + j.id + "/cancel", { method: "POST" }, "cancel requested")));
  add(c, acts);
  return c;
}

/** Re-read the fleet: the snapshot carries it, so this is a reconcile button too. */
async function refreshFleet() {
  const r = await api("/v1/vast/instances");
  if (r.ok && Array.isArray(r.data) && S.snap) S.snap.instances = r.data;
  await firstPaint();
  scheduleRender();
}

// ---------------------------------------------------------------------------------------
// panel: Catalog — recipes, search profiles, local models, HF
// ---------------------------------------------------------------------------------------

/** The Catalog panel. This is "dynamic recipe building in the GUI". */
function renderCatalog(root) {
  const kids = [];
  kids.push(
    panelHead(
      "Catalog",
      "recipes are saved launch plans; profiles are saved market queries",
      btn("New recipe", "＋", "btn-sm", () => openRecipeEditor(null)),
      btn("New profile", "＋", "btn-sm", () => openProfileEditor(null)),
      btn("Rescan models", "⟳", "btn-sm", async () => {
        await loadLocalModels(true);
        toast("model roots re-walked", "ok");
      }),
    ),
  );

  const recipes = (S.snap && S.snap.recipes) || [];
  kids.push(el("h3", null, "Recipes (" + recipes.length + ")"));
  if (!recipes.length) {
    kids.push(el("div", "empty", "No recipes yet. Save one from the Launch drawer, or create one here."));
  } else {
    const grid = el("div", "grid");
    for (const r of recipes) add(grid, recipeCard(r));
    kids.push(grid);
  }

  const profiles = (S.snap && S.snap.profiles) || [];
  kids.push(el("h3", null, "Search profiles (" + profiles.length + ")"));
  if (!profiles.length) {
    kids.push(el("div", "empty", "No search profiles. A profile is a saved vast.ai query: GPU names, price ceiling, geography."));
  } else {
    const grid = el("div", "grid");
    for (const p of profiles) add(grid, profileCard(p));
    kids.push(grid);
  }

  kids.push(el("h3", null, "Local models (" + S.localModels.length + ")"));
  kids.push(localModelTable());

  kids.push(el("h3", null, "Hugging Face"));
  kids.push(hfSection());
  root.replaceChildren(...kids);
}

/** Staleness of a recipe, as findings rather than errors. */
function recipeStaleness(r) {
  const out = [];
  const k = r.kind || {};
  if (k.kind === "local") {
    const builds = ((S.snap && S.snap.rig && S.snap.rig.builds) || []).map((b) => b.id);
    if (k.build && builds.indexOf(k.build) < 0) out.push("build " + k.build + " is not on this machine");
    const paths = S.localModels.map((m) => m.dir);
    if (k.model_path && S.localModels.length && !paths.some((p) => k.model_path.indexOf(p) === 0)) {
      out.push("the model file is not under a known model root");
    }
  }
  if (k.kind === "vast") {
    const profiles = ((S.snap && S.snap.profiles) || []).map((p) => p.id);
    if (k.profile && profiles.indexOf(k.profile) < 0) out.push("search profile " + k.profile + " is gone");
  }
  return out;
}

/** One recipe card. */
function recipeCard(r) {
  const c = el("div", "card");
  const head = el("div", "card-head");
  add(head, el("span", "card-title", r.label));
  add(head, badge(pretty(r.kind.kind), "▤", null, "recipe kind"));
  add(c, head);
  add(c, el("div", "card-id", r.id));
  if (r.description) add(c, el("div", "muted", r.description));
  const k = r.kind;
  add(
    c,
    kv([
      ["model", k.model_path || k.model_id || (k.launch && k.launch.image) || "—"],
      ["build", k.build || null],
      ["ctx", k.ctx ? k.ctx.toLocaleString() : null],
      ["profile", k.profile || null],
      ["source", r.provenance ? r.provenance.source : null],
      ["updated", ago(r.updated_at_unix)],
    ]),
  );
  const stale = recipeStaleness(r);
  for (const s of stale) {
    const b = el("div", "banner banner-warn");
    add(b, badge("stale", "!", "warn"), el("span", null, s));
    add(c, b);
  }
  const acts = el("div", "actions");
  add(
    acts,
    btn("Run", "▶", "btn-sm btn-primary", () =>
      promptDrawer({
        title: "Run " + r.label,
        label: "bind alias (blank binds nothing)",
        message: "The recipe is instantiated in the background; the Launch drawer shows its boot phases.",
        value: "",
        allowEmpty: true,
        confirmLabel: "Run",
        run: async (alias) => {
          const q = alias ? "?alias=" + encodeURIComponent(slug(alias, "auto")) + "&no_wait=true" : "?no_wait=true";
          await act("/v1/recipes/" + r.id + "/instantiate" + q, { method: "POST" }, "starting " + r.label);
        },
      }),
    ),
  );
  add(acts, btn("Edit", "✎", "btn-sm", () => openRecipeEditor(r)));
  add(
    acts,
    btn("Duplicate", "⧉", "btn-sm", () => {
      const copy = JSON.parse(JSON.stringify(r));
      copy.label = r.label + " copy";
      copy.id = slug(copy.label, "recipe");
      openRecipeEditor(copy, true);
    }),
  );
  add(
    acts,
    btn("Validate", "◎", "btn-sm", async () => {
      const v = await act("/v1/recipes/" + r.id + "/validate", { method: "POST" });
      if (v.ok) showReport(r.label, v.data);
    }),
  );
  add(
    acts,
    btn("Delete", "✕", "btn-sm btn-danger", () =>
      confirmAction({
        title: "Delete recipe " + r.label,
        danger: true,
        message: "The recipe is removed from the catalog. Nothing running is affected.",
        lines: [["id", r.id], ["kind", pretty(r.kind.kind)]],
        confirmLabel: "Delete",
        run: async () => {
          const d = await act("/v1/recipes/" + r.id, { method: "DELETE" }, "deleted " + r.label);
          if (d.ok && S.snap) {
            S.snap.recipes = S.snap.recipes.filter((x) => x.id !== r.id);
            scheduleRender();
          }
        },
      }),
    ),
  );
  add(c, acts);
  return c;
}

/** A `ValidationReport` in the editor drawer. */
function showReport(title, report) {
  openEditor("Validation — " + title, (body) => {
    const kids = [];
    kids.push(
      badge(report.ok ? "no blocking issues" : "blocked", report.ok ? "●" : "✕", report.ok ? "good" : "critical"),
    );
    for (const i of report.issues || []) {
      const c = el("div", "card");
      const head = el("div", "card-head");
      const tone = i.severity === "error" ? "critical" : i.severity === "warning" ? "warn" : null;
      add(head, badge(pretty(i.severity), i.severity === "error" ? "✕" : i.severity === "warning" ? "!" : "ⓘ", tone));
      add(head, el("span", "card-id", i.field));
      add(c, head, el("div", null, i.message));
      if (i.fix) add(c, el("div", "muted", "fix: " + i.fix));
      kids.push(c);
    }
    if (!(report.issues || []).length) kids.push(el("div", "muted", "Nothing to report."));
    body.replaceChildren(...kids);
  });
}

/** One search-profile card. */
function profileCard(p) {
  const c = el("div", "card");
  const head = el("div", "card-head");
  add(head, el("span", "card-title", p.label));
  add(head, badge(pretty(p.image_type), "▤", null, "container image family"));
  add(c, head);
  add(c, el("div", "card-id", p.id));
  add(
    c,
    kv([
      ["gpus", (p.gpu_names || []).join(", ") || "any"],
      ["count", p.num_gpus_min + "–" + p.num_gpus_max],
      ["max $/hr", p.max_dph === null || p.max_dph === undefined ? "—" : money(p.max_dph)],
      ["reliability", "≥ " + num(p.min_reliability, 2)],
      ["down", "≥ " + p.min_inet_down + " Mbps"],
      ["disk", "≥ " + p.min_disk_gb + " GB"],
      ["cuda", p.min_cuda ? "≥ " + num(p.min_cuda, 1) : "any"],
      ["geo", typeof p.geo === "string" ? pretty(p.geo) : (p.geo.codes || []).join(", ")],
    ]),
  );
  const acts = el("div", "actions");
  add(
    acts,
    btn("Search offers", "◎", "btn-sm", () => {
      openLaunch("rent");
      S.launch.rent.profile = p.id;
      renderLaunch();
      searchOffers();
    }),
  );
  add(acts, btn("Edit", "✎", "btn-sm", () => openProfileEditor(p)));
  add(
    acts,
    btn("Delete", "✕", "btn-sm btn-danger", () =>
      confirmAction({
        title: "Delete profile " + p.label,
        danger: true,
        message: "The saved market query is removed. Nothing rented is affected.",
        lines: [["id", p.id]],
        confirmLabel: "Delete",
        run: async () => {
          const d = await act("/v1/profiles/" + p.id, { method: "DELETE" }, "deleted " + p.label);
          if (d.ok && S.snap) {
            S.snap.profiles = S.snap.profiles.filter((x) => x.id !== p.id);
            scheduleRender();
          }
        },
      }),
    ),
  );
  add(c, acts);
  return c;
}

/** Discovered GGUFs: shards grouped, real sizes, a vision badge where there is an mmproj. */
function localModelTable() {
  if (!S.localModels.length) {
    return el("div", "empty", "No local models discovered under the configured model roots.");
  }
  const wrap = el("div", "tablewrap");
  const table = el("table");
  const thead = el("thead");
  const hr = el("tr");
  for (const h of ["model", "quant", "size", "shards", "arch", "ctx train", ""]) {
    add(hr, el("th", h === "size" || h === "shards" || h === "ctx train" ? "num" : null, h));
  }
  add(thead, hr);
  add(table, thead);
  const tb = el("tbody");
  for (const m of S.localModels) {
    const tr = el("tr");
    const name = el("td");
    add(name, el("span", null, m.name));
    if ((m.mmproj || []).length) add(name, el("span", null, " "), badge("vision", "◉", null, "an mmproj sits beside the weights"));
    add(name, el("div", "card-id truncate", m.dir));
    add(tr, name);
    add(tr, el("td", "mono", m.quant || "—"));
    add(tr, el("td", "num", bytes(m.total_bytes)));
    add(tr, el("td", "num", String((m.shards || []).length)));
    add(tr, el("td", "mono", m.gguf ? m.gguf.arch : "—"));
    add(tr, el("td", "num", m.gguf ? m.gguf.n_ctx_train.toLocaleString() : "—"));
    const ac = el("td");
    add(
      ac,
      btn("Launch…", "▶", "btn-sm", () => {
        openLaunch("local");
        S.launch.local.model = m.id;
        S.launch.fitInput = null;
        renderLaunch();
        solveFit();
      }),
    );
    add(tr, ac);
    add(tb, tr);
  }
  add(table, tb);
  add(wrap, table);
  return wrap;
}

/** HF search with per-file sizes and a Download button. Debounced 250 ms, seq-guarded. */
function hfSection() {
  const box = el("div", "stack");
  const row = el("div", "split");
  const q = input("search", S.hf.q, hfSearch, { placeholder: "search Hugging Face for GGUF repos…", size: "40" });
  add(row, q, btn("Search", "◎", "btn-sm", () => hfSearch(q.value)));
  add(box, row);
  if (S.gone.hf) {
    add(box, unavailableNote("hf", "the Hugging Face surface"));
    return box;
  }
  if (!S.hf.rows.length) {
    add(box, el("div", "hint", "Type at least two characters. Sizes come from the paths-info API, so they are authoritative."));
    return box;
  }
  const wrap = el("div", "tablewrap");
  const table = el("table");
  const tb = el("tbody");
  for (const m of S.hf.rows) {
    const tr = el("tr");
    const c = el("td");
    add(c, el("span", "mono", m.id));
    if (m.gated) add(c, el("span", null, " "), badge("gated", "🔒", "warn", "needs an accepted licence"));
    add(tr, c);
    add(tr, el("td", "num", m.downloads ? Number(m.downloads).toLocaleString() : "—"));
    add(tr, el("td", "num", m.likes ? "♥ " + m.likes : "—"));
    const ac = el("td");
    add(
      ac,
      btn("Files", "▤", "btn-sm", async () => {
        const r = await api("/v1/hf/models/" + m.id + "/files");
        if (r.ok) {
          S.hf.files[m.id] = r.data || [];
          scheduleRender();
        }
      }),
    );
    add(tr, ac);
    add(tb, tr);
    for (const f of S.hf.files[m.id] || []) {
      const fr = el("tr");
      const fc = el("td");
      fc.colSpan = 3;
      const line = el("div", "rowline");
      add(line, el("span", "mono", f.rfilename));
      if (f.quant) add(line, badge(f.quant, "▦", null, "quantisation"));
      if (f.is_mmproj) add(line, badge("mmproj", "◉", null, "vision projector"));
      add(line, el("span", "muted", bytes(f.size)));
      add(fc, line);
      add(fr, fc);
      const dc = el("td");
      add(
        dc,
        btn("Download", "⇣", "btn-sm", () =>
          act("/v1/hf/downloads?no_wait=true", { method: "POST", body: { repo: m.id, files: [f.rfilename] } }, "download queued"),
        ),
      );
      add(fr, dc);
      add(tb, fr);
    }
  }
  add(table, tb);
  add(wrap, table);
  add(box, wrap);
  return box;
}

/** Debounced HF search with a monotonic guard, so a slow answer never overwrites a fast one. */
const hfSearch = debounced("hf", 250, async (seq, value) => {
  S.hf.q = value === undefined ? S.hf.q : value;
  if (!S.hf.q || S.hf.q.length < 2) {
    S.hf.rows = [];
    scheduleRender();
    return;
  }
  const r = await api("/v1/hf/search?limit=20&q=" + encodeURIComponent(S.hf.q));
  if (!fresh("hf", seq)) return; // a stale answer, dropped
  S.hf.rows = r.ok && Array.isArray(r.data) ? r.data : [];
  scheduleRender();
});

// ---------------------------------------------------------------------------------------
// panel: Providers
// ---------------------------------------------------------------------------------------

/** The Providers panel: source, never value; a masked field; a live catalogue. */
function renderProviders(root) {
  const kids = [];
  kids.push(panelHead("Providers", "credentials are shown by SOURCE, never by value"));
  if (S.gone.providers) kids.push(unavailableNote("providers", "the provider surface"));

  const providers = (S.snap && S.snap.providers) || [];
  if (!providers.length) {
    kids.push(el("div", "empty", "No managed providers are configured. Add one under [providers] in config.toml."));
    root.replaceChildren(...kids);
    return;
  }
  const grid = el("div", "grid grid-wide");
  for (const p of providers) add(grid, providerCard(p));
  kids.push(grid);
  root.replaceChildren(...kids);
}

/** One provider card. */
function providerCard(p) {
  const c = el("div", "card");
  const head = el("div", "card-head");
  add(head, el("span", "card-title", p.id));
  add(
    head,
    p.credential_present
      ? badge("credential found", "🔑", "good", "source: " + credentialText(p.credential))
      : badge("no credential", "✕", "critical", "the whole resolution chain came up empty"),
  );
  add(c, head);
  add(
    c,
    kv([
      ["base url", p.base_url],
      ["source", credentialText(p.credential)],
      ["models cached", p.models_cached],
      ["last ok", p.last_ok_unix ? ago(p.last_ok_unix) : "never"],
      ["last error", p.last_error || null],
    ]),
  );

  const form = el("div", "fields");
  const base = input("text", p.base_url, null, { placeholder: "https://api.together.xyz" });
  const key = input("password", "", null, { placeholder: p.credential_present ? "•••••••• (leave blank to keep)" : "paste an API key" });
  key.autocomplete = "off";
  add(form, field("base URL", base), field("API key (written to credentials.toml at 0600 — never config.toml)", key));
  add(c, form);

  const acts = el("div", "actions");
  add(
    acts,
    btn("Save", "✓", "btn-sm", async () => {
      const body = {};
      if (base.value && base.value !== p.base_url) body.base_url = base.value;
      if (key.value) body.api_key = key.value;
      if (!Object.keys(body).length) {
        toast("nothing changed", "info");
        return;
      }
      const r = await act("/v1/providers/" + p.id, { method: "PUT", body: body }, p.id + " updated");
      if (r.ok) key.value = "";
    }),
  );
  add(
    acts,
    btn("Test", "◎", "btn-sm", async () => {
      const r = await act("/v1/providers/" + p.id + "/test", { method: "POST" });
      if (r.ok) showChecks("Provider test — " + p.id, r.data || []);
    }),
  );
  add(
    acts,
    btn("Load catalogue", "▤", "btn-sm", async () => {
      const r = await act("/v1/providers/" + p.id + "/models", { method: "GET" });
      if (r.ok) {
        S.providerModels[p.id] = r.data || [];
        scheduleRender();
      }
    }),
  );
  add(c, acts);

  const models = S.providerModels[p.id];
  if (models && models.length) {
    const byOrg = new Map();
    for (const m of models) {
      const org = m.id.indexOf("/") > 0 ? m.id.split("/")[0] : "other";
      if (!byOrg.has(org)) byOrg.set(org, []);
      byOrg.get(org).push(m);
    }
    for (const org of Array.from(byOrg.keys()).sort()) {
      add(c, el("h4", "muted", org));
      const wrap = el("div", "tablewrap");
      const table = el("table");
      const tb = el("tbody");
      for (const m of byOrg.get(org)) {
        const tr = el("tr");
        const t = el("td", "mono", m.id);
        add(tr, t);
        const caps = el("td");
        if (m.vision) add(caps, badge("vision", "◉", null, "accepts image blocks"));
        if (m.tools) add(caps, badge("tools", "⚒", null, "accepts tools / emits tool_calls"));
        if (m.ctx) add(caps, badge(m.ctx.toLocaleString() + " ctx", "▦", null, "advertised context"));
        add(tr, caps);
        const ac = el("td");
        const line = el("div", "rowline");
        add(
          line,
          btn("Activate", "▶", "btn-sm", () => activateManaged(p, m.id)),
        );
        add(
          line,
          btn("Save as recipe", "✎", "btn-sm", () => {
            openRecipeEditor(
              {
                id: slug(p.id + "-" + m.id.split("/").pop(), "recipe"),
                label: p.id + " " + m.id.split("/").pop(),
                description: "managed model on " + p.id,
                kind: { kind: "managed", provider: p.id, base_url: p.base_url, credential: p.credential, model_id: m.id, protocol: "open_ai" },
                provenance: { discovered_at_unix: Math.floor(Date.now() / 1000), source: "providers panel", size_bytes: null, fit: null },
                created_at_unix: Math.floor(Date.now() / 1000),
                updated_at_unix: Math.floor(Date.now() / 1000),
              },
              true,
            );
          }),
        );
        add(ac, line);
        add(tr, ac);
        add(tb, tr);
      }
      add(table, tb);
      add(wrap, table);
      add(c, wrap);
    }
  }
  return c;
}

/** Register a managed model as an endpoint and bind an alias to it. */
function activateManaged(p, modelId) {
  promptDrawer({
    title: "Activate " + modelId,
    label: "bind alias (blank registers it without an alias)",
    message: "The model is registered as a managed endpoint on " + p.id + ". No key is copied anywhere: the credential stays where it lives.",
    value: slug(modelId.split("/").pop(), "managed"),
    allowEmpty: true,
    confirmLabel: "Activate",
    run: async (alias) => {
      const spec = {
        kind: "managed",
        provider: p.id,
        base_url: p.base_url,
        credential: p.credential,
        model_id: modelId,
        protocol: "open_ai",
      };
      const q = alias ? "?alias=" + encodeURIComponent(slug(alias, "managed")) : "";
      await act("/v1/endpoints" + q, { method: "POST", body: spec }, modelId + " activated");
    },
  });
}

/** A `CheckResult[]` in the editor drawer. */
function showChecks(title, results) {
  openEditor(title, (body) => {
    const kids = [];
    for (const r of results) kids.push(checkRow(r, false));
    if (!results.length) kids.push(el("div", "muted", "No results."));
    body.replaceChildren(...kids);
  });
}

// ---------------------------------------------------------------------------------------
// panel: Live requests
// ---------------------------------------------------------------------------------------

/** The Live requests panel: the WS stream, plus whatever the ring already held. */
function renderRequests(root) {
  const kids = [];
  kids.push(
    panelHead(
      "Live requests",
      S.inflight.size + " in flight · " + S.requests.length + " finished this session",
      btn("Reload", "⟳", "btn-sm", () => loadPanel("requests", true)),
    ),
  );
  const note = el("div", "banner banner-note");
  add(
    note,
    badge("prompts not captured", "🔒", null, "capture_bodies"),
    el(
      "span",
      null,
      "Prompt and completion bodies are never shown here unless [telemetry] capture_bodies is on in config.toml. It is off unless you turned it on.",
    ),
  );
  kids.push(note);

  const wrap = el("div", "tablewrap");
  const table = el("table");
  const thead = el("thead");
  const hr = el("tr");
  for (const h of ["time", "alias → backend", "model", "status", "TTFT", "tok/s", "tokens", "cost", "att", "reason", ""]) {
    add(hr, el("th", ["TTFT", "tok/s", "tokens", "att"].indexOf(h) >= 0 ? "num" : null, h));
  }
  add(thead, hr);
  add(table, thead);
  const tb = el("tbody");

  for (const f of S.inflight.values()) {
    const tr = el("tr");
    add(tr, el("td", "mono", clock(f.at)));
    add(tr, el("td", "mono", (f.alias || "—") + " → " + (f.backend || "…")));
    add(tr, el("td", "muted", "—"));
    const st = el("td");
    add(st, badge("in flight", "◐", "warn", "started " + dur(Date.now() / 1000 - f.at) + " ago"));
    add(tr, st);
    for (let i = 0; i < 5; i += 1) add(tr, el("td", "num", "—"));
    add(tr, el("td", "muted", "—"));
    const ac = el("td");
    add(ac, btn("Cancel", "✕", "btn-sm", () => act("/v1/requests/" + f.id + "/cancel", { method: "POST" }, "cancel requested")));
    add(tr, ac);
    add(tb, tr);
  }

  for (const r of S.requests.slice(0, 200)) {
    const tr = el("tr", "is-clickable");
    tr.addEventListener("click", () => showRequest(r));
    add(tr, el("td", "mono", clock(r.started_unix)));
    add(tr, el("td", "mono", (r.alias || "—") + " → " + (r.backend || "—")));
    add(tr, el("td", "mono truncate", r.upstream_model || "—"));
    const st = el("td");
    const ok = r.status >= 200 && r.status < 400;
    add(st, badge(String(r.status), ok ? "●" : "✕", ok ? "good" : "critical", r.error || ""));
    if (r.streamed) add(st, badge("stream", "≋", null, "server-sent events"));
    add(tr, st);
    add(tr, el("td", "num", r.ttft_ms === null || r.ttft_ms === undefined ? "—" : r.ttft_ms + " ms"));
    add(tr, el("td", "num", num(r.tok_per_s, 1)));
    const tk = el("td", "num");
    add(tk, tokensNode(r.completion_tokens));
    add(tr, tk);
    const co = el("td");
    add(co, costNode(r.cost));
    add(tr, co);
    add(tr, el("td", "num", String(r.attempts)));
    add(tr, el("td", "mono", r.route_reason));
    add(tr, el("td", null, ""));
    add(tb, tr);
  }
  add(table, tb);
  add(wrap, table);
  kids.push(S.requests.length || S.inflight.size ? wrap : el("div", "empty", "No requests yet. Point a client at the base URL above."));
  root.replaceChildren(...kids);
}

/** One request, in full, in the editor drawer. */
function showRequest(r) {
  openEditor("Request " + r.id, (body) => {
    body.replaceChildren(
      kv([
        ["started", clock(r.started_unix) + " (" + ago(r.started_unix) + ")"],
        ["alias", r.alias || "—"],
        ["backend", r.backend || "—"],
        ["upstream model", r.upstream_model || "—"],
        ["route reason", r.route_reason],
        ["ingress", r.ingress],
        ["method", r.method + " " + r.path],
        ["status", String(r.status)],
        ["attempts", String(r.attempts)],
        ["streamed", String(r.streamed)],
        ["aborted", String(r.aborted)],
        ["TTFT", r.ttft_ms === null || r.ttft_ms === undefined ? "—" : r.ttft_ms + " ms"],
        ["total", r.total_ms + " ms"],
        ["prompt tokens", tokensNode(r.prompt_tokens)],
        ["completion tokens", tokensNode(r.completion_tokens)],
        ["cached tokens", r.cached_tokens === null || r.cached_tokens === undefined ? "—" : String(r.cached_tokens)],
        ["tok/s", num(r.tok_per_s, 2)],
        ["cost", costNode(r.cost)],
        ["error", r.error || null],
      ]),
    );
  });
}

// ---------------------------------------------------------------------------------------
// panel: Usage — hand-rolled inline SVG, no CDN
// ---------------------------------------------------------------------------------------

/** Fetch the three groupings the panel draws. */
async function loadUsage() {
  const since = S.usage.since;
  const [main, day, backend] = await Promise.all([
    api("/v1/usage?since=" + encodeURIComponent(since) + "&by=" + encodeURIComponent(S.usage.by)),
    api("/v1/usage?since=" + encodeURIComponent(since) + "&by=day"),
    api("/v1/usage?since=" + encodeURIComponent(since) + "&by=backend"),
  ]);
  S.usage.summary = main.ok ? main.data : null;
  S.usage.day = day.ok ? day.data : null;
  S.usage.backend = backend.ok ? backend.data : null;
}

/** The Usage panel. */
function renderUsage(root) {
  const kids = [];
  const sinceSel = select(
    [
      ["1h", "last hour"],
      ["24h", "last 24 hours"],
      ["7d", "last 7 days"],
      ["30d", "last 30 days"],
      ["all", "all time"],
    ],
    S.usage.since,
    async (v) => {
      S.usage.since = v;
      await loadUsage();
      scheduleRender();
    },
  );
  const bySel = select(
    [
      ["provider", "by provider"],
      ["model", "by model"],
      ["backend", "by backend"],
      ["alias", "by alias"],
      ["day", "by day"],
    ],
    S.usage.by,
    async (v) => {
      S.usage.by = v;
      await loadUsage();
      scheduleRender();
    },
  );
  kids.push(panelHead("Usage", "from the append-only usage log", sinceSel, bySel));

  const u = S.usage.summary;
  if (!u) {
    kids.push(el("div", "empty", "No usage data for this window."));
    root.replaceChildren(...kids);
    return;
  }
  const tiles = el("div", "tiles");
  const total = costUsd(u.total_cost);
  add(tiles, tile("cost", total === null ? "—" : usd(total), honesty(u.total_cost)));
  add(tiles, tile("prompt tokens", Number(u.total_prompt).toLocaleString(), "sent"));
  add(tiles, tile("completion tokens", Number(u.total_completion).toLocaleString(), "generated"));
  add(tiles, tile("requests", Number(u.rows).toLocaleString(), "rows in the window " + u.window));
  kids.push(tiles);

  if (S.usage.day && (S.usage.day.by || []).length) {
    kids.push(
      stackedChart("Tokens per day", S.usage.day.by, (b) => [
        { value: b.prompt_tokens, cls: "bar-a", label: "prompt" },
        { value: b.completion_tokens, cls: "bar-b", label: "completion" },
      ]),
    );
    kids.push(
      stackedChart("Spend per day", S.usage.day.by, (b) => [{ value: (costUsd(b.cost) || 0) * 100, cls: "bar-a", label: "cents" }], (v) =>
        usd(v / 100),
      ),
    );
  }
  kids.push(groupTable("Breakdown " + bySel.value, u.by || []));
  if (S.usage.backend && (S.usage.backend.by || []).length) {
    kids.push(
      hbarChart(
        "Median tok/s by backend",
        S.usage.backend.by.filter((b) => b.tok_per_s_p50).map((b) => ({ label: b.key, value: b.tok_per_s_p50 })),
        (v) => num(v, 1) + " tok/s",
      ),
    );
  }
  root.replaceChildren(...kids);
}

/** A stacked column chart, drawn by hand. */
function stackedChart(title, buckets, parts, fmt) {
  const box = el("div", "chart");
  add(box, el("h3", null, title));
  const rows = buckets.slice(-30);
  const totals = rows.map((b) => parts(b).reduce((a, p) => a + (p.value || 0), 0));
  const max = Math.max(1, ...totals);
  const w = Math.max(720, rows.length * 48);
  const h = 200;
  const pad = { l: 54, r: 8, t: 8, b: 30 };
  const svg = svgEl("svg", { viewBox: "0 0 " + w + " " + h, width: w, height: h, role: "img" });
  svg.setAttribute("aria-label", title);
  for (let i = 0; i <= 4; i += 1) {
    const y = pad.t + ((h - pad.t - pad.b) * i) / 4;
    add(svg, svgEl("line", { x1: pad.l, y1: y, x2: w - pad.r, y2: y, class: "grid-line" }));
    const lab = svgEl("text", { x: 4, y: y + 3, class: "axis-label" });
    lab.textContent = fmt ? fmt(max * (1 - i / 4)) : shortNum(max * (1 - i / 4));
    add(svg, lab);
  }
  const bw = (w - pad.l - pad.r) / Math.max(1, rows.length);
  rows.forEach((b, i) => {
    let y = h - pad.b;
    const x = pad.l + i * bw + bw * 0.15;
    for (const p of parts(b)) {
      const ph = ((p.value || 0) / max) * (h - pad.t - pad.b);
      y -= ph;
      const rect = svgEl("rect", { x: x, y: y, width: bw * 0.7, height: Math.max(0, ph), class: p.cls, rx: 2 });
      const t = svgEl("title");
      t.textContent = b.key + " · " + p.label + ": " + (fmt ? fmt(p.value) : Number(p.value).toLocaleString());
      add(rect, t);
      add(svg, rect);
    }
    if (i % Math.ceil(rows.length / 8) === 0) {
      const lab = svgEl("text", { x: x, y: h - 10, class: "axis-label" });
      lab.textContent = String(b.key).slice(5);
      add(svg, lab);
    }
  });
  add(box, svg);
  const legend = el("div", "fitlegend");
  for (const p of parts(rows[0] || {})) {
    const item = el("span");
    const sw = el("span", "swatch");
    sw.style.background = p.cls === "bar-a" ? "var(--fill-2)" : "var(--fill-1)";
    add(item, sw, el("span", null, p.label));
    add(legend, item);
  }
  add(box, legend);
  return box;
}

/** A horizontal bar chart, for "tok/s by backend". */
function hbarChart(title, items, fmt) {
  const box = el("div", "chart");
  add(box, el("h3", null, title));
  if (!items.length) {
    add(box, el("div", "muted", "nothing measured yet"));
    return box;
  }
  const max = Math.max(...items.map((i) => i.value));
  const stack = el("div", "stack");
  for (const it of items) {
    const row = el("div");
    const head = el("div", "split");
    add(head, el("span", "mono truncate", it.label), el("span", "spacer"), el("span", "muted", fmt ? fmt(it.value) : num(it.value, 1)));
    const meter = el("div", "meter");
    const f = el("span");
    f.style.width = Math.round((it.value / max) * 100) + "%";
    add(meter, f);
    add(row, head, meter);
    add(stack, row);
  }
  add(box, stack);
  return box;
}

/** The grouping table, with a metered/approximate badge per row. */
function groupTable(title, buckets) {
  const box = el("div", "chart");
  add(box, el("h3", null, title));
  const wrap = el("div", "tablewrap");
  const table = el("table");
  const thead = el("thead");
  const hr = el("tr");
  for (const h of ["key", "requests", "prompt", "completion", "p50 tok/s", "cost"]) {
    add(hr, el("th", h === "key" ? null : "num", h));
  }
  add(thead, hr);
  add(table, thead);
  const tb = el("tbody");
  for (const b of buckets) {
    const tr = el("tr");
    add(tr, el("td", "mono", b.key));
    add(tr, el("td", "num", Number(b.requests).toLocaleString()));
    add(tr, el("td", "num", Number(b.prompt_tokens).toLocaleString()));
    add(tr, el("td", "num", Number(b.completion_tokens).toLocaleString()));
    add(tr, el("td", "num", b.tok_per_s_p50 ? num(b.tok_per_s_p50, 1) : "—"));
    const c = el("td", "num");
    add(c, costNode(b.cost));
    add(tr, c);
    add(tb, tr);
  }
  add(table, tb);
  add(wrap, table);
  add(box, buckets.length ? wrap : el("div", "muted", "nothing in this window"));
  return box;
}

/** `12345` -> `12.3k`, for axis labels. */
function shortNum(v) {
  const n = Number(v);
  if (!isFinite(n)) return "0";
  if (n >= 1e9) return (n / 1e9).toFixed(1) + "G";
  if (n >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "k";
  return String(Math.round(n));
}

// ---------------------------------------------------------------------------------------
// panel: Doctor
// ---------------------------------------------------------------------------------------

/** One check row: status badge, timing, detail and the fix line. */
function checkRow(c, runnable) {
  const card = el("div", "card");
  const head = el("div", "card-head");
  const tone = c.status === "pass" ? "good" : c.status === "warn" ? "warn" : c.status === "fail" ? "critical" : "unknown";
  const icon = c.status === "pass" ? "●" : c.status === "warn" ? "!" : c.status === "fail" ? "✕" : "○";
  add(head, badge(pretty(c.status), icon, tone));
  add(head, el("span", "card-title", c.label || c.id));
  add(head, el("span", "card-id", c.id));
  if (c.ms !== undefined) add(head, el("span", "muted", c.ms + " ms"));
  add(card, head);
  if (c.detail) add(card, el("div", "muted", c.detail));
  if (c.fix) {
    const f = el("div", "banner banner-note");
    add(f, badge("fix", "→", null), el("code", "mono", c.fix));
    add(card, f);
  }
  if (runnable) {
    const acts = el("div", "actions");
    add(acts, btn("Run", "▶", "btn-sm", () => runDiagnose(c.id)));
    add(card, acts);
  }
  return card;
}

/** The Doctor panel: the check registry as rows, plus the four smoke probes. */
function renderDoctor(root) {
  const kids = [];
  kids.push(
    panelHead(
      "Doctor",
      "each check is individually runnable and carries its own fix line",
      btn("Run all", "▶", "btn-primary", () => runDiagnose(null)),
      btn("Smoke…", "≋", "btn-sm", () => runSmoke()),
    ),
  );
  if (S.gone.checks) kids.push(unavailableNote("checks", "the check registry and the diagnose stream"));
  if (!S.checks.length) {
    kids.push(el("div", "empty", "No checks have run yet."));
  } else {
    const grid = el("div", "grid");
    for (const c of S.checks) add(grid, checkRow(c, true));
    kids.push(grid);
  }
  if (S.smoke && S.smoke.length) {
    kids.push(el("h3", null, "Smoke probes"));
    const grid = el("div", "grid");
    for (const p of S.smoke) {
      const c = el("div", "card");
      const head = el("div", "card-head");
      add(head, badge(p.ok ? "pass" : "fail", p.ok ? "●" : "✕", p.ok ? "good" : "critical"));
      add(head, el("span", "card-title", p.name));
      add(c, head);
      add(
        c,
        kv([
          ["ms", p.ms],
          ["TTFT", p.ttft_ms === null || p.ttft_ms === undefined ? "—" : p.ttft_ms + " ms"],
          ["tok/s", num(p.tok_per_s, 2) + "  (read from the upstream timings object)"],
          ["tokens", p.tokens === null || p.tokens === undefined ? "—" : String(p.tokens)],
          ["detail", p.detail],
        ]),
      );
      add(grid, c);
    }
    kids.push(grid);
  }
  root.replaceChildren(...kids);
}

/**
 * Read an SSE body from a fetch, one JSON payload per `data:` line.
 *
 * `EventSource` cannot POST and cannot carry a body, and two of the three streams here are
 * POSTs, so the stream is read off the response body instead.
 */
async function sseFetch(path, opts, onData) {
  const o = opts || {};
  const init = { method: o.method || "GET", headers: { accept: "text/event-stream" } };
  if (o.body !== undefined) {
    init.headers["content-type"] = "application/json";
    init.body = JSON.stringify(o.body);
  }
  try {
    const res = await fetch(path, init);
    if (!res.ok || !res.body) {
      if (res.status === 404 || res.status === 501 || res.status === 503) S.gone[featureOf(path)] = "the daemon answered " + res.status;
      toast(path + " — " + res.status, "bad");
      return;
    }
    const reader = res.body.getReader();
    const dec = new TextDecoder();
    let buf = "";
    for (;;) {
      const chunk = await reader.read();
      if (chunk.done) break;
      buf += dec.decode(chunk.value, { stream: true });
      let nl = buf.indexOf("\n");
      while (nl >= 0) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (line.indexOf("data:") === 0) {
          const payload = line.slice(5).trim();
          if (payload) {
            try {
              onData(JSON.parse(payload));
            } catch (e) {
              onData({ raw: payload });
            }
          }
        }
        nl = buf.indexOf("\n");
      }
    }
  } catch (e) {
    S.lastError = String(e);
    paintConnection();
    toast("the stream from " + path + " ended: " + e, "bad");
  }
}

/** `GET /v1/diagnose` — one event per check, rendered as it lands. */
async function runDiagnose(only) {
  if (!only) S.checks = [];
  await sseFetch("/v1/diagnose" + (only ? "?only=" + encodeURIComponent(only) : ""), null, (d) => {
    const r = d && d.result ? d.result : d;
    if (!r || !r.id) return;
    const i = S.checks.findIndex((c) => c.id === r.id);
    if (i >= 0) S.checks[i] = r;
    else S.checks.push(r);
    scheduleRender();
  });
  scheduleRender();
}

/** `POST /v1/smoke` — the four probes, streamed. */
function runSmoke() {
  promptDrawer({
    title: "Smoke probes",
    label: "alias",
    message: "Four probes: the models list, an 80-token warm-up, a tool-calling probe and a 200-token throughput run. Every number is read from the upstream timings object, never from a stopwatch.",
    value: (S.snap && S.snap.proxy.default_alias) || "auto",
    confirmLabel: "Run",
    run: async (alias) => {
      S.smoke = [];
      show("doctor");
      await sseFetch("/v1/smoke", { method: "POST", body: { alias: alias } }, (d) => {
        const p = d && d.probe ? d.probe : d;
        if (!p || !p.name) return;
        S.smoke.push(p);
        scheduleRender();
      });
    },
  });
}

// ---------------------------------------------------------------------------------------
// the Launch drawer — one EndpointSpec, three tabs, the summary visible WHILE you edit
// ---------------------------------------------------------------------------------------

/** A fresh draft. Nothing here is a hidden default: every field is on screen. */
function resetLaunch() {
  S.launch = {
    tab: "local",
    local: {
      model: "",
      build: "",
      devices: [],
      ctx: 8192,
      parallel: 1,
      kv: "f16",
      batch: 2048,
      split_mode: "layer",
      main_gpu: null,
      tensor_split: "",
      mode: "nonthinking",
      flash: "auto",
      alias: "",
    },
    vllm: { model_id: "", tp: 1, ctx: 8192, quantization: "", kv_cache_dtype: "", devices: [], alias: "" },
    rent: { profile: "", recipe: "", offer: null, hours: 2, maxDph: "", alias: "" },
    fitInput: null,
    fit: null,
    fitErr: null,
    job: null,
    startedAt: null,
  };
}

/** Open the drawer, optionally on a given tab. */
function openLaunch(tab) {
  if (!S.launch) resetLaunch();
  if (tab) S.launch.tab = tab;
  $("drawer-launch").hidden = false;
  if (!S.localModels.length) loadLocalModels(false).then(() => renderLaunch());
  renderLaunch();
  if (S.launch.tab === "local") solveFit();
}

/** Close it. The draft survives, so re-opening does not lose ten slider moves. */
function closeLaunch() {
  $("drawer-launch").hidden = true;
  if (location.hash === "#launch") history.replaceState(null, "", "#" + S.panel);
}

/** Render the drawer body for the active tab. */
function renderLaunch() {
  const L = S.launch;
  for (const t of document.querySelectorAll(".dtab")) t.classList.toggle("is-active", t.dataset.ltab === L.tab);
  $("launch-title").textContent = L.job ? "Launching…" : "Launch";
  const body = $("launch-body");
  if (L.job) {
    body.replaceChildren(bootView());
  } else if (L.tab === "local") {
    body.replaceChildren(...localTab());
  } else if (L.tab === "vllm") {
    body.replaceChildren(...vllmTab());
  } else {
    body.replaceChildren(...rentTab());
  }
  renderLaunchSummary();
}

/** Any change: invalidate the cached FitInput when the shape of the budget changed. */
function launchChanged(invalidate) {
  if (invalidate) S.launch.fitInput = null;
  scheduleFit();
}

/** The fit solve, debounced so a dragged slider does not issue 200 POSTs. */
const scheduleFit = debounced("fit", 120, () => solveFit());

/** Resolve the model + budget once, then hammer the pure solver as the sliders move. */
async function solveFit() {
  const L = S.launch;
  if (!L || L.tab !== "local") {
    renderLaunchSummary();
    return;
  }
  const l = L.local;
  if (!l.model) {
    L.fit = null;
    L.fitErr = null;
    renderLaunchSummary();
    return;
  }
  if (!L.fitInput) {
    const q = {
      model: l.model,
      build: l.build || null,
      devices: l.devices.length ? l.devices.join(",") : null,
      split_mode: l.split_mode,
      main_gpu: l.main_gpu,
      tensor_split: l.tensor_split || null,
      ctx: l.ctx,
      parallel: l.parallel,
      kv: l.kv,
      batch: l.batch,
    };
    const r = await api("/v1/fit/input", { method: "POST", body: q });
    if (!r.ok) {
      L.fit = null;
      L.fitErr = r.error;
      renderLaunchSummary();
      return;
    }
    L.fitInput = r.data;
  }
  const input = JSON.parse(JSON.stringify(L.fitInput));
  input.want_ctx = l.ctx;
  input.want_parallel = l.parallel;
  input.want_kv = l.kv;
  input.batch = l.batch;
  input.split.mode = l.split_mode;
  if (l.devices.length) input.split.devices = l.devices;
  input.split.main_gpu = l.main_gpu;
  input.split.tensor_split = parseRatios(l.tensor_split);
  const r2 = await api("/v1/fit", { method: "POST", body: input });
  if (r2.ok) {
    L.fit = r2.data;
    L.fitErr = null;
  } else {
    L.fit = null;
    L.fitErr = r2.error;
  }
  renderLaunchSummary();
}

/** `"0.6,0.4"` -> `[0.6, 0.4]`, or null. */
function parseRatios(s) {
  if (!s) return null;
  const parts = String(s)
    .split(",")
    .map((x) => Number(x.trim()))
    .filter((x) => isFinite(x));
  return parts.length ? parts : null;
}

/** The Local tab. */
function localTab() {
  const l = S.launch.local;
  const rig = (S.snap && S.snap.rig) || { gpus: [], builds: [] };
  const out = [];

  const s1 = el("div", "section");
  add(s1, el("h3", null, "Weights"));
  const models = S.localModels.map((m) => [
    m.id,
    m.name + " · " + bytes(m.total_bytes) + (m.quant ? " · " + m.quant : "") + ((m.mmproj || []).length ? " · vision" : ""),
  ]);
  models.unshift(["", S.localModels.length ? "choose a model…" : "no local models discovered"]);
  add(
    s1,
    field(
      "model (shards grouped, real sizes)",
      select(models, l.model, (v) => {
        l.model = v;
        const m = S.localModels.find((x) => x.id === v);
        if (m && m.gguf && m.gguf.n_ctx_train) l.ctx = Math.min(l.ctx || 8192, m.gguf.n_ctx_train);
        launchChanged(true);
        renderLaunch();
      }),
    ),
  );
  const builds = rig.builds.map((b) => [b.id, b.label + " · " + (b.backends || []).map(backendName).join("/")]);
  builds.unshift(["", "let endpoint start pick"]);
  add(
    s1,
    field(
      "llama.cpp build",
      select(builds, l.build, (v) => {
        l.build = v;
        launchChanged(true);
      }),
      "the build picks the compute backend, and therefore which enumeration of a device the budget is over",
    ),
  );
  out.push(s1);

  const s2 = el("div", "section");
  add(s2, el("h3", null, "Devices"));
  const checks = el("div", "checks");
  for (const d of physicalDevices(rig.gpus)) {
    for (const v of d.views) {
      const lab = el("label");
      const cb = input("checkbox", null, null, {});
      cb.checked = l.devices.indexOf(v.device) >= 0;
      cb.addEventListener("change", () => {
        if (cb.checked) l.devices.push(v.device);
        else l.devices = l.devices.filter((x) => x !== v.device);
        launchChanged(true);
      });
      add(lab, cb, el("span", null, v.device + " · " + backendName(v.backend) + " · " + mb(v.vram_free_mb) + " free"));
      add(checks, lab);
    }
  }
  if (!rig.gpus.length) add(checks, el("span", "muted", "no GPUs enumerated — this will run on the CPU"));
  add(s2, checks);
  const split = el("div", "fields");
  add(
    split,
    field(
      "split mode (-sm)",
      select(
        [
          ["none", "none"],
          ["layer", "layer"],
          ["row", "row"],
          ["tensor", "tensor"],
        ],
        l.split_mode,
        (v) => {
          l.split_mode = v;
          launchChanged(false);
        },
      ),
    ),
  );
  add(
    split,
    field(
      "main GPU (-mg)",
      input("number", l.main_gpu === null ? "" : l.main_gpu, (v) => {
        l.main_gpu = v === "" ? null : Number(v);
        launchChanged(false);
      }, { min: "0", step: "1" }),
    ),
  );
  add(
    split,
    field(
      "tensor split",
      input("text", l.tensor_split, (v) => {
        l.tensor_split = v;
        launchChanged(false);
      }, { placeholder: "0.6,0.4" }),
    ),
  );
  add(s2, split);
  out.push(s2);

  const s3 = el("div", "section");
  add(s3, el("h3", null, "Context, slots, KV"));
  const m = S.localModels.find((x) => x.id === l.model);
  const maxCtx = m && m.gguf && m.gguf.n_ctx_train ? m.gguf.n_ctx_train : 262144;
  add(
    s3,
    slider("context (total pool, shared across slots)", l.ctx, 512, maxCtx, 512, (v) => {
      l.ctx = v;
      launchChanged(false);
    }),
  );
  add(
    s3,
    slider("parallel slots (-np)", l.parallel, 1, 16, 1, (v) => {
      l.parallel = v;
      launchChanged(false);
    }),
  );
  add(
    s3,
    slider("logical batch (-b)", l.batch, 256, 8192, 256, (v) => {
      l.batch = v;
      launchChanged(false);
    }),
  );
  const kvs = ["f32", "f16", "bf16", "q8_0", "q5_1", "q5_0", "q4_1", "q4_0", "iq4_nl"].map((k) => [k, k]);
  add(
    s3,
    field(
      "KV cache type (-ctk/-ctv)",
      select(kvs, l.kv, (v) => {
        l.kv = v;
        launchChanged(false);
      }),
    ),
  );
  out.push(s3);

  const s4 = el("div", "section");
  add(s4, el("h3", null, "Behaviour"));
  const f4 = el("div", "fields");
  add(
    f4,
    field(
      "mode preset",
      select(
        [
          ["thinking", "thinking"],
          ["coding", "coding"],
          ["nonthinking", "nonthinking"],
          ["raw", "raw (no sampling opinions)"],
        ],
        l.mode,
        (v) => {
          l.mode = v;
        },
      ),
    ),
  );
  add(
    f4,
    field(
      "flash attention",
      select(
        [
          ["auto", "auto"],
          ["on", "on"],
          ["off", "off"],
        ],
        l.flash,
        (v) => {
          l.flash = v;
        },
      ),
    ),
  );
  add(
    f4,
    field(
      "bind alias",
      input("text", l.alias, (v) => {
        l.alias = v;
        renderLaunchSummary();
      }, { placeholder: "auto" }),
      "the alias clients will put in \"model\"",
    ),
  );
  add(s4, f4);
  out.push(s4);
  return out;
}

/** A labelled range slider with a live readout. */
function slider(label, value, min, max, step, onChange) {
  const wrap = el("div", "field");
  const head = el("div", "split");
  add(head, el("span", null, label), el("span", "spacer"), el("span", "mono", Number(value).toLocaleString()));
  const readout = head.lastChild;
  const r = input("range", value, null, { min: String(min), max: String(max), step: String(step) });
  r.addEventListener("input", () => {
    readout.textContent = Number(r.value).toLocaleString();
    onChange(Number(r.value));
  });
  add(wrap, head, r);
  return wrap;
}

/** The vLLM tab. */
function vllmTab() {
  const v = S.launch.vllm;
  const out = [];
  const s = el("div", "section");
  add(s, el("h3", null, "vLLM"));
  const f = el("div", "fields");
  add(
    f,
    field(
      "model id",
      input("text", v.model_id, (x) => {
        v.model_id = x;
        renderLaunchSummary();
      }, { placeholder: "Qwen/Qwen3-8B-AWQ" }),
    ),
  );
  add(
    f,
    field(
      "tensor parallel",
      input("number", v.tp, (x) => {
        v.tp = Number(x) || 1;
        renderLaunchSummary();
      }, { min: "1", step: "1" }),
    ),
  );
  add(
    f,
    field(
      "context",
      input("number", v.ctx, (x) => {
        v.ctx = Number(x) || null;
        renderLaunchSummary();
      }, { min: "512", step: "512" }),
    ),
  );
  add(
    f,
    field(
      "quantization",
      input("text", v.quantization, (x) => {
        v.quantization = x;
      }, { placeholder: "awq / gptq / fp8" }),
    ),
  );
  add(
    f,
    field(
      "kv cache dtype",
      input("text", v.kv_cache_dtype, (x) => {
        v.kv_cache_dtype = x;
      }, { placeholder: "auto / fp8" }),
    ),
  );
  add(
    f,
    field(
      "bind alias",
      input("text", v.alias, (x) => {
        v.alias = x;
        renderLaunchSummary();
      }, { placeholder: "auto" }),
    ),
  );
  add(s, f);
  add(
    s,
    el(
      "div",
      "hint",
      "vLLM sizes its own KV cache from gpu_memory_utilization, so the fit solver — which is GGUF header arithmetic — does not apply here.",
    ),
  );
  out.push(s);
  return out;
}

/** The Rent tab: profile → offers → weights → cost → a typed confirmation. */
function rentTab() {
  const r = S.launch.rent;
  const out = [];

  const s0 = el("div", "section");
  add(s0, el("h3", null, "Search profile"));
  const profiles = ((S.snap && S.snap.profiles) || []).map((p) => [p.id, p.label]);
  profiles.unshift(["", "choose a saved profile…"]);
  add(
    s0,
    field(
      "profile",
      select(profiles, r.profile, (v) => {
        r.profile = v;
      }),
    ),
  );
  const vastRecipes = ((S.snap && S.snap.recipes) || []).filter((x) => x.kind.kind === "vast").map((x) => [x.id, x.label]);
  vastRecipes.unshift(["", "choose a vast recipe (it carries the container launch)…"]);
  add(
    s0,
    field(
      "container recipe",
      select(vastRecipes, r.recipe, (v) => {
        r.recipe = v;
        renderLaunchSummary();
      }),
      "a rent needs an image, an onstart and a disk size — that is what a vast recipe is",
    ),
  );
  const row = el("div", "split");
  add(row, btn("Search offers", "◎", "btn-sm", () => searchOffers()));
  add(
    row,
    input("search", S.offers.filter, (v) => {
      S.offers.filter = v;
      renderLaunch();
    }, { placeholder: "filter: gpu, geo…" }),
  );
  add(s0, row);
  out.push(s0);

  if (S.gone.vast) {
    out.push(unavailableNote("vast", "the vast.ai market surface"));
    return out;
  }

  const s1 = el("div", "section");
  add(s1, el("h3", null, "Offers (" + S.offers.rows.length + ")"));
  for (const rel of S.offers.relaxations) {
    const b = el("div", "banner banner-note");
    add(b, badge("relaxed", "ⓘ", null), el("span", null, rel));
    add(s1, b);
  }
  add(s1, offerTable());
  out.push(s1);

  const s2 = el("div", "section");
  add(s2, el("h3", null, "Weights"));
  add(s2, hfSection());
  out.push(s2);

  const s3 = el("div", "section");
  add(s3, el("h3", null, "Cost"));
  add(s3, costPanel());
  out.push(s3);
  return out;
}

/** `POST /v1/vast/offers/search` — a live, re-queryable market table. */
async function searchOffers() {
  const r = S.launch.rent;
  if (!r.profile) {
    toast("choose a search profile first", "info");
    return;
  }
  S.offers.busy = true;
  const res = await api("/v1/vast/offers/search", { method: "POST", body: { profile: r.profile } });
  S.offers.busy = false;
  if (res.ok && res.data) {
    S.offers.rows = res.data.offers || [];
    S.offers.relaxations = res.data.relaxations || [];
  }
  renderLaunch();
}

/** The sortable, filterable offer table. */
function offerTable() {
  if (!S.offers.rows.length) {
    return el("div", "empty", S.offers.busy ? "searching…" : "No offers yet. Pick a profile and search.");
  }
  const f = S.offers.filter.toLowerCase();
  const rows = S.offers.rows
    .filter((o) => !f || (o.gpu_name + " " + (o.geolocation || "")).toLowerCase().indexOf(f) >= 0)
    .slice()
    .sort((a, b) => {
      const k = S.offers.sort;
      const av = a[k] === null || a[k] === undefined ? 0 : a[k];
      const bv = b[k] === null || b[k] === undefined ? 0 : b[k];
      return (av > bv ? 1 : av < bv ? -1 : 0) * S.offers.dir;
    });
  const wrap = el("div", "tablewrap");
  const table = el("table");
  const thead = el("thead");
  const hr = el("tr");
  const cols = [
    ["gpu_name", "gpu"],
    ["num_gpus", "n"],
    ["gpu_total_ram", "pooled VRAM"],
    ["dph_total", "$/hr"],
    ["reliability2", "rel"],
    ["inet_down", "down"],
    ["geolocation", "geo"],
  ];
  for (const c of cols) {
    const th = el("th", "sortable" + (c[0] === "gpu_name" || c[0] === "geolocation" ? "" : " num"), c[1]);
    th.addEventListener("click", () => {
      S.offers.dir = S.offers.sort === c[0] ? -S.offers.dir : 1;
      S.offers.sort = c[0];
      renderLaunch();
    });
    add(hr, th);
  }
  add(hr, el("th", null, ""));
  add(thead, hr);
  add(table, thead);
  const tb = el("tbody");
  for (const o of rows.slice(0, 100)) {
    const tr = el("tr", "is-clickable");
    if (S.launch.rent.offer && S.launch.rent.offer.id === o.id) tr.classList.add("is-selected");
    tr.addEventListener("click", () => {
      S.launch.rent.offer = o;
      S.launch.rent.maxDph = String(o.dph_total.toFixed(3));
      renderLaunch();
    });
    add(tr, el("td", "mono", o.gpu_name));
    add(tr, el("td", "num", String(o.num_gpus)));
    add(tr, el("td", "num", mb(o.gpu_total_ram || o.gpu_ram * o.num_gpus)));
    add(tr, el("td", "num", usd(o.dph_total, 3)));
    add(tr, el("td", "num", num(o.reliability2, 2)));
    add(tr, el("td", "num", o.inet_down ? num(o.inet_down, 0) : "—"));
    add(tr, el("td", null, o.geolocation || "—"));
    const ac = el("td");
    add(ac, badge("select", "→", null));
    add(tr, ac);
    add(tb, tr);
  }
  add(table, tb);
  add(wrap, table);
  return wrap;
}

/** $/hr, estimated total, credit and burn-down, then a typed confirmation. */
function costPanel() {
  const r = S.launch.rent;
  const box = el("div", "stack");
  if (!r.offer) {
    add(box, el("div", "hint", "Select an offer to see what it costs before anything is rented."));
    return box;
  }
  const credit = S.snap && S.snap.totals.vast_credit !== null && S.snap.totals.vast_credit !== undefined ? S.snap.totals.vast_credit : null;
  const est = r.offer.dph_total * r.hours;
  const tiles = el("div", "tiles");
  add(tiles, tile("rate", usd(r.offer.dph_total, 3) + "/hr", r.offer.num_gpus + "× " + r.offer.gpu_name));
  add(tiles, tile("estimated total", usd(est), r.hours + " h at that rate"));
  add(tiles, tile("credit now", credit === null ? "—" : usd(credit), "vast.ai"));
  add(
    tiles,
    tile("burn-down", credit === null ? "—" : dur((credit / r.offer.dph_total) * 3600), "credit ÷ this rate, ignoring anything else running"),
  );
  add(box, tiles);

  const weights = selectedHfBytes();
  const pooled = (r.offer.gpu_total_ram || r.offer.gpu_ram * r.offer.num_gpus) * 1;
  if (weights) {
    const needMb = (weights / (1024 * 1024)) * 1.15;
    const fits = needMb < pooled;
    const b = el("div", "banner " + (fits ? "banner-note" : "banner-warn"));
    add(
      b,
      badge(fits ? "weights fit" : "weights do not fit", fits ? "●" : "✕", fits ? "good" : "critical"),
      el("span", null, bytes(weights) + " of weights + 15 % against " + mb(pooled) + " of pooled VRAM"),
    );
    add(box, b);
  }

  add(
    box,
    field(
      "hours to estimate",
      input("number", r.hours, (v) => {
        r.hours = Number(v) || 1;
        renderLaunch();
      }, { min: "1", step: "1" }),
    ),
  );
  add(
    box,
    field(
      "your ceiling, $/hr (the daemon refuses anything above it)",
      input("text", r.maxDph, (v) => {
        r.maxDph = v;
      }, { placeholder: "0.900" }),
    ),
  );
  add(
    box,
    btn("Rent…", "＄", "btn-danger", () => {
      if (!r.recipe) {
        toast("pick a container recipe first — a rent needs an image and an onstart", "bad");
        return;
      }
      const recipe = (S.snap.recipes || []).find((x) => x.id === r.recipe);
      const max = Number(r.maxDph);
      if (!isFinite(max) || max <= 0) {
        toast("set a $/hr ceiling first", "bad");
        return;
      }
      confirmMoney({
        title: "Rent " + r.offer.num_gpus + "× " + r.offer.gpu_name,
        danger: true,
        word: "rent",
        lines: [
          ["offer", String(r.offer.id) + " · " + (r.offer.geolocation || "?")],
          ["rate", usd(r.offer.dph_total, 3) + "/hr"],
          ["your ceiling", usd(max, 3) + "/hr"],
          ["estimated total", usd(r.offer.dph_total * r.hours) + " over " + r.hours + " h"],
          ["credit now", credit === null ? "unknown" : usd(credit)],
          ["burn-down", credit === null ? "unknown" : dur((credit / r.offer.dph_total) * 3600)],
        ],
        confirmLabel: "Rent — this spends real money",
        run: () =>
          act(
            "/v1/vast/instances?no_wait=true",
            {
              method: "POST",
              body: {
                offer_id: r.offer.id,
                profile: r.profile || null,
                launch: recipe ? recipe.kind.launch : null,
                confirm: true,
                max_usd_per_hour: max,
                auto_tunnel: true,
                bind_alias: r.alias ? slug(r.alias, "rented") : null,
              },
            },
            "rent requested",
          ),
      });
    }),
  );
  return box;
}

/** The size of the HF file the user last listed, if any — for the pooled-VRAM check. */
function selectedHfBytes() {
  for (const repo of Object.keys(S.hf.files)) {
    const files = S.hf.files[repo].filter((f) => !f.is_mmproj && f.size);
    if (files.length) return files.reduce((a, f) => a + f.size, 0);
  }
  return null;
}

// ---------------------------------------------------------------------------------------
// the Launch summary: spec + fit bar + the buttons. Visible WHILE you edit.
// ---------------------------------------------------------------------------------------

/** The drawer footer. This is what makes the fit bar move as a slider moves. */
function renderLaunchSummary() {
  const box = $("launch-summary");
  if (!box || !S.launch) return;
  const L = S.launch;
  if (L.job) {
    box.replaceChildren(
      btn("Close", "✕", "btn-ghost", () => closeLaunch()),
      btn("Launch another", "＋", "btn-sm", () => {
        resetLaunch();
        renderLaunch();
      }),
    );
    return;
  }
  const kids = [];
  if (L.tab === "local") {
    const l = L.local;
    const m = S.localModels.find((x) => x.id === l.model);
    kids.push(
      el(
        "div",
        "hint truncate",
        (m ? m.name : "no model") +
          " · " +
          (l.build || "auto build") +
          " · " +
          (l.devices.length ? l.devices.join("+") : "every GPU of that backend") +
          " · ctx " +
          Number(l.ctx).toLocaleString() +
          " × " +
          l.parallel +
          " · kv " +
          l.kv,
      ),
    );
    if (L.fitErr) {
      const b = el("div", "banner banner-warn");
      add(b, badge("fit unavailable", "!", "warn"), el("span", null, L.fitErr));
      kids.push(b);
    } else if (L.fit) {
      kids.push(fitBar(L.fit));
    } else {
      kids.push(el("div", "hint", "choose a model to solve the fit"));
    }
  } else if (L.tab === "vllm") {
    kids.push(el("div", "hint truncate", (L.vllm.model_id || "no model id") + " · TP " + L.vllm.tp + " · ctx " + L.vllm.ctx));
  } else {
    const r = L.rent;
    kids.push(
      el("div", "hint truncate", r.offer ? r.offer.num_gpus + "× " + r.offer.gpu_name + " at " + usd(r.offer.dph_total, 3) + "/hr" : "no offer selected"),
    );
  }

  const acts = el("div", "split");
  if (L.tab === "local") {
    add(acts, btn("Start & bind", "▶", "btn-primary", () => startLocal()));
    add(acts, btn("Save as recipe", "✎", "btn-sm", () => saveLaunchRecipe()));
  } else if (L.tab === "vllm") {
    add(acts, btn("Start & bind", "▶", "btn-primary", () => startVllm()));
    add(acts, btn("Save as recipe", "✎", "btn-sm", () => saveLaunchRecipe()));
  } else {
    add(acts, el("span", "hint", "renting is confirmed in the cost panel above, never from this bar"));
  }
  kids.push(acts);
  box.replaceChildren(...kids);
}

/** The stacked weights / KV / compute / headroom bar, with `why[]` as tooltips. */
function fitBar(fit) {
  const wrap = el("div", "stack");
  const head = el("div", "split");
  const v = fit.verdict;
  const tone = v.verdict === "fits" ? "good" : v.verdict === "tight" ? "warn" : v.verdict === "needs_offload" ? "serious" : "critical";
  const icon = v.verdict === "fits" ? "●" : v.verdict === "tight" ? "!" : v.verdict === "needs_offload" ? "▲" : "✕";
  const label = {
    fits: "Fits",
    tight: "Tight",
    needs_offload: "Needs offload — " + v.layers_on_gpu + " layers on GPU",
    wont_fit: "Won't fit — short by " + mb(v.short_by_mb),
  }[v.verdict] || pretty(v.verdict);
  add(head, badge(label, icon, tone, (fit.why || []).join("\n")));
  add(head, el("span", "mono", "ctx " + Number(fit.ctx).toLocaleString() + " × " + fit.parallel + " · " + fit.kv_type));
  add(head, el("span", "spacer"));
  add(
    head,
    el(
      "span",
      "muted",
      fit.headroom_mb >= 0 ? "headroom " + mb(fit.headroom_mb) : "short by " + mb(-fit.headroom_mb),
    ),
  );
  add(wrap, head);

  const used = fit.weights_mb + fit.kv_mb + fit.compute_mb;
  const total = used + Math.max(0, fit.headroom_mb);
  const bar = el("div", "fitbar");
  bar.title = (fit.why || []).join("\n");
  const segs = [
    ["weights", fit.weights_mb, "fitseg-weights"],
    ["KV", fit.kv_mb, "fitseg-kv"],
    ["compute", fit.compute_mb, "fitseg-compute"],
  ];
  for (const s of segs) {
    const seg = el("div", "fitseg " + s[2]);
    seg.style.width = (total > 0 ? (s[1] / total) * 100 : 0) + "%";
    seg.title = s[0] + ": " + mb(s[1]);
    add(bar, seg);
  }
  if (fit.headroom_mb < 0) {
    const over = el("div", "fitseg fitseg-over");
    over.style.width = "100%";
    over.title = "over budget by " + mb(-fit.headroom_mb);
    bar.replaceChildren(over);
  }
  add(wrap, bar);

  const legend = el("div", "fitlegend");
  const items = [
    ["weights " + mb(fit.weights_mb), "var(--fill-1)"],
    ["KV " + mb(fit.kv_mb), "var(--fill-2)"],
    ["compute " + mb(fit.compute_mb), "var(--fill-3)"],
    ["headroom " + mb(Math.max(0, fit.headroom_mb)), "var(--fill-4)"],
  ];
  for (const it of items) {
    const s = el("span");
    const sw = el("span", "swatch");
    sw.style.background = it[1];
    add(s, sw, el("span", null, it[0]));
    add(legend, s);
  }
  add(wrap, legend);

  if ((fit.per_device_mb || []).length > 1) {
    const per = el("div", "fitlegend");
    for (const d of fit.per_device_mb) add(per, el("span", "mono", d[0] + ": " + mb(d[1])));
    add(wrap, per);
  }
  if ((fit.why || []).length) {
    const ul = el("ul", "why");
    for (const w of fit.why) add(ul, el("li", null, w));
    add(wrap, ul);
  }
  return wrap;
}

/** The `EndpointSpec` the Local tab describes. */
function localSpec() {
  const l = S.launch.local;
  const m = S.localModels.find((x) => x.id === l.model);
  return {
    kind: "local_llama",
    build: l.build || (S.snap && S.snap.rig.builds.length ? S.snap.rig.builds[0].id : ""),
    model_path: m && m.shards.length ? m.shards[0].path : "",
    mmproj: m && (m.mmproj || []).length ? m.mmproj[0].path : null,
    alias_flag: slug(l.alias || (m ? m.name : "model"), "model"),
    host: "127.0.0.1",
    port: null,
    ctx: l.ctx,
    parallel: l.parallel,
    kv_type: l.kv,
    ngl: { ngl: "auto" },
    split: {
      devices: l.devices,
      mode: l.split_mode,
      main_gpu: l.main_gpu,
      tensor_split: parseRatios(l.tensor_split),
    },
    mode: l.mode,
    flash_attn: l.flash,
    api_key: null,
    extra_args: [],
  };
}

/** The `EndpointSpec` the vLLM tab describes. */
function vllmSpec() {
  const v = S.launch.vllm;
  return {
    kind: "local_vllm",
    bin: "vllm",
    model_id: v.model_id,
    tp: v.tp,
    ctx: v.ctx,
    quantization: v.quantization || null,
    kv_cache_dtype: v.kv_cache_dtype || null,
    enforce_eager: false,
    reasoning_parser: null,
    gpu_util: null,
    max_num_seqs: null,
    trust_remote: false,
    chunked_prefill: true,
    host: "127.0.0.1",
    port: null,
    devices: v.devices,
    extra_args: [],
  };
}

/** `POST /v1/endpoints?no_wait=true`, then become the boot view. */
async function startEndpoint(spec, alias) {
  const q = "?no_wait=true" + (alias ? "&alias=" + encodeURIComponent(slug(alias, "auto")) : "");
  const r = await act("/v1/endpoints" + q, { method: "POST", body: spec }, "launch accepted");
  if (!r.ok) return;
  S.launch.job = r.data;
  S.launch.startedAt = Date.now() / 1000;
  S.logs.lines = [];
  renderLaunch();
}

/** Start what the Local tab describes. */
function startLocal() {
  const spec = localSpec();
  if (!spec.model_path) {
    toast("choose a model first", "bad");
    return;
  }
  startEndpoint(spec, S.launch.local.alias);
}

/** Start what the vLLM tab describes. */
function startVllm() {
  const spec = vllmSpec();
  if (!spec.model_id) {
    toast("a vLLM model id is required", "bad");
    return;
  }
  startEndpoint(spec, S.launch.vllm.alias);
}

/** Save the current draft as a recipe, without starting anything. */
function saveLaunchRecipe() {
  const L = S.launch;
  const spec = L.tab === "local" ? localSpec() : vllmSpec();
  const guess = L.tab === "local" ? (S.localModels.find((x) => x.id === L.local.model) || {}).name : L.vllm.model_id;
  promptDrawer({
    title: "Save as recipe",
    label: "name",
    message: "The whole draft — build, devices, split, context, KV, mode — is saved, together with the fit it solved to.",
    value: guess || "recipe",
    confirmLabel: "Save",
    run: async (label) => {
      const now = Math.floor(Date.now() / 1000);
      const kind = JSON.parse(JSON.stringify(spec));
      kind.kind = L.tab === "local" ? "local" : "local_vllm";
      const body = {
        id: slug(label, "recipe"),
        label: label,
        description: null,
        kind: kind,
        provenance: { discovered_at_unix: now, size_bytes: null, source: "launch drawer", fit: L.fit || null },
        created_at_unix: now,
        updated_at_unix: now,
      };
      const r = await act("/v1/recipes", { method: "POST", body: body }, "recipe saved");
      if (r.ok && S.snap) {
        S.snap.recipes = (S.snap.recipes || []).filter((x) => x.id !== r.data.id).concat([r.data]);
        scheduleRender();
      }
    },
  });
}

/** The live `BootPhase` view the drawer becomes — there is no separate "watch boot". */
function bootView() {
  const L = S.launch;
  const wrap = el("div", "stack");
  const j = L.job || {};
  const backendId = j.result && j.result.id ? j.result.id : null;
  if (backendId && S.logs.src !== backendId) followLogs(backendId);
  const boot = backendId ? S.boots[backendId] : null;

  const head = el("div", "card-head");
  const state = j.state || "pending";
  const tone = state === "failed" ? "critical" : state === "succeeded" ? "good" : "warn";
  add(head, badge(pretty(state), state === "failed" ? "✕" : state === "succeeded" ? "●" : "◐", tone));
  add(head, el("span", "card-title", j.kind || "endpoint.start"));
  add(head, el("span", "mono", dur(Date.now() / 1000 - (L.startedAt || Date.now() / 1000))));
  add(wrap, head);

  const phase = boot ? boot.phase : null;
  if (phase) {
    const line = el("div", "rowline");
    add(line, badge(pretty(phase.phase), "◐", "warn", boot.line || ""));
    if (phase.pct !== undefined && phase.pct !== null) add(line, el("span", "mono", num(phase.pct, 0) + "%"));
    if (phase.mbps) add(line, el("span", "muted", num(phase.mbps, 1) + " MB/s"));
    add(wrap, line);
  }
  if (j.message) add(wrap, el("div", "muted", j.message));
  if (j.error) {
    const b = el("div", "banner banner-bad");
    add(b, badge("failed", "✕", "critical"), el("span", null, j.error));
    add(wrap, b);
  }

  const log = el("pre", "logbox", S.logs.lines.slice(-400).join("\n") || "waiting for the first log line…");
  add(wrap, log);

  const acts = el("div", "split");
  if (backendId) {
    add(acts, btn("Logs", "▤", "btn-sm", () => openLogs(backendId)));
    add(
      acts,
      btn("Destroy", "⛔", "btn-sm btn-danger", () => {
        confirmAction({
          title: "Destroy " + backendId,
          danger: true,
          message: "Stop what is booting and forget the endpoint record.",
          confirmLabel: "Stop and forget",
          run: () => act("/v1/endpoints/" + backendId, { method: "DELETE" }, "removed"),
        });
      }),
    );
  }
  add(acts, btn("Cancel job", "✕", "btn-sm", () => act("/v1/jobs/" + j.id + "/cancel", { method: "POST" }, "cancel requested")));
  add(wrap, acts);
  return wrap;
}

/** Follow one backend's log lines off the event stream. */
function followLogs(id) {
  S.logs.src = id;
  S.logs.lines = [];
}

// ---------------------------------------------------------------------------------------
// the editor drawer — routes, recipes, profiles, logs, typed confirmations
// ---------------------------------------------------------------------------------------

/** Open the editor drawer and let `fill(body, summary)` build it. */
function openEditor(title, fill) {
  $("edit-title").textContent = title;
  const body = $("edit-body");
  const summary = $("edit-summary");
  body.replaceChildren();
  summary.replaceChildren();
  summary.hidden = true;
  $("drawer-edit").hidden = false;
  fill(body, summary);
}

/** Close it. */
function closeEditor() {
  $("drawer-edit").hidden = true;
  S.logs.src = null;
  S.edit = null;
}

/** Route editor: alias, strategy, targets, filters, retry — saved with a hot `PUT`. */
function openRouteEditor(existing) {
  const route = existing
    ? JSON.parse(JSON.stringify(existing))
    : {
        alias: "",
        targets: [],
        strategy: "first_healthy",
        filter: { require_tags: [], exclude_tags: [], max_cost_per_mtok: null, min_ctx: null, require_vision: false, require_tools: false },
        retry: { attempts: 2, failover: true, honor_retry_after: true },
        is_default: false,
        description: null,
      };
  openEditor(existing ? "Route " + existing.alias : "New route", (body, summary) => {
    const redraw = () => openRouteEditor(route);
    const kids = [];

    const s0 = el("div", "section");
    add(
      s0,
      field(
        "alias",
        input("text", route.alias, (v) => {
          route.alias = v;
        }, { placeholder: "auto", readonly: existing ? "readonly" : null }),
        "the string a client puts in \"model\"",
      ),
    );
    add(
      s0,
      field(
        "strategy",
        select(
          [
            ["first_healthy", "first healthy — order matters, nothing surprises you"],
            ["round_robin", "round robin — weighted"],
            ["least_busy", "least busy — by the router's own counter"],
            ["cheapest", "cheapest — refused when no target has a price"],
          ],
          route.strategy,
          (v) => {
            route.strategy = v;
          },
        ),
      ),
    );
    add(
      s0,
      field(
        "description",
        input("text", route.description || "", (v) => {
          route.description = v || null;
        }),
      ),
    );
    kids.push(s0);

    const s1 = el("div", "section");
    add(s1, el("h3", null, "Targets — order matters; drag or use ↑↓"));
    add(s1, targetChips(route, redraw));
    const addRow = el("div", "split");
    const backends = ((S.snap && S.snap.backends) || []).map((b) => [b.id, b.id + " · " + (b.label || "")]);
    backends.unshift(["", "add a backend…"]);
    const sel = select(backends, "", null);
    add(
      addRow,
      sel,
      btn("Add", "＋", "btn-sm", () => {
        if (!sel.value) return;
        route.targets.push({ backend: { sel: "id", value: sel.value }, model: null, weight: 1 });
        redraw();
      }),
    );
    const tagIn = input("text", "", null, { placeholder: "or a tag, e.g. local" });
    add(
      addRow,
      tagIn,
      btn("Add tag", "＋", "btn-sm", () => {
        if (!tagIn.value) return;
        route.targets.push({ backend: { sel: "tag", value: tagIn.value }, model: null, weight: 1 });
        redraw();
      }),
    );
    add(s1, addRow);
    kids.push(s1);

    const s2 = el("div", "section");
    add(s2, el("h3", null, "Filter"));
    const f = el("div", "fields");
    add(
      f,
      field(
        "require tags (comma)",
        input("text", (route.filter.require_tags || []).join(","), (v) => {
          route.filter.require_tags = v.split(",").map((x) => x.trim()).filter(Boolean);
        }),
      ),
    );
    add(
      f,
      field(
        "exclude tags (comma)",
        input("text", (route.filter.exclude_tags || []).join(","), (v) => {
          route.filter.exclude_tags = v.split(",").map((x) => x.trim()).filter(Boolean);
        }),
      ),
    );
    add(
      f,
      field(
        "min ctx",
        input("number", route.filter.min_ctx === null ? "" : route.filter.min_ctx, (v) => {
          route.filter.min_ctx = v === "" ? null : Number(v);
        }),
      ),
    );
    add(
      f,
      field(
        "max $/Mtok (micro-USD)",
        input("number", route.filter.max_cost_per_mtok === null ? "" : route.filter.max_cost_per_mtok, (v) => {
          route.filter.max_cost_per_mtok = v === "" ? null : Number(v);
        }),
      ),
    );
    add(s2, f);
    const caps = el("div", "checks");
    for (const c of [
      ["require_vision", "vision only"],
      ["require_tools", "tool-calling only"],
    ]) {
      const lab = el("label");
      const cb = input("checkbox", null, null, {});
      cb.checked = !!route.filter[c[0]];
      cb.addEventListener("change", () => {
        route.filter[c[0]] = cb.checked;
      });
      add(lab, cb, el("span", null, c[1]));
      add(caps, lab);
    }
    add(s2, caps);
    kids.push(s2);

    const s3 = el("div", "section");
    add(s3, el("h3", null, "Retry"));
    const rf = el("div", "fields");
    add(
      rf,
      field(
        "total attempts (including the first)",
        input("number", route.retry.attempts, (v) => {
          route.retry.attempts = Number(v) || 1;
        }, { min: "1", max: "9" }),
      ),
    );
    const rc = el("div", "checks");
    for (const c of [
      ["failover", "a retry may go to a different backend"],
      ["honor_retry_after", "respect an upstream Retry-After"],
    ]) {
      const lab = el("label");
      const cb = input("checkbox", null, null, {});
      cb.checked = !!route.retry[c[0]];
      cb.addEventListener("change", () => {
        route.retry[c[0]] = cb.checked;
      });
      add(lab, cb, el("span", null, c[1]));
      add(rc, lab);
    }
    add(s3, rf, rc);
    kids.push(s3);
    body.replaceChildren(...kids);

    summary.hidden = false;
    const acts = el("div", "split");
    add(
      acts,
      btn("Validate", "◎", "btn-sm", async () => {
        const r = await api("/v1/routes/validate", { method: "POST", body: [route] });
        if (r.ok || r.data) showReport(route.alias || "draft", r.data);
      }),
    );
    add(
      acts,
      btn("Save", "✓", "btn-primary", async () => {
        if (!route.alias) {
          toast("an alias is required", "bad");
          return;
        }
        route.alias = slug(route.alias, "auto");
        if (await saveRoute(route, false)) {
          closeEditor();
          await firstPaint();
        }
      }),
    );
    if (existing) {
      add(
        acts,
        btn("Delete", "✕", "btn-sm btn-danger", () =>
          confirmAction({
            title: "Delete route " + route.alias,
            danger: true,
            message: "Clients still sending this alias will fall through to the default route, or be refused, depending on [router] unknown_model.",
            lines: [["targets", (route.targets || []).length]],
            confirmLabel: "Delete",
            run: async () => {
              const d = await act("/v1/routes/" + encodeURIComponent(route.alias), { method: "DELETE" }, "route deleted");
              if (d.ok) await firstPaint();
            },
          }),
        ),
      );
    }
    add(summary, el("div", "hint", "Save is a PUT: hot, no restart. A refusal names the field and the fix."), acts);
  });
}

/** Bind a backend to an alias — the one-click "make this the thing my agent talks to". */
function openBindEditor(backend) {
  openEditor("Bind " + backend.id, (body, summary) => {
    const routes = (S.snap && S.snap.routes) || [];
    const opts = routes.map((r) => [r.alias, r.alias + (r.is_default ? " (default)" : "")]);
    opts.unshift(["", "a new alias…"]);
    const sel = select(opts, routes.length ? routes[0].alias : "", null);
    const fresh = input("text", "", null, { placeholder: "new alias, e.g. coder" });
    const front = input("checkbox", null, null, {});
    front.checked = true;
    const lab = el("label");
    add(lab, front, el("span", null, "put it first in the chain"));
    body.replaceChildren(
      el("div", "hint", "Binding edits the routing table and takes effect immediately."),
      field("existing alias", sel),
      field("or a new alias", fresh),
      lab,
    );
    summary.hidden = false;
    add(
      summary,
      btn("Bind", "⇄", "btn-primary", async () => {
        const alias = slug(fresh.value || sel.value, "");
        if (!alias) {
          toast("choose or type an alias", "bad");
          return;
        }
        let route = routes.find((r) => r.alias === alias);
        if (!route) {
          route = {
            alias: alias,
            targets: [],
            strategy: "first_healthy",
            filter: { require_tags: [], exclude_tags: [], max_cost_per_mtok: null, min_ctx: null, require_vision: false, require_tools: false },
            retry: { attempts: 2, failover: true, honor_retry_after: true },
            is_default: false,
            description: null,
          };
        } else {
          route = JSON.parse(JSON.stringify(route));
        }
        route.targets = route.targets.filter((t) => !(t.backend.sel === "id" && t.backend.value === backend.id));
        const target = { backend: { sel: "id", value: backend.id }, model: null, weight: 1 };
        if (front.checked) route.targets.unshift(target);
        else route.targets.push(target);
        if (await saveRoute(route, false)) {
          closeEditor();
          await firstPaint();
        }
      }),
    );
  });
}

/** Register a plain OpenAI-compatible URL as a backend. */
function openNodeEditor() {
  openEditor("Register a URL", (body, summary) => {
    const url = input("text", "", null, { placeholder: "http://192.168.1.20:8080" });
    const label = input("text", "", null, { placeholder: "the box in the study" });
    const proto = select(
      [
        ["open_ai", "OpenAI-compatible"],
        ["anthropic", "Anthropic Messages"],
      ],
      "open_ai",
      null,
    );
    const envVar = input("text", "", null, { placeholder: "OPENAI_API_KEY (optional)" });
    body.replaceChildren(
      el("div", "hint", "The base URL is stored WITHOUT a trailing /v1 — the relay joins the segments itself."),
      field("base URL", url),
      field("label", label),
      field("protocol", proto),
      field("credential: environment variable name", envVar),
    );
    summary.hidden = false;
    add(
      summary,
      btn("Register", "＋", "btn-primary", async () => {
        const spec = {
          kind: "node",
          base_url: url.value.replace(/\/+v1\/?$/, "").replace(/\/+$/, ""),
          credential: envVar.value ? { kind: "env", var: envVar.value } : { kind: "none" },
          label: label.value || url.value,
          declared_models: [],
          protocol: proto.value,
        };
        const r = await act("/v1/endpoints", { method: "POST", body: spec }, "registered");
        if (r.ok) {
          closeEditor();
          await firstPaint();
        }
      }),
    );
  });
}

/** Recipe editor: structured for a local llama.cpp plan, JSON for the rest. */
function openRecipeEditor(existing, isNew) {
  const now = Math.floor(Date.now() / 1000);
  const r = existing
    ? JSON.parse(JSON.stringify(existing))
    : {
        id: "",
        label: "",
        description: null,
        kind: JSON.parse(JSON.stringify(Object.assign(localSpecTemplate(), { kind: "local" }))),
        provenance: { discovered_at_unix: now, size_bytes: null, source: "web ui", fit: null },
        created_at_unix: now,
        updated_at_unix: now,
      };
  const creating = !existing || isNew;
  openEditor(creating ? "New recipe" : "Recipe " + r.label, (body, summary) => {
    const kids = [];
    const s0 = el("div", "section");
    add(
      s0,
      field(
        "label",
        input("text", r.label, (v) => {
          r.label = v;
          if (creating) r.id = slug(v, "recipe");
        }),
      ),
    );
    add(
      s0,
      field(
        "description",
        input("text", r.description || "", (v) => {
          r.description = v || null;
        }),
      ),
    );
    kids.push(s0);

    if (r.kind.kind === "local") {
      const s1 = el("div", "section");
      add(s1, el("h3", null, "Local llama.cpp plan"));
      const f = el("div", "fields");
      const models = S.localModels.map((m) => [m.shards[0] ? m.shards[0].path : "", m.name]);
      models.unshift([r.kind.model_path || "", r.kind.model_path ? "keep: " + r.kind.model_path : "choose a model…"]);
      add(
        f,
        field(
          "model path",
          select(models, r.kind.model_path, (v) => {
            r.kind.model_path = v;
          }),
        ),
      );
      const builds = ((S.snap && S.snap.rig.builds) || []).map((b) => [b.id, b.label]);
      builds.unshift(["", "(none)"]);
      add(
        f,
        field(
          "build",
          select(builds, r.kind.build, (v) => {
            r.kind.build = v;
          }),
        ),
      );
      add(
        f,
        field(
          "context",
          input("number", r.kind.ctx || "", (v) => {
            r.kind.ctx = v === "" ? null : Number(v);
          }),
        ),
      );
      add(
        f,
        field(
          "parallel",
          input("number", r.kind.parallel || "", (v) => {
            r.kind.parallel = v === "" ? null : Number(v);
          }),
        ),
      );
      add(
        f,
        field(
          "KV type",
          select(
            ["f32", "f16", "bf16", "q8_0", "q5_1", "q5_0", "q4_1", "q4_0", "iq4_nl"].map((k) => [k, k]),
            r.kind.kv_type || "f16",
            (v) => {
              r.kind.kv_type = v;
            },
          ),
        ),
      );
      add(
        f,
        field(
          "mode",
          select(
            [
              ["thinking", "thinking"],
              ["coding", "coding"],
              ["nonthinking", "nonthinking"],
              ["raw", "raw"],
            ],
            r.kind.mode || "nonthinking",
            (v) => {
              r.kind.mode = v;
            },
          ),
        ),
      );
      add(
        f,
        field(
          "served alias flag",
          input("text", r.kind.alias_flag || "", (v) => {
            r.kind.alias_flag = v;
          }),
        ),
      );
      add(s1, f);
      kids.push(s1);
    } else {
      const s1 = el("div", "section");
      add(s1, el("h3", null, pretty(r.kind.kind) + " plan"));
      const ta = el("textarea");
      ta.value = JSON.stringify(r.kind, null, 2);
      ta.addEventListener("input", () => {
        try {
          r.kind = JSON.parse(ta.value);
          ta.style.borderColor = "";
        } catch (e) {
          ta.style.borderColor = "var(--critical)";
        }
      });
      add(s1, el("div", "hint", "Edited as JSON: this kind has no dedicated form yet, and a half-form would hide fields."), ta);
      kids.push(s1);
    }
    body.replaceChildren(...kids);

    summary.hidden = false;
    const acts = el("div", "split");
    add(
      acts,
      btn("Save", "✓", "btn-primary", async () => {
        if (!r.label) {
          toast("a label is required", "bad");
          return;
        }
        if (!r.id) r.id = slug(r.label, "recipe");
        const path = creating ? "/v1/recipes" : "/v1/recipes/" + r.id;
        const res = await act(path, { method: creating ? "POST" : "PUT", body: r }, "recipe saved");
        if (res.ok) {
          closeEditor();
          await firstPaint();
        }
      }),
    );
    add(summary, acts);
  });
}

/** The empty local spec a new recipe starts from. */
function localSpecTemplate() {
  return {
    build: "",
    model_path: "",
    mmproj: null,
    alias_flag: "model",
    host: "127.0.0.1",
    port: null,
    ctx: 8192,
    parallel: 1,
    kv_type: "f16",
    ngl: { ngl: "auto" },
    split: { devices: [], mode: "layer", main_gpu: null, tensor_split: null },
    mode: "nonthinking",
    flash_attn: "auto",
    api_key: null,
    extra_args: [],
  };
}

/** Search-profile editor. */
function openProfileEditor(existing) {
  const p = existing
    ? JSON.parse(JSON.stringify(existing))
    : {
        id: "",
        label: "",
        gpu_names: [],
        num_gpus_min: 1,
        num_gpus_max: 2,
        max_dph: null,
        min_reliability: 0.95,
        min_inet_down: 300,
        min_disk_gb: 60,
        min_cuda: null,
        geo: "any",
        image_type: "prebuilt",
        extra: {},
      };
  const creating = !existing;
  openEditor(creating ? "New search profile" : "Profile " + p.label, (body, summary) => {
    const f = el("div", "fields");
    add(
      f,
      field(
        "label",
        input("text", p.label, (v) => {
          p.label = v;
          if (creating) p.id = slug(v, "profile");
        }),
      ),
    );
    add(
      f,
      field(
        "GPU names (comma)",
        input("text", (p.gpu_names || []).join(","), (v) => {
          p.gpu_names = v.split(",").map((x) => x.trim()).filter(Boolean);
        }, { placeholder: "RTX_4090,H100_SXM" }),
      ),
    );
    add(
      f,
      field(
        "min GPUs",
        input("number", p.num_gpus_min, (v) => {
          p.num_gpus_min = Number(v) || 1;
        }, { min: "1" }),
      ),
    );
    add(
      f,
      field(
        "max GPUs",
        input("number", p.num_gpus_max, (v) => {
          p.num_gpus_max = Number(v) || 1;
        }, { min: "1" }),
      ),
    );
    add(
      f,
      field(
        "max $/hr",
        input("text", p.max_dph === null || p.max_dph === undefined ? "" : (p.max_dph / 1e6).toFixed(3), (v) => {
          const n = Number(v);
          p.max_dph = v === "" || !isFinite(n) ? null : Math.round(n * 1e6);
        }, { placeholder: "1.200" }),
        "stored as integer micro-USD, so nothing drifts",
      ),
    );
    add(
      f,
      field(
        "min reliability",
        input("number", p.min_reliability, (v) => {
          p.min_reliability = Number(v) || 0;
        }, { step: "0.01", min: "0", max: "1" }),
      ),
    );
    add(
      f,
      field(
        "min down (Mbps)",
        input("number", p.min_inet_down, (v) => {
          p.min_inet_down = Number(v) || 0;
        }),
      ),
    );
    add(
      f,
      field(
        "min disk (GB)",
        input("number", p.min_disk_gb, (v) => {
          p.min_disk_gb = Number(v) || 0;
        }),
      ),
    );
    add(
      f,
      field(
        "min CUDA",
        input("text", p.min_cuda === null ? "" : p.min_cuda, (v) => {
          p.min_cuda = v === "" ? null : Number(v);
        }, { placeholder: "12.4" }),
      ),
    );
    add(
      f,
      field(
        "geography",
        select(
          [
            ["any", "anywhere"],
            ["eu_nordic", "EU — Nordic"],
            ["eu", "EU"],
            ["us", "US"],
          ],
          typeof p.geo === "string" ? p.geo : "any",
          (v) => {
            p.geo = v;
          },
        ),
      ),
    );
    add(
      f,
      field(
        "image family",
        select(
          [
            ["prebuilt", "prebuilt llama.cpp"],
            ["builder", "build from a fork (+12–18 min)"],
            ["vllm", "vLLM"],
          ],
          p.image_type,
          (v) => {
            p.image_type = v;
          },
        ),
      ),
    );
    body.replaceChildren(el("div", "hint", "A profile is a saved market query. Searching one never spends anything."), f);

    summary.hidden = false;
    add(
      summary,
      btn("Save", "✓", "btn-primary", async () => {
        if (!p.label) {
          toast("a label is required", "bad");
          return;
        }
        if (!p.id) p.id = slug(p.label, "profile");
        const path = creating ? "/v1/profiles" : "/v1/profiles/" + p.id;
        const res = await act(path, { method: creating ? "POST" : "PUT", body: p }, "profile saved");
        if (res.ok) {
          closeEditor();
          await firstPaint();
        }
      }),
    );
  });
}

/** Logs for one backend: a tail, a follow toggle and a filter box. */
function openLogs(id) {
  S.logs.id = id;
  S.logs.filter = "";
  S.logs.follow = false;
  S.logs.lines = [];
  openEditor("Logs — " + id, async (body) => {
    const controls = el("div", "split");
    const filter = input("search", "", (v) => {
      S.logs.filter = v;
      paint();
    }, { placeholder: "filter lines…" });
    const followBtn = btn("Follow", "≋", "btn-sm", () => {
      S.logs.follow = !S.logs.follow;
      followBtn.classList.toggle("btn-primary", S.logs.follow);
      if (S.logs.follow) {
        S.logs.src = id;
        sseFetch("/v1/backends/" + id + "/logs?follow=1&tail=200", null, (d) => {
          const line = typeof d === "string" ? d : d.line || d.raw || JSON.stringify(d);
          S.logs.lines.push(line);
          paint();
        });
      } else {
        S.logs.src = null;
      }
    });
    const pre = el("pre", "logbox", "loading…");
    function paint() {
      const f = S.logs.filter.toLowerCase();
      const shown = S.logs.lines.filter((l) => !f || l.toLowerCase().indexOf(f) >= 0);
      pre.textContent = shown.slice(-800).join("\n") || "(nothing)";
      pre.scrollTop = pre.scrollHeight;
    }
    add(controls, filter, followBtn);
    body.replaceChildren(controls, pre);
    const r = await api("/v1/backends/" + id + "/logs?tail=200");
    S.logs.lines = typeof r.data === "string" ? r.data.split("\n") : [];
    paint();
  });
}

/** Logs for a rented instance, streamed. */
function openInstanceLogs(id) {
  S.logs.lines = [];
  openEditor("Instance logs — " + id, (body) => {
    const pre = el("pre", "logbox", "streaming…");
    body.replaceChildren(pre);
    sseFetch("/v1/vast/instances/" + id + "/log?follow=1", null, (d) => {
      const line = typeof d === "string" ? d : d.line || d.raw || JSON.stringify(d);
      S.logs.lines.push(line);
      pre.textContent = S.logs.lines.slice(-800).join("\n");
      pre.scrollTop = pre.scrollHeight;
    });
  });
}

/**
 * A destructive-but-free action: stopping a process, deleting a draft, forgetting a record.
 *
 * Deliberately NOT `window.confirm`: a native dialog blocks the event loop, so the live
 * event stream stops arriving behind it, and it cannot show the id it is about to destroy in
 * anything but one line of unstyled text. Money actions go through [`confirmMoney`] instead,
 * which additionally requires a typed word.
 */
function confirmAction(opts) {
  openEditor(opts.title, (body, summary) => {
    const kids = [];
    const b = el("div", "banner " + (opts.danger ? "banner-bad" : "banner-warn"));
    add(b, badge(opts.danger ? "destructive" : "careful", opts.danger ? "⛔" : "!", opts.danger ? "critical" : "warn"));
    add(b, el("span", null, opts.message));
    kids.push(b);
    if (opts.lines) kids.push(kv(opts.lines));
    if (opts.note) kids.push(el("div", "hint", opts.note));
    body.replaceChildren(...kids);
    summary.hidden = false;
    add(
      summary,
      el("div", "split"),
    );
    const row = summary.lastChild;
    add(
      row,
      btn(opts.confirmLabel || "Confirm", "✓", opts.danger ? "btn-danger" : "btn-primary", async () => {
        await opts.run();
        closeEditor();
      }),
      btn("Cancel", "✕", "btn-ghost", () => closeEditor()),
    );
  });
}

/** Ask for one string — an alias, a label — without a native `prompt()`. */
function promptDrawer(opts) {
  openEditor(opts.title, (body, summary) => {
    const box = input("text", opts.value || "", null, { placeholder: opts.placeholder || "" });
    body.replaceChildren(el("div", "hint", opts.message || ""), field(opts.label, box));
    summary.hidden = false;
    const row = el("div", "split");
    add(
      row,
      btn(opts.confirmLabel || "OK", "✓", "btn-primary", async () => {
        const v = box.value.trim();
        if (!v && !opts.allowEmpty) {
          toast(opts.label + " is required", "bad");
          return;
        }
        closeEditor();
        await opts.run(v);
      }),
      btn("Cancel", "✕", "btn-ghost", () => closeEditor()),
    );
    add(summary, row);
    box.addEventListener("keydown", (e) => {
      if (e.key === "Enter") row.firstChild.click();
    });
  });
}

/**
 * The money gate: every figure on screen, and a word typed by a human, before the request.
 *
 * Nothing in this file spends without going through here.
 */
function confirmMoney(plan) {
  openEditor(plan.title, (body, summary) => {
    const kids = [];
    const warn = el("div", "banner " + (plan.danger ? "banner-bad" : "banner-warn"));
    add(warn, badge("real money", "＄", "critical"), el("span", null, "This changes what is billed. Nothing is sent until you type the word."));
    kids.push(warn);
    kids.push(kv(plan.lines));
    const typed = input("text", "", (v) => {
      go.disabled = v.trim().toLowerCase() !== plan.word;
    }, { placeholder: "type " + plan.word + " to enable the button" });
    kids.push(field("confirmation", typed));
    body.replaceChildren(...kids);

    const go = btn(plan.confirmLabel || "Confirm", "✓", "btn-danger", async () => {
      go.disabled = true;
      await plan.run();
      closeEditor();
      await firstPaint();
    });
    go.disabled = true;
    summary.hidden = false;
    add(summary, el("div", "hint", "The daemon refuses this anyway without an explicit approval — this is the second lock, not the only one."), go);
  });
}


// ---------------------------------------------------------------------------------------
// boot
// ---------------------------------------------------------------------------------------

/** Wire the shell's static controls, then paint. */
function init() {
  for (const t of document.querySelectorAll(".tab")) {
    t.addEventListener("click", () => show(t.dataset.panel));
  }
  $("rb-copy").addEventListener("click", async () => {
    const url = $("rb-base-url").textContent;
    flashCopied(await copyText(url));
  });
  $("rb-copy-both").addEventListener("click", async () => {
    const url = $("rb-base-url").textContent;
    flashCopied(await copyText("OPENAI_BASE_URL=" + url + "\nOPENAI_API_KEY=not-needed"));
  });
  $("rb-default-alias").addEventListener("change", async (e) => {
    const alias = e.target.value;
    if (!alias) return;
    const r = await act("/v1/routes/default", { method: "POST", body: { alias: alias } }, "default is now " + alias);
    if (r.ok && S.snap) S.snap.proxy.default_alias = alias;
    scheduleRender();
  });
  $("open-launch").addEventListener("click", () => openLaunch());
  $("launch-close").addEventListener("click", () => closeLaunch());
  $("edit-close").addEventListener("click", () => closeEditor());
  for (const t of document.querySelectorAll(".dtab")) {
    t.addEventListener("click", () => {
      if (!S.launch) resetLaunch();
      S.launch.tab = t.dataset.ltab;
      if (t.dataset.ltab === "rent" && !S.localModels.length) loadLocalModels(false);
      renderLaunch();
      if (S.launch.tab === "local") solveFit();
    });
  }
  window.addEventListener("hashchange", () => {
    const h = location.hash.replace(/^#/, "");
    if (h === "launch") openLaunch();
    else if (h) show(h);
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      if (!$("drawer-edit").hidden) closeEditor();
      else if (!$("drawer-launch").hidden) closeLaunch();
    }
  });

  const h = location.hash.replace(/^#/, "");
  S.panel = PANELS.indexOf(h) >= 0 ? h : "routes";

  firstPaint().then(() => {
    connectWS();
    loadPanel(S.panel, false);
    // The request ring feeds the Routes and Backends percentile columns too, so it is read
    // once at startup rather than only when the Live requests tab is first opened.
    loadPanel("requests", false);
    if (h === "launch") openLaunch();
  });

  // Relative timestamps go stale silently; a minute is close enough to honest.
  setInterval(render, 60000);
  scheduleRender();
}

document.addEventListener("DOMContentLoaded", init);
