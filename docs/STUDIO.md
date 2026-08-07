# STUDIO.md — the studio charter

Status: adopted 2026-08-02, from the ultracode design fleet (5 repo maps, 3 design lenses, 1 synthesis). Amend the decisions S1–S18 with a dated entry, never silently — same
rule as CHARTER.md D1–D18. Where this document disagrees with GARDEN.md G2, G2 wins.

## 1. Mission

One rented box (★ machine 140330: 2× modded-48GB RTX 4090, $0.836/hr, 2 TB disk, Guangdong CN)
runs the full studio posture — an LLM slot, a video lane, an image lane — and the whole garden
reaches it through addresses that never change: LLM through proxy `127.0.0.1:8888` as an alias,
ComfyUI lanes through fixed local tunnel ports. ApexRouter-RS owns the box: rent, park, wake,
tunnels, supervision, money — and never learns ComfyUI's wire shape. Imaginarium-RS owns
generation: it becomes the single aggregator (xAI grok, local-comfy, together.ai) behind its
existing 11 MCP tools plus a `studio_*` director family, with the library as the system of record.
CerebroCortex-RS remembers approved art. ApexOS-RS composes the package. The promise is the
proxy's promise extended: **configure every consumer once, then only ever say
`apexrouter studio up`.**

## 2. Posture — the 96 GB pair (R3 measured, not aspirational)

| Service | Runtime | GPU | In-container port | Local port | VRAM reserved | Measured (R3, warm) |
|---|---|---|---|---|---|---|
| `llm` | llama-server, Qwen3.6-27B q5_k_m + kv q8 | 0 (remainder) | 8000 | 88xx lease → alias `studio-llm` via 8888 | fit()-solved against 48 GB − 23 000 MB − headroom (~20 GB model fits) | ~55–105 tok/s (quant/MTP dependent, prior run on this box) |
| `video` | ComfyUI, Wan 2.2 TI2V-5B | 0 | 8188 | **8811** (fixed) | 23 000 MB (measured 21.6 GB + margin) | 1280×704×81f in **95 s** |
| `image` | ComfyUI, Qwen-Image 20B fp8 | 1 | 8189 | **8812** (fixed) | 32 000 MB (measured 30.5 GB + margin) | 1328² in **29 s** |

Zero cross-lane contention observed. Box facts: 4.5 min bare-box setup + model pulls;
huggingface.co blocked (CN) → ModelScope first, hf-mirror fallback; tuna pip mirror;
python3.12-dev required for triton fp8. q6/q8 LLM variants that split across both remainders with
`-sm row` are separate saved recipes, flagged experimental. Reservations are one measurement at
one workflow/resolution — re-measure on any recipe change (see Open questions).

## 3. Ownership boundaries

| Repo | Owns | Explicitly does NOT own |
|---|---|---|
| **ApexRouter-RS** | Rent/park/wake/favorites, SpendApproval, ledger; multi-tunnel ssh; ServiceRecord/StudioRecord lifecycle + health probes; studio recipe + docker image; `studio up/down/status` verb, API, panel, MCP; optional `images-cloud` alias relaying together's **sync** image path | ComfyUI wire protocol (POST /prompt etc.); generation jobs, prompts, seeds; async video APIs; generation UI |
| **Imaginarium-RS** | Engine registry (xai \| comfy \| together); ComfyClient + versioned workflow recipes; job store + library (system of record); `studio_*` director tools + project.json; craft_video assembly; per-generation cost estimates; SPA incl. future shot board; cerebro ingest hook | Renting, tunnels, ssh, park/wake, VRAM planning, vast money |
| **CerebroCortex-RS** | Image memory: describe_image, CLIP index, search_vision; (queued) tiny `metadata` param patch | Being in the render loop; polling anything |
| **ApexOS-RS** | Provisioning: binaries, configs, plugins.toml, skills placement, env-var contract, kiosk composition; flips the cerebro hook on | Any generation, routing, or lifecycle logic |
| **NeuralSymphony-RS** | (adjacency, separate arc) music as `AssetKind::Audio` library imports into craft_video's music slot | — no coupling now |

Scope creep across this table is the standing failure mode. Any "quick" ComfyUI status endpoint in
ApexRouter, or any rent logic in Imaginarium, cites G2 in review and dies there.

## 4. Decisions

**S1 — Spin-up is one verb: `apexrouter studio up`, resolving wake / converge / rent.**
Resolution order: (1) `$STATE/studio.json` names an instance and the fleet shows it parked →
wake path; (2) shows it running → converge (re-establish missing tunnels, re-run readiness
barrier); (3) no studio → rent path with `machine_id` pin (favorites ★/skull guard applies;
140330 is starred, degrading to profile search if the box is gone). Rationale: the operator and
the agent should not need to know which state the rig is in — that knowledge is exactly what the
daemon persists. Rent path = today's chain (`api/vast.rs:454–648`) with three edits: loop
`ensure_tunnel` per service, write ServiceRecords + StudioRecord beside the llm EndpointRecord,
register Backend + alias only for OpenAI-routed services. Wake path = today's wake job with its
dph re-check and re-park-on-boot-budget-expiry doctrine (`providers/src/vast/rent.rs:541–651`)
kept intact, plus restore-all-tunnels and the barrier. New noun files `cli/src/cmd/studio.rs` and
`server/src/api/studio.rs` (one `pub fn router()`, merged in `v1_routes()` by S-01;
`mounted_routes.rs` recovers it from source automatically — the mount-it-don't-describe-it
invariant applies to every surface in this charter).

**S2 — ComfyUI lanes are ServiceRecords, never Backends.** Three verified structural reasons: the
prober is OpenAI-shaped (`server/src/prober.rs`), `rented_backend` hardcodes `Protocol::OpenAi`
(`providers/src/vast/rent.rs:1011`), and `Health::Ready` is a routable state the resolver acts
on — a ComfyUI row in the routing table would be a lie with consequences. G2 (`docs/GARDEN.md:150`)
already mandates the boundary. The llm slot stays a plain EndpointRecord → Backend → alias; the
request path changes by zero lines.

**S3 — Protocol shapes are additive; records hold facts, never status.** New sibling variant
`RecipeKind::VastStudio { profile, machine_id, launch: StudioLaunch }` with
`ServiceSpec { name, runtime, port, devices, reserved_mb, env, health, routing, fit, local_port }`
— `RecipeKind::Vast` and `ContainerLaunch` are Stage-0 published and untouched (`ContainerLaunch`
is one-image/one-port by design, `protocol/src/catalog.rs:176–198`; a new `ServiceRuntime` enum,
never an extension of `ContainerRuntime`). New records in `protocol/src/endpoint.rs`:
`ServiceRecord` (id, instance, name, runtime, remote/local port, probe *spec*, devices,
reserved_mb) and `StudioRecord` (instance, machine_id, recipe, service ids, endpoint ids) — the
manifest that makes "the whole studio" a defined thing for park/wake/status. Persisted at
`$STATE/services.json` / `$STATE/studio.json` via the atomic store; liveness computed on read by a
new `server/src/svc_prober.rs` (GET `/system_stats` through the tunnel, never touching the routing
table). All new fields `#[serde(default)]`-tolerant; pre-studio state dirs must keep deserializing
(fixture-tested).

**S4 — Tunnels generalize to (instance_id, remote_port).** `ensure_tunnel` dedupes on that pair;
ControlPath becomes `cm-<instance>-<rport>` (`providers/src/ssh.rs:313`); wake restores **all**
persisted forwards at their recorded local ports; `tunnel_down(instance)` drops all,
`(instance, port)` drops one. `VastSpec.tunnel: Option<TunnelSpec>` survives for single-service
rentals. This edits the live money-adjacent wake path — single-forward regression tests stay green
as a gate.

**S5 — Fixed local ports 8811/8812 from a reserved 8810–8819 studio slice.** Designs disagreed
(8811/8812 vs mirroring 8188/8189). Decision: 8811 (video) / 8812 (image), because the ports
doctrine says vast tunnels live in the 8800+ pool, and because the promise must be enforced by the
**allocator** — the 8810–8819 slice is never handed to an ordinary lease and is reclaimed by owner
on daemon start, so the ports are collision-free by construction rather than by convention.
Mirroring 8188/8189 would put load-bearing ports outside the pool where nothing defends them.
Consequence: ComfyUI's own web UI is at `http://127.0.0.1:8811` and `:8812`; Imaginarium's
`[engines.comfy] video_url/image_url` point there once, forever — the same promise as proxy 8888,
across re-rents, park/wake, and even a different machine.

**S6 — Studio docker image, not boot-time pip.** `ghcr.io/buckster123/vastai-studio:cu128` (new
`docker/studio/{Dockerfile,studio.sh,stop.sh}`, built by extending the existing GHA workflow with
dated rollback tags). CUDA 12.8 base, python3.12 **with dev headers** (the triton fp8 lesson),
torch cu128, **ComfyUI + custom-node packs pinned to SHAs** (workflow-template brittleness is an
image-rebuild discipline, not a boot-time gamble), llama-server via multi-stage
`COPY --from=vastai-gguf:prebuilt` so one image runs all three services. CN doctrine applies to
weights, not the image (ghcr pulled fine in R3): `studio.sh` reuses `launch.sh`'s download logic —
ModelScope first, hf-mirror fallback, exact filenames never globs, skip-if-present-and-size-matches,
check-pid-first starts, pid files under `/run/studio/`, HOST=127.0.0.1 always, no pkill —
**idempotent by construction**, because wake re-runs `onstart` and warm start must do zero
downloads. Boot math: wake = service starts only (weights held on the parked 2 TB disk, $3–6/wk);
target < ~3 min to full posture, **measured before claimed** (house rule 7).

**S7 — fit() is not taught torch.** ComfyUI lanes enter planning as static per-device
`reserved_mb` (R3-measured + margin). A thin `studio_budget()` in core computes per-device free =
capacity − Σ reserved_mb − headroom, then calls the one existing `fit()` for each llm service
against its remainder — budgets stay per-device, never summed. svc_prober samples `/system_stats`
VRAM and alerts when observed usage exceeds reservation.

**S8 — Readiness barrier on no-progress deadlines; the barrier never destroys.** The job reports
100% only when every service in the StudioRecord answers (llm via the existing prober's Ready;
ComfyUI via `/system_stats` 200 through the tunnel). The deadline measures **no progress, not
elapsed time** (the `health_deadline_ms` lesson): progress = any state transition
(connection-refused → TCP-open → 5xx → 200) *and* process-alive via pid file — ComfyUI's torch
import can sit minutes with no HTTP change. A partial resurrection (2 of 3 lanes) fails the
barrier loudly; failure alerts, and per invariant 4 never destroys a billing box.
`BootPhase::Healthy` (container running) is explicitly not the finish line.

**S9 — Money: the box bills; attribution is presentational; `studio down` = park.** Ledger rows
stay per-instance — the metered truth. Per-service burn is split by reserved-VRAM share and
labeled `CostEstimate::Approximate` end-to-end including the UI; the honesty types forbid dressing
an allocation up as a meter. SpendApproval gates both paths of the one verb (rent as today; wake's
live-dph re-check already exists); ledger row before the billing call, verbatim. Destruction stays
solely on `DELETE /v1/vast/instances/{id}?confirm=true` — nothing in the studio surface
auto-destroys. No auto-park either: parking kills in-flight renders ApexRouter cannot see; instead
one alert ("healthy, 0 proxy requests, empty queues 30 min at $0.836/hr — park it"), fed by
`/queue` depth sampling, which is lifecycle telemetry, not wire-protocol parsing.

**S10 — Imaginarium opens one seam: `Engine` trait + `EngineRegistry` + dynamic catalog.** Extract
the async trait from `ImagineClient`'s public surface
(`image_generate/image_edit/video_generate/video_edit/video_extend/status_once/wait`); engine
selected by model-id prefix — bare = xai (bit-identical compat), `comfy/…`, `together/…`. Exactly
three call sites rewire (`server/src/lib.rs:69–80`, `mcp/src/backend.rs:62–71`, `cli/main.rs`).
The closed `ModelId` enum (`core/src/models.rs:9–18`) becomes a runtime `Catalog` (static xAI
entries + config-loaded comfy/together entries); every parse alias kept as a shim; the panic at
`models.rs:84` deleted in the same PR. `estimate.rs` grows `CostModel::LocalAmortized` so agents
see "$0 marginal (~$0.022 amortized)" vs "$0.31 on together" — the delta that makes lane choice
deliberate. JobResult/JobStore/Library/auth untouched; ComfyUI renders appear in the existing SPA
Jobs/Library tabs **for free**.

**S11 — ComfyClient: versioned workflow recipes with node-id patch maps, never string
substitution.** `POST /prompt` → poll `GET /history/{prompt_id}` (prompt_id stored in the existing
`upstream_request_id` column) → `GET /view` → library. Four recipes to start, the R3 fixed JSONs
dropping in: `wan22_t2v@1`, `wan22_i2v@1`, `qwen_image_t2i@1`, `qwen_image_edit@1`, each with a
sidecar manifest mapping param → `nodes[id].inputs.key` plus `require_nodes` checked against
`GET /object_info` at attach — fail loud with the node diff. `@N` versions: a node-pack bump mints
`@2`; old takes keep naming what made them. Timeout leaves jobs **non-terminal** (client.rs poll
semantics reused verbatim); CUDA-OOM in failed history → `POST /free {unload_models:true}` + one
retry. i2v inputs resolve `library:` refs → `POST /upload/image`. Security: agents never pass a
base_url — only configured lanes; `MediaRef::from_remote_input` path rejection stands; ComfyUI has
no auth, the ssh tunnel is the entire boundary.

**S12 — Seed law (non-negotiable).** The tool layer resolves the seed client-side when unset,
writes it into the graph before POST, and records it in the take and meta.json. ComfyUI's
server-side `-1` randomization never runs — enforced by a test that greps outgoing graphs for `-1`
seeds. Without this, retakes and continuity are silently unreproducible.

**S13 — Director tools: three tiers, one MCP server, two-document state law.** `studio_*` lands in
`imaginarium-mcp` beside the 11 existing tools (flat Vec + one dispatch arm each; a second server
doubles plumbing for nothing; `studio_` is collision-safe in `.mcp.json`). **T1** ships with ZERO
new tools: `imaginarium_video_generate {model:"comfy/wan22-t2v"}` just works once the catalog has
comfy entries, plus optional `seed/negative/steps/cfg/frames/fps` on the two generate tools
(ignored-with-warning elsewhere). **T2**: `studio_shot` (camera-as-prompt, loras, `continue_from`
last-frame chaining), `studio_frame_extract` (ffmpeg → chainable library still),
`studio_reference_set` (incl. `from_search` over your own prior takes), `studio_retake` (reuses
recorded seed; takes append-only), `studio_approve` (the cerebro hook point),
`studio_scene_render`, `studio_project_status`. **T3**: `studio_storyboard`, `studio_produce`
(topo-sorted shot graph; independent shots parallel across the two lanes — ComfyUI queues are FIFO
per instance, so parallelism = two instances, exactly what R3 proved), `studio_assemble` (drives
existing craft_video; Sonus/NeuralSymphony bed via the music slot). State law mirrors ApexRouter
invariant 3: job store + library = append-only render **facts**; `projects/<slug>/project.json` =
creative **intent** (scenes, shots, seeds, `template_version` per take, approvals); status is
always computed by joining the two, never stored. imaginarium-mcp gains a
tools-inventory-vs-dispatch test — the thrice-shipped unreachable-module bug has an MCP-shaped
twin.

**S14 — together.ai is a third Imaginarium engine, never an ApexRouter video route.** Image = sync
`POST /v1/images/generations` (near payload-rename of the xAI path); video = async `/v2/videos`
create-then-poll fitting the request_id skeleton; `outputs.cost` persisted verbatim as **Metered**;
status enum deserialized as the superset. Sequencing: **spike fal.ai first** per
`notes/queued-open-model-upstreams.md` — fal hosts grok-imagine plus the open fleet and could
collapse two adapters into one; time-boxed half day, the seam makes either ~1 day. ApexRouter's
only involvement: an optional `images-cloud` alias relaying the sync image path through the
existing together Provider backend (near-zero code, honestly `CostEstimate::Unknown` without a
price table). Video is /v2, async, job-shaped — it never rides the byte relay. One live
FLUX.1-schnell call (~$0.003) verifies param semantics, **outside** the test suite (hermeticity).

**S15 — Cerebro hook: approval-time only, config-gated, degrades to skip-with-log.** Fires on
`studio_approve` (`[cerebro] mode = off|approved|all`, default **off**; the ApexOS installer flips
it to `approved` because the full package guarantees cerebro-mcp). Video → ffmpeg poster frame →
`describe_image {remember:true, prompt seeded with the generation prompt, tags: vision/gen/model/
seed/job/project/scene/shot/rig/cost}` with `image_path` pointed at the .mp4 so `search_vision`
returns the clip; `associate(DerivedFrom)` to the prompt memory; sessions wrapped in episodes.
Hard-won rules: salt caption content with job_id (exact-content dedup silently returns the OLD
node); run only on the machine holding the library (paths must stay resolvable — never vast-box
paths); no VLM reachable → skip and log, never block approval. Structured provenance rides tags
until the queued upstream `metadata`-param patch lands (column exists; input plumbing missing).

**S16 — UI story: zero-new-code baseline, two thin additions, no new app.** Today, free: ComfyUI's
own web UI through the tunnels (8811/8812 — the power-user studio UI), ApexRouter's web UI at
:2739 (fleet/tunnels/jobs/money), Imaginarium's SPA at :8791 (comfy renders appear in
Jobs/Library once S10 lands), cerebro dashboard ambient. New: (a) one `studio` entry in
ApexRouter's `PANELS` — lane cards (device, reserved MB, tunnel port, probe state, attributed burn
labeled approximate), one Studio Up button with the 409-preview confirm dialog,
`Event::ServiceChanged` on WS, jobCard progress, unknown-route banner for old daemons; (b) later, a
read-only shot board tab in Imaginarium's SPA over `studio_project_status` — deferred until T2
exists. Generation UX (thumbnails, queues, takes) is deliberately absent from ApexRouter's panel,
per G2.

**S17 — Operator skill: `~/.claude/skills/studio/SKILL.md`, source-controlled in Imaginarium-RS.**
Cutting-room voice; written against real T1 artifacts, not speculation. Core doctrine it must
teach: money discipline ($0.836/hr, park when idle, estimate before any cloud model); the director
loop (storyboard → reference stills → shot → **extract frames and Read them with your own eyes —
you cannot cut what you have not seen** → approve → chain → assemble); continuity physics
(resolved-seed discipline, i2v chains drift after ~3 hops → re-anchor on a Qwen-edit reference
keyframe, cut on action to hide seams, ~81-frame/~3.4 s per-generation ceiling, camera control is
prompt grammar, no lip-sync — set trailer-grade expectations or produce runs will burn rig-hours
retrying the impossible); failure modes (OOM → `/free` + retry, ModelScope not huggingface on CN
boxes, stuck queue → `/interrupt`, attach-diff on template mismatch). ApexRouter's skill gains the
park-don't-destroy economics and the one-verb loop.

**S18 — Autonomy hardening gates producer budgets.** Before any agent gets `studio_produce` with
cloud models in reach: fix the serial MCP loop (one blocking video call stalls every tool),
proxy-mode `job_status` upstream polling, library download tools for proxy callers, per-token
spend caps (BACKLOG C6). Local comfy is $0-marginal, so T1–T2 are safe early; grok-imagine 1.5 at
$4.20/min is not.

## 5. Roadmap

| # | Phase | Repo | Deliverable | Effort | Depends on |
|---|---|---|---|---|---|
| 1 | Engine seam + catalog | Imaginarium-RS | `Engine` trait, `EngineRegistry`, dynamic `Catalog`, panic deleted, 3 call sites rewired, compat shims — one PR, suite green | S, ~2 d | — |
| 2 | Local-comfy T1 | Imaginarium-RS | ComfyClient, 4 `@1` recipes + manifests, `/object_info` validation, seed law + `-1` grep test, `[engines.comfy]` config, generate-tool params. **GATE: a real agent session produces a real Wan clip over a manual `ssh -L` tunnel** — the fun-per-effort peak; everything after compounds | M, ~2 d | 1 |
| 3 | Protocol shapes | ApexRouter-RS | `RecipeKind::VastStudio`, `ServiceSpec`, `ServiceRecord`/`StudioRecord`, `ImageType::Studio`, `Event::ServiceChanged`, config slots — all additive, serde-tolerant, fixture-tested against a pre-studio state dir | S, ~1–2 d | — (parallel with 1–2) |
| 4 | Multi-tunnel | ApexRouter-RS | (instance, remote_port) keying, `cm-<i>-<rport>`, 8810–8819 reserved slice + reclaim-by-owner, wake restores all forwards; single-tunnel regressions green | M, ~2 d | 3 |
| 5 | Studio image | ApexRouter-RS | `docker/studio/`, `vastai-studio:cu128` + dated tags via GHA, idempotent `studio.sh`/`stop.sh` | M, ~1–2 d | 3 |
| 6 | Store + planner + prober | ApexRouter-RS | services/studio.json atomic stores, `svc_prober.rs`, `studio_budget()` over existing `fit()`, seeded `studio-96gb` recipe | S, ~2 d | 3 |
| 7 | The verb | ApexRouter-RS | `studio up/down/status`: API module merged by S-01, CLI noun, resolution order, extended rent job, wake-all-tunnels, readiness barrier, park-only down; money gates verbatim | L, ~3–4 d | 4, 5, 6 |
| 8 | **Acceptance on ★140330** | ApexRouter-RS + vast | Timed cold `studio up`, 3 pids, `/system_stats` VRAM vs reservations, tokens through `studio-llm`, one Wan + one Qwen render via 8811/8812, park, timed wake with zero downloads and same ports; **credit reconciles to the ledger exactly** | ~$1–2, 0.5 d | 2, 7 |
| 9 | T2 director tools | Imaginarium-RS | `studio_frame_extract` + chaining first (unlocks manual multi-shot immediately), then project.json + shot/retake/reference/approve/scene/status; inventory-vs-dispatch test | M, ~2–3 d | 2 |
| 10 | Operator skill | Imaginarium-RS → `~/.claude/skills/studio/` | SKILL.md per S17, written against #2/#9 artifacts | S, ~0.5 d | 2, 9 |
| 11 | MCP surfaces | ApexRouter-RS | `apexrouter_studio_up/_status/_down` (approval-gated) + missing `_vast_park/_wake/_tunnel`; skill economics section | S, ~1 d | 7 |
| 12 | UI | ApexRouter-RS / Imaginarium-RS | Studio PANELS entry (M, ~1–2 d); read-only shot board tab (M, ~1–2 d, deferrable) | M | 7 / 9 |
| 13 | Cerebro hook | Imaginarium-RS (+ tiny CerebroCortex-RS patch) | `cerebro_hook.rs` on approve, poster frames, salted captions, episode wrap; optional upstream `metadata` param | S, ~1 d | 9 |
| 14 | Cloud engine | Imaginarium-RS | fal-vs-together spike (0.5 d, one ~$0.003 live call) then the chosen provider with Metered costs; optional ApexRouter `images-cloud` alias (hours) | S–M, ~1.5 d | 1 |
| 15 | Autonomy hardening | Imaginarium-RS | Serial-loop fix, proxy polling, download tools, spend caps (C6) — **gates 16 and any cloud budget** | M, ~2 d | 9 |
| 16 | T3 producer | Imaginarium-RS | storyboard/produce (two-lane scheduler)/assemble | M, ~1–2 d | 9, 15 |
| 17 | ApexOS packaging | ApexOS-RS | Binaries, configs, plugins.toml, skills, env-var rows, cerebro flag on, kiosk note; NeuralSymphony stays a note | M, ~1–2 d | 8, 10, 13 |

Phases 1–2 and 3–7 are independent tracks; T1 needs nothing from ApexRouter beyond one manual
`ssh -L` line, documented in the skill until #8 lands.

## 6. Open questions

1. **Does vast re-run `onstart` cleanly for three pids on wake?** Proven for one service only.
   Phase 8's timed park/wake is the gate before any warm-start claim; partial resurrection must
   fail the barrier loudly.
2. **Does ModelScope host every needed file** — specifically the ComfyUI text encoders — or does a
   fresh boot on a new machine stall at weights pull? Unverified; hf-mirror is the fallback, not a
   guarantee.
3. **ghcr.io pull from CN** worked once (R3) and is not a contract. If it degrades, the image
   becomes the cold-start long pole and the pip path is retired. Contingency: ModelScope-hosted
   image mirror — unbuilt, undecided.
4. **fal.ai vs together.ai** (S14): unresolved until the spike. Related unknowns: `size` vs
   width/height semantics, whether legacy `api.together.xyz` serves `/v2/videos`.
5. **Reservation drift**: 23 000/32 000 MB reflect one workflow at one resolution. A bigger Wan
   job or node-pack bump can blow past the reservation and OOM-starve the co-resident llm slot on
   GPU0. Mitigation is margin + svc_prober alerting; the real answer (per-recipe re-measurement
   discipline) is process, not code.
6. **Cross-hardware reproducibility**: takes record engine + `template_version`, but a re-rent
   onto different silicon is not bit-reproducible. Accepted; is per-take node-pack version
   recording enough provenance?
7. **Split-GPU llm recipes** (q6/q8 with `-sm row` across both remainders): flagged experimental;
   no measured numbers yet.
8. **CLIP scale**: brute-force cosine over all cerebro vision rows will degrade at studio volumes;
   at what library size does this need an index?
9. **140330 as single point of rehire**: the recipe degrades to profile search, but the CN weights
   doctrine and cached image are per-machine advantages that vanish with the box. Do we pre-warm a
   second favorite?
10. **MiniMax-H3 modes in studio recipes**: one ServiceSpec per mode (FL2VA vs Ref2VA) or one
    service with recipe-selected weights? Prefer **one video service, recipe-selected weights** so
    ports stay 8811 forever (S5).
11. **Qwen3.8 flip date**: when do we re-seed `studio-96gb` (and chat-heavy) to 3.8-27B — same day
    as unsloth GGUFs, or after a full R1-style cell?

## 7. Amendments log

- **2026-08-07** — **Phase 6 shipped.** `core::studio::studio_budget()` (per-device free =
  capacity − Σ reserved − headroom → `VramBudget` for existing `fit()`); `server::svc_prober`
  probes ServiceRecords via local tunnels (`/system_stats` / `/v1/models`), caches
  `ServiceStatus`, alerts on VRAM over reservation; seed profile+recipe `studio-96gb` (★140330
  pin, EU-first geo, ImageType::Studio) via `ensure_studio_seeds` at daemon start. Phase 7
  (the verb) still open.
- **2026-08-07** — **Phase 5 image scaffold shipped.** `docker/studio/{Dockerfile,studio.sh,
  stop.sh,install-custom-nodes.sh,README.md}` + GHA `vastai-studio image` workflow. Image
  ref `ghcr.io/buckster123/vastai-studio:cu128` (dated rollback tags). llama-server via
  `COPY --from=vastai-gguf:prebuilt`; ComfyUI pin `e803f24` (R3); python3.12-dev + torch
  cu128; idempotent onstart with `/run/studio/*.pid`; exact-file weight pulls. **Not yet
  built/pushed to ghcr** — dispatch the workflow when ready.
- **2026-08-07** — **Phase 3–4 foundation shipped** (protocol + multi-tunnel + store). Additive
  only: `RecipeKind::VastStudio`, `ServiceSpec`/`ServiceRecord`/`StudioRecord`,
  `ImageType::Studio`, `Event::ServiceChanged|Removed|StudioChanged`,
  `DEFAULT_STUDIO_PORT_RANGE` (8810–8819) with fixed 8811/8812, tunnels keyed on
  `(instance_id, remote_port)` with `cm-<i>-<rport>`, ordinary port allocator skips the studio
  slice, wake restores **all** forwards, `$STATE/services.json` + `studio.json` atomic stores.
  No Comfy wire protocol. No rent/destroy path change.
- **2026-08-07** — **S19–S22** (roster refresh after MiniMax-H3 open weights + Qwen3.8 announcement;
  operator default demand). Complements GARDEN G7–G11. Does not change S1–S18; does not implement
  code. **FLUX.3** remains API/preview for self-host — tracked under G9 only until open weights.

### S19 — Default operator studio demand (the “most often rent”)

When no recipe is named, **daily** rung (GARDEN §6.1 ladder) is the implied demand:

1. **LLM:** 2–4 concurrent slots of **27B-class Qwen at 256k** (today: 3.6; succession: 3.8 per G11),
   OpenAI path only, alias stack unchanged for clients.
2. **Video:** high-quant lane ready (R3: Wan TI2V-5B; **candidate:** MiniMax-H3 mid/high quant).
3. **Image:** ready lane (R3: Qwen-Image 20B fp8; expand candidates under G9).

All three **warm after launch-build-serve** (S6 idempotent image + S8 barrier). Box class is
whatever fits the rung — consumer multi-GPU or datacenter — not a fixed SKU. **★140330** remains
the preferred hire when available (S1); otherwise ranked search.

### S20 — Quality × price is a recipe dimension, not a second product

Draft / daily / show rungs (GARDEN §6.1) map to **different `VastStudio` recipes** (or quant
fields on one recipe family), not to different verbs. `studio up` still resolves wake → converge →
rent (S1). SpendApproval and ledger stay per-instance (S9). Cheap Q2 H3 on a small box and fp8/H100
H3 on a fat box are the **same product surface**, different recipe + profile.

### S21 — Host selection defaults for studio rent

Search / auto-rent defaults for studio recipes (profile fields, not hard-coded in the verb):

1. **vast verified** preferred (lemon rate from R1 still applies).
2. **Geo preference order: EU → Asia → USA** — express as ordered profile preference when the
   offer query can rank; until then default profile geo = EU, with Asia (★140330 CN doctrine) and
   USA as explicit profiles / favorites.
3. ★ / ☠ favorites still override anonymous ranking (existing money path).
4. Datacenter vs consumer is **whatever matches VRAM + cuda band** for the recipe; no ideology.

### S22 — Creative roster candidates (Comfy lanes only)

| Lane | Measured default (keep until re-measured) | Candidates (enter only after a cell) |
|---|---|---|
| Video | Wan 2.2 TI2V-5B (R3) | **MiniMax-H3** (FL2VA / Ref2VA, quant ladder Q2→high / pruned-fp8); heavier Wan/LTX |
| Image | Qwen-Image 20B fp8 (R3) | FLUX.2-klein family; **FLUX.3 local only when open weights + Comfy path exist**; other 2026 open image GGUF/fp8 as they pin |

MiniMax-H3 does **not** become an OpenAI Backend (S2). Imaginarium catalogs `comfy/minimax-h3-…`
recipes; ApexRouter only reserves VRAM + ports + process lifecycle.