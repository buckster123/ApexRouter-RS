# ApexRouter-RS — Full Repository Audit

**Date:** 2026-08-07  
**Scope:** Entire workspace (`main` @ `4ff55a8`, working tree clean)  
**Mode:** Read-only code review + live loopback probes against a running daemon  
**Money:** No vast create/modify/destroy. No writes to `~/.vastai-gguf`.  
**Method:** Four parallel specialist auditors (security/money, request path, control plane, core/providers) + orchestrator greps, live HTTP checks, and independent verification of critical claims.

---

## 0. Executive summary

ApexRouter-RS is an unusually disciplined systems product for its age: ~118k LOC of Rust, nine crates, ~1.5k+ unit/integration tests, binding charter decisions (D1–D18), and a culture that encodes past failures as types and mount guards rather than prose. The five product invariants are largely real; money-safety on the **library** rent path is structurally sound (`SpendApproval` unforgeable, ledger reserve-before-create, `new_contract` not `id`).

The recurring defect class this repo already knows — **“implemented, unit-tested, never composed”** — still appears, now outside `v1_routes()`:

| Class | Where it hits today |
|---|---|
| Gate written, not mounted | Proxy `auth::mutation_gate` (DNS rebinding on `POST /switch`) — **live-confirmed 2026-08-07** |
| Feature written, not called | `LiveBackend.retry_bucket.try_take` never used on the request path |
| Helper written, wrong call site | `register_started` used by swap/vast; endpoint/recipe start still uses `register_backend` |
| Wiring written, not at startup | Jobs `ensure_wired` / `attach_store` lazy and skipped by several spawn sites |
| Durability incomplete | Ledger append `flush` without `fsync` |

**Overall grade: A− foundations, B+ composition.** Ship-quality core; several high-severity composition gaps before treating mk1 as “closed under adversarial local browser + agent spend.”

---

## 1. Inventory

| Metric | Value |
|---|---|
| Crates | 9 (`protocol`, `core`, `router`, `providers`, `client`, `server`, `cli`, `slint` out of default-members, `tests-support`) |
| Rust sources | ~160 files under `crates/`, ~118k LOC |
| Test attributes | ~1,578 `#[test]` / `#[tokio::test]` |
| Release binary | `target/release/apexrouter` 0.1.0 present; live daemon ~20h uptime during audit |
| Tag posture | mk1 release notes dated 2026-07-31; post-tag work: garden/studio campaigns, mk1.1 defect closes |
| Forward charters | `GARDEN.md` (mk2 model garden), `STUDIO.md` (S1–S18, Comfy lanes) — design, not full code |

Live health during audit:

- Control `GET /health` → house envelope, product `apexrouter`
- Proxy `GET /health` → legacy LocalRouter-shaped body (`provider: vast-gguf`)
- Control `GET /v1/snapshot` → 200
- Control `GET /metrics` → **404** (documented PENDING)

---

## 2. Invariant compliance matrix

| # | Invariant | Verdict | Notes |
|---|---|---|---|
| 1 | **One resolver** | **Pass** | Single `RoutingTable::resolve` in `router/src/resolve.rs`. Proxy, models, compat “active”, and control route-test share it. Other `resolve` names are paths/creds/recipes. |
| 2 | **Request path never touches FS** | **Partial fail** | Core resolve/attempt/relay clean. **`credential_for` does `tokio::fs::read_to_string` per attempt for `CredentialSource::File`.** Legacy `/health`/`/providers`/`/switch` also touch FS (legacy surface). |
| 3 | **Persisted facts, never status** | **Pass** | `EndpointRecord` holds `desired` + proc identity; no `status: "running"` on disk. Vast live statuses are API observations, not endpoint records. |
| 4 | **Money safety** | **Partial** | Library rent: SpendApproval + reserve-before-bill + no shutdown destroy → strong. Gaps: offer dph vs approval (rent), HTTP destroy order, boot-watchdog auto-destroy vs D10 letter, ledger no fsync, `?source=` spoofing human gate. |
| 5 | **One XDG state dir** | **Pass** | Defaults under `~/.local/state/apexrouter`; tests use TempDir; `mirror_usage_log` default off. |

---

## 3. Critical findings

### C1 — Proxy mutation gate implemented but never mounted (DNS rebinding → `POST /switch`)

**Severity:** Critical  
**Confidence:** High (code + **live probe**)  
**Class:** Same shape as historical unmounted `api/*` modules  

**Where**

- Gate defined: `crates/apexrouter-server/src/auth.rs` (`mutation_gate`, ~281)
- Docs claim: `proxy_router(...).layer(mutation_gate)`
- Assembly: `crates/apexrouter-server/src/lib.rs` `proxy_app` (~861–868) — **only** `cors_middleware` + tracing
- `ListenerBind` never installed on either listener
- Handler-side rule 2 only: `compat.rs` Origin/Sec-Fetch-Site for `/switch`

**Live probe (2026-08-07, real daemon)**

```text
POST http://127.0.0.1:8888/switch
  Host: evil.example.com:8888
  Origin: http://evil.example.com:8888
→ 400 {"error":"Unknown provider: null"}     # handler RAN — not 403

POST http://127.0.0.1:2739/v1/routes/default
  Host: evil.example.com:2739
  Origin: http://evil.example.com:2739
→ 403 Host ... not an address this daemon listens on (possible DNS rebinding)
```

Control plane is correct. Proxy is not. Under real DNS rebinding, the page is same-origin with the evil Host, so rule 2 passes and rule 1 never runs.

**Impact:** `POST /switch` retargets the default alias and can persist keys / change `base_url` (credential-exfiltration primitive per own docs). ARCHITECTURE §9.3 threat model not met on the proxy listener.

**Fix (recommended)**

```rust
// proxy_app — conceptual
apexrouter_router::proxy_router(...)
    .layer(Extension(ListenerBind(proxy_addr)))
    .layer(from_fn_with_state(state.clone(), auth::mutation_gate))
    .layer(from_fn_with_state(state.clone(), cors_middleware))
```

Add hermetic integration test: foreign Host + same-origin Origin → 403 on `/switch`.  
Also insert `ListenerBind` on the control listener so Host/port classification is not config-fallback only.

**Priority:** P0 — same discipline as `mounted_routes.rs`, applied to proxy assembly.

---

## 4. High findings

### H1 — Rent does not enforce offer `dph_total` ≤ approval ceiling

**Where:** `providers/src/vast/rent.rs` `rent()` (~231–280)  
**What:** Checks `req.max_usd_per_hour ≤ approval.max_usd_per_hour()`, then `create(offer_id, …)` with **no** fetch/compare of the offer’s live `dph_total`. `preview()` only **warns**. Wake/attach paths **do** check live dph.  
**Impact:** SpendApproval is hollow for rent if a caller sets `max_usd_per_hour: 0.40` (under default $4 daemon ceiling) and an expensive `offer_id`.  
**Fix:** Before reserve/create, load offer (or require a just-searched snapshot) and refuse if `dph_total > approval` (and ideally `> req.max`). Unit-test $0.40 approval + $3.34 offer.  
**Confidence:** High  

### H2 — Control-plane destroy path ≠ library destroy (order + error handling)

**Where:** `server/src/api/vast.rs` DELETE vs `providers/src/vast/rent.rs` `destroy_within`  
**What:** HTTP path can destroy then ledger-append; library path appends `DestroyRequested` first. `let _ = ledger.append` swallows errors.  
**Impact:** Crash mid-path leaves intent/record skew; two authorities (the bug class D5/D16 exist to kill).  
**Fix:** One implementation — call library destroy from API; surface ledger errors as 5xx.  
**Confidence:** High  

### H3 — `require_human_confirm` bypassable via `?source=`

**Where:** `api/vast.rs` source parsing; `money.rs` only gates `Mcp { human_cleared: false }`  
**What:** Client can send `?source=cli` or omit (→ Api) and rent without an approval job when the flag is on.  
**Impact:** Operators who set the flag believe agents cannot spend alone; any loopback process can.  
**Fix:** Derive source from authenticated surface, not a free query param; treat untrusted HTTP as MCP-equivalent when flag is on.  
**Confidence:** High (behavior may be intentional for “MCP only”; spoofing is still real)  
**Note:** Default `require_human_confirm = false`; ceiling remains the hard backstop.

### H4 — Endpoint/recipe start uses `register_backend`, not `register_started`

**Where:** `api/endpoints.rs` (~119, 136); `api/catalog.rs` recipe instantiate; contrast swap/vast which use `register_started`  
**What:** After drain, old id can keep `accepting = false`; new process registers Ready but never dispatches → `no_healthy_backend` with a healthy process.  
**Fix:** All new-process start paths → `register_started`.  
**Confidence:** High  

### H5 — Jobs registry not wired at daemon start; several spawns skip wiring

**Where:** `jobs.rs` `ensure_wired` / `attach_store`; wired: jobs API, hf, some catalog; **not** at `build_state`; spawn gaps: endpoints `?no_wait`, compare `?no_wait`, vast rent/wake  
**Impact:** UI boots endpoints with `no_wait=true`; jobs may be memory-only; crash recovery of Pending rows deferred; WS job events quiet.  
**Fix:** `ensure_wired` once in `build_state`/`run` before bind; make `spawn_with` always persist.  
**Confidence:** High  

### H6 — `retry_bucket` is dead on the request path

**Where:** `registry.rs` field; `limits.rs` `try_take` only unit-tested; never called from handler/attempt  
**Impact:** Documented per-backend retry storm budget is a no-op.  
**Fix:** Call `try_take` on retry legs; test failover exhaustion.  
**Confidence:** High  

### H7 — Ledger append lacks `fsync`

**Where:** `core/src/ledger.rs` — flock → write → `flush`, no `sync_all`/`fdatasync`  
**Contrast:** `store::write_atomic`, secrets, lockfile owner all fsync.  
**Impact:** Power loss between successful `create` and durable `Reserved` reopens A1 (box billing, no local record) — the failure the ledger exists to prevent.  
**Fix:** `file.sync_all()` (or fdatasync) after write, before unlock; same on Drop/`OrphanSuspect`.  
**Confidence:** High  

### H8 — Rule-4 `ImplicitMulti` alert never emitted

**Where:** `resolve.rs` documents caller must raise one-shot Alert; handler never checks `plan.reason`  
**Impact:** Multi-backend collisions after a rental are silent except header inspection.  
**Fix:** Emit deduped `Event::Alert` on `ImplicitMulti`.  
**Confidence:** High  

### H9 — Invariant 2 broken for `CredentialSource::File`

**Where:** `handler.rs` `credential_for` (~941–954)  
**Impact:** Hot-path FS per attempt; latency/jitter; invariant false under File creds.  
**Fix:** Materialize secrets into `LiveBackend` at register/probe time.  
**Confidence:** High  

---

## 5. Medium findings

| ID | Issue | Confidence |
|---|---|---|
| M1 | CHARTER **D10** says nothing that costs money is ever auto-destroyed; **boot watchdog** intentionally destroys at `max_boot_secs` / fatal status stall. Product intent (stop the meter) is correct; **letter of D10 is false**. Amend charter with dated carve-out. | High |
| M2 | D10 “startup reconciles against live vast account” not in `reconcile_on_start` (local endpoints only). Orphans need doctor / `vast ls --orphans`. | Med |
| M3 | `/metrics` still written, still not mounted (OpenAPI PENDING). Live 404. Assets correctly reserve path. | Certain |
| M4 | CLI PENDING / clap comments still claim MCP not built; MCP is live via stdio intercept before clap. | High |
| M5 | `mounted_routes.rs` comment still lists `POST /v1/migrate` as unbuilt; it is mounted. | Certain |
| M6 | Auth refusal envelope `{"ok":false,"error"}` vs control `ErrorEnvelope` (`error.kind`) dual shapes. | Med |
| M7 | `X-ApexRouter-Route` stamp loses alias/reason on some park / NoHealthy edges (`auto\|-`, `-\|-`). | High |
| M8 | Docs: rule 5 reason token claimed `default_fallback`, code emits `legacy_model_name`. | High |
| M9 | `RouterCfg.request_usage` config key never read by router. | High |
| M10 | Ledger `append` O(n) full scan under exclusive lock for seq assignment. | Med |
| M11 | Usage log also no fsync (honesty, not billing-leak class). | Med |
| M12 | SpendApproval credit check = one hour of rate, not projected burn. | Med |
| M13 | Multi-GPU physical_key heuristics residual mis-budget risk. | Med |
| M14 | ARCHITECTURE last full cross-check **2026-07-31**; fleet, migrate, park/wake, register_started partial, etc. post-date. | High |

---

## 6. Low / nits / accepted charter risks

- Single bearer is always Admin (scopes half-built) — documented mk1.  
- `?token=` query presentation — residual URL leakage; TraceLayer tested to omit query.  
- Proxy open for inference on loopback — product.  
- Legacy three routes removed at 1.0 (D9) — still live; no `legacy.traffic` doctor check yet.  
- MCP smaller than CLI (no park/wake/tunnel/approvals tools) — product prioritization.  
- Studio/garden charters not implemented as full code surfaces yet.  
- `require_human_confirm` default **false**.  
- Production `unwrap` hygiene: greps mostly hit `#[cfg(test)]` modules co-located in lib files; not a production epidemic.  
- No `sh -c` / `.arg("-c")` in Rust sources.  
- ROCm free>total: clamped at discovery; CI gate against `total - free`.  

---

## 7. What is solid (do not “fix” these)

### Architecture & process model

- Two listeners; proxy catch-all as `.fallback`, not route (axum 0.8 overlap avoided).  
- Children `setsid`; kill_children_on_exit default false; vast never destroyed on shutdown.  
- Reconcile-before-bind for local endpoints; vast network reconcile not on critical path.  
- `flock` + owner record; non-loopback bind refuses without token (tested).  

### Money (library path)

- `SpendApproval`: private fields, `#[non_exhaustive]`, only `confirm()`, compile_fail in docs.  
- Reserve → create → commit; Drop → OrphanSuspect; Critical on create failure.  
- `new_contract` not `id` — explicit parse + NO_CONTRACT messaging + tests.  
- Mettered bandwidth warning on preview (garden R2a lesson).  
- Tests panic if money paths hit live create/destroy.  

### Request path

- No retry past first committed byte (`PreFlight` not Clone).  
- Timeouts: connect 5s, headers 600s, idle 300s; **no total stream timeout**.  
- Clean EOF without `data: [DONE]` → synthetic error frame.  
- `X-Usage` buffered only; streams deferred.  
- `/slots` 403 redacted.  
- `/v1` normalisation **adds** missing prefix (both base URL forms).  
- Anthropic: tools default on in config; reverse OpenAI→Anthropic 501; count_tokens 501; tools refused loudly when off.  
- Warm queue re-arms on **launch future pending** (not stopwatch) — the 74,550 outage lesson is encoded and tested.  

### Control plane composition (fixed class)

- **`mounted_routes.rs` is the real guard** — source-scanned inventory, booted daemon, 403/405 probes without handlers (hermetic even for `/v1/vast/*`).  
- All current `api/*` `pub fn router()` modules are merged in `v1_routes()` including migrate.  
- OpenAPI bidirectional gate; only `/metrics` PENDING.  

### Core craftsmanship

- Atomic store 0600 + fsync; Secret redacts Debug; argv-only exec; LAST `)` proc parse; boot_id identity.  
- One `fit()`, one argv builder, LD_LIBRARY_PATH=dirname(binary), cwd=$STATE.  
- Capability from devices/libs, not directory names.  
- Honesty types end-to-end (`TokenCount`, `CostEstimate`).  
- Hermetic `test_config()` pattern (together → closed loopback).  

---

## 8. Mount / surface matrix (control plane)

| Module | Merged | OpenAPI | CLI | MCP | UI |
|---|---|---|---|---|---|
| snapshot/backends/routes/endpoints | Y | Y | Y | partial | Y |
| rig / fit / catalog | Y | Y | Y | Y | Y |
| usage / requests / jobs | Y | Y | usage only | usage | Y |
| vast (+ park/wake/tunnel API) | Y | Y | Y | offers/rent/destroy | Y |
| hf / providers / checks / compare | Y | Y | Y | partial | Y |
| migrate | Y | Y | Y (offline Pure) | — | — |
| **/metrics** | **N** | PENDING | — | — | reserved path |

MCP: 24 tools, all `apexrouter_` prefixed; definitions ↔ dispatch tested. Stdio intercept before clap.

---

## 9. Hermeticity

**Strong.** Shared test configs redirect together/vast to closed loopback; vast fixtures panic on accidental destroy; live vast is `#[ignore]` + env gate; stage gates encode VRAM and hermeticity lessons. Residual risk: any new test using bare `Config::default()` with real `$TOGETHER_API_KEY` (known Stage-3 class) — consider a workspace lint.

---

## 10. Doc drift (ARCHITECTURE / CHARTER / CLI)

| Claim | Reality 2026-08-07 |
|---|---|
| Mutation gate on **both** listeners | Control yes; proxy **no** (C1) |
| `/metrics` not mounted | Still true |
| migrate mounted | True (guard comment stale) |
| MCP not in build (CLI PENDING) | False — MCP live |
| D10 never auto-destroy | Boot watchdog exception |
| D10 startup vast reconcile | Not automatic |
| ARCHITECTURE cross-check 2026-07-31 | Stale relative to mk1.1 / garden / fleet / migrate API |
| Invariant 2 absolute | File credentials exception |

---

## 11. Recommended fix backlog

### P0 (do next — correctness / security / money durability)

1. **Wire `mutation_gate` + `ListenerBind` on proxy** (and control); live-style rebinding test for `/switch`.  
2. **`Ledger::append` fsync** before unlock.  
3. **Rent: refuse if offer `dph_total` > approval** (and request max).  
4. **Single destroy path** (API → library `destroy_within`); no swallowed ledger errors.  

### P1 (ops reliability)

5. **`jobs.ensure_wired` in `build_state`**; all spawn sites persist.  
6. **`register_started` on endpoint/recipe start**.  
7. **Wire `retry_bucket.try_take`** + test.  
8. **Stop trusting `?source=` for human gate** (or document + refuse non-MCP spoof when flag on).  
9. **ImplicitMulti one-shot Alert** in handler.  
10. **Materialize File credentials** off the request path.  

### P2 (hygiene / product finish)

11. Mount `/metrics` or keep PENDING but drop from “what shipped” lists that imply it works.  
12. CHARTER D10 dated carve-out for boot watchdog; optional `never_auto_destroy` hard switch.  
13. Startup ledger↔fleet **alert-only** reconcile when vast key present.  
14. Doc fixes: rule 5 reason, max_inflight semantics, AnthropicCfg default comment, MCP PENDING, mounted_routes migrate comment, ARCHITECTURE re-cross-check.  
15. Stamp completeness on park / NoHealthy.  
16. Implement or tombstone `request_usage`.  

### P3 (scale / mk2)

17. Ledger O(1) seq (sidecar / tail max).  
18. Multi-GPU soak tests for physical_key.  
19. VRAM reservation lock across concurrent plan/up.  
20. Studio/garden implementation per STUDIO.md / GARDEN.md (out of mk1 scope).  

---

## 12. Suggested work packaging

| Option | Scope | Outcome |
|---|---|---|
| **(a) Security & money patch** (recommended first) | C1, H1, H2, H7, H3 policy | Closes rebinding + hollow approval + destroy skew + durable ledger |
| **(b) Composition patch** | H4, H5, H6, H8, H9 | Closes the “written but not wired” class on jobs/register/retry/creds/alert |
| **(c) Doc & surface hygiene** | M3–M5, M8–M9, ARCHITECTURE re-check, D10 amend | Stops the next agent rediscovering false docs |
| Full mk1.2 tag | (a)+(b)+(c) + residual list from RELEASE-NOTES §6.3 | Honest “composition closed” tag |

---

## 13. Bottom line

This codebase already **learned** its expensive lessons (billing leak, VRAM GTT, warm-queue stopwatch, unmounted APIs, hermetic Stage-3 dial-out) and turned most of them into structure. The audit’s message is not “start over” — it is **finish the composition layer** with the same ruthlessness already applied to `mounted_routes.rs`.

The single best sentence:

> **The next bugs are not in algorithms; they are in the one-line `.layer(...)` / `register_started` / `fsync` / `try_take` calls that unit tests of the module-in-isolation cannot see.**

Live-confirmed P0: **mount the proxy mutation gate.**

---

## Appendix A — Live probes (audit day)

| Probe | Result |
|---|---|
| `GET :2739/health` | 200 house shape |
| `GET :8888/health` | 200 legacy shape |
| `GET :2739/v1/snapshot` | 200 |
| `GET :2739/metrics` | 404 |
| `PATCH :2739/v1/routes/auto` + foreign Origin | 403 gate |
| `POST :8888/switch` + evil Host/Origin | **400 handler ran** (C1) |
| `POST :2739/v1/routes/default` + evil Host/Origin | **403 rebinding** |

## Appendix B — Specialist agents

| Agent | Focus |
|---|---|
| Security & money | auth, SpendApproval, vast rent/destroy, credentials |
| Request path | resolve, relay, warm queue, anthropic, retry/breaker |
| Control plane | v1_routes, CLI, MCP, jobs, UI, openapi |
| Core & providers | ledger, fit, argv, discover, hermeticity, honesty types |

No source files were modified by this audit.
