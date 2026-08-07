# GARDEN.md — the mk2 exploration charter: a model garden on one honest card

*Drafted 2026-08-01 from a two-stage brainstorm. §2 was verified that day against the live
HuggingFace index and the live vast.ai market, with read-only calls only. §4 is arithmetic,
clearly bounded. Nothing in this document is a measured benchmark until §7 produces one —
house rule 7 applies to gardens too.*

This charter is exploratory: it binds direction, not implementation. The seeded decisions in
§6 are agreed and dated; everything else is a hypothesis the campaign (§7) exists to test.
Where this document and `CHARTER.md` D1–D18 disagree, D1–D18 win.

---

## 1. The vision

A fully-local ApexOS-RS colony — one to four nodes — fed by **one fat GPU node running a
model garden**: thinker, embedder, dreamer, image and video models co-habiting a single
VRAM budget under one honest ledger. ApexRouter is the gardener: it plants (recipes),
waters (supervision, health), prunes (idle-unload), and accounts for every megabyte the
way it already accounts for every dollar.

The router's promise does not move: **point every agent at `http://127.0.0.1:8888/v1` and
never change it again.** The garden changes what stands behind the aliases, never the door.

**The design centre is Tier B: 48 GB.** That is the rig a person can build without selling
a house — two used RTX 3090s — and, not coincidentally, the exact layout rentable on
vast.ai today from $0.22/hr for development. Vast instances posing as local (rented box,
SSH tunnel, backend registered at `127.0.0.1:88xx`) are the sanctioned development cheat;
nothing in the design may *depend* on the box being remote.

| Tier | VRAM | Hardware | Role |
|---|---|---|---|
| A | 24–32 GB | 1× 3090/4090/5090 | Must degrade gracefully: thinker + embedder, creative swap-in only |
| **B** | **48 GB** | **2× 3090 · 2× 4090 · 1× RTX 6000 Ada** | **Design centre: full garden** |
| C | 96 GB+ | 2× 6000 Ada, 3×+ 5090 | Aspirational: the 122B anchor |

## 2. Ground truth — verified 2026-08-01

### 2.1 Text/vision residents (GGUF, sizes from authoritative per-file listings)

| Model | Kind | Quant → size | Notes |
|---|---|---|---|
| `unsloth/Qwen3.6-27B-GGUF` | dense 27B, **native vision** (mmproj in-repo) | Q4_K_M 16.8 · **Q6_K 22.5** · UD-Q6_K_XL 25.6 GB | the thinker |
| `unsloth/Qwen3.6-27B-MTP-GGUF` | same, MTP heads baked in | full quant ladder, IQ2 9.6 → Q6+ | the 50-tps lever |
| `unsloth/Qwen3.6-35B-A3B-GGUF` | MoE, **3B active**, vision | UD-Q4_K_M 22.1 · UD-Q6_K 29.3 · UD-Q6_K_XL 31.8 GB | throughput-first alternate |
| `unsloth/Qwen3.5-122B-A10B-GGUF` | MoE, **10B active**, vision | UD-IQ4_XS 60.2 · Q4_K_M 76.5 · Q6_K 101 GB | Tier C anchor (+MTP variant exists) |
| `Qwen/Qwen3-Embedding-8B-GGUF` | embedder | Q6_K 6.2 · Q8_0 8.0 GB | cerebro goes harder |
| `ggml-org/embeddinggemma-300M-GGUF` | embedder | ~0.3 GB | Pi-node local fallback |

MTP (multi-token prediction) is **merged in llama.cpp** (PR #22673, May 2026):
`--spec-type mtp --spec-draft-n-max N`. Claimed 1.4–2.2× generation speedup at unchanged
output quality. Both claims are §7's to confirm; `core::argv` must learn the flags through
the existing `FlagSupport` probe, never hardcoded.

### 2.2 Creative residents (GGUF, ComfyUI-served)

| Model | Quant → size | Notes |
|---|---|---|
| `unsloth/FLUX.2-klein-9B-GGUF` | Q4_K_M 5.9 · **Q6_K 7.9** · Q8_0 10.0 GB | the co-habitation candidate |
| `unsloth/Qwen-Image-2512-GGUF` | Q4_K_M 13.2 · Q6_K 16.8 GB | best-in-class in-image text; Edit-2511 sibling |
| `QuantStack/Wan2.2-T2V-A14B-GGUF` | Q4_K_M 9.7 GB ×2 (high/low-noise experts) | video, I2V/T2V/Animate variants |
| `unsloth/LTX-2.3-GGUF` (22B distilled) | Q4_K_M 14.2 GB | video; 16 GB min, 24 GB comfortable; two-stage pipeline + Gemma text encoder; wants generous system RAM |

The GGUF-local lane trails hosted-latest by a version (Wan 2.2 local vs Wan 2.6/2.7 on
fal.ai). That gap is what the serverless upstream is for, not a reason to chase it locally.

### 2.3 The market that day (read-only `vast offers` search)

| Rig | VRAM | From | Note |
|---|---|---|---|
| 2× RTX 3090 | 48 GB | **$0.22/hr** | the people's 48 GB |
| 1× RTX 6000 Ada | 48 GB | $0.25/hr | single-card 48, no split, rel 0.999 |
| 2× RTX 4090 | 48 GB | $0.27/hr | bandwidth mid-point |
| 1× RTX 5090 | 32 GB | $0.21/hr | Tier A probe |
| 1× RTX 3090 | 24 GB | $0.09/hr | Tier A floor |

No H100/H200/80 GB-A100 in the live vocabulary that day; 48 GB was the biggest single
card on the market. Account credit at survey time: $7.73.

### 2.4 fal.ai (the serverless creative upstream)

Queue API: submit → poll or webhook, bearer auth, REST. Prices the open fleet per-output
(FLUX.2 dev $0.012/MP; Wan 2.6 video $0.10/s at 720p) or per-GPU-second. Hosts FLUX, Wan,
Kling, Veo, Seedream, hundreds more — **including `xai/grok-imagine`**, Imaginarium's
current sole upstream. One fal queue adapter therefore potentially unifies the closed
model Imaginarium already speaks and the entire open fleet.

## 3. The residents — Tier B reference garden (48 GB)

| Resident | Model | VRAM | State |
|---|---|---|---|
| Thinker | Qwen3.6-27B Q6_K (MTP build) | 22.5 GB + mmproj | always warm |
| Embedder | Qwen3-Embedding-8B Q6_K | 6.2 GB | always warm |
| Dreamer | **= the thinker alias** | 0 GB | dream jobs scheduled into idle windows (§6 G3) |
| KV pool | per-model arithmetic | ~10–12 GB | see below |
| Image | FLUX.2-klein-9B Q6_K | 7.9 GB | co-resident in the *studio* posture |
| Video | Wan 2.2 / LTX-2.3 | 10–15 GB | swap-in only on 48 GB, always |

**KV is arithmetic, not a constant.** Bytes/token = 2 × n_layer × n_kv_head × head_dim ×
bytes(cache-type), read from the GGUF header by the discover code; `fit()` prints the real
number per model and that number, not this table, is authoritative. Gemini's "2×256k =
32 GB" was a guess about a different model; 256k slots are a Tier C luxury.

**Postures.** A garden recipe names one of two postures rather than pretending both fit:
*chat-heavy* (creative swapped out, KV pool maximal — roughly 2 slots at long context or 4
at moderate) and *studio* (klein co-resident, leaner KV). Switching postures is a supervised
stop/start with the cost visible, never a silent eviction.

## 4. Throughput doctrine — bounds, not benchmarks

Target: **≥50 tok/s per slot, 1–4 concurrent slots.** Single-stream generation is
bandwidth-bound: ceiling ≈ effective-bandwidth ÷ bytes-read-per-token.

| Thinker | Bytes/token | 2×3090 (~0.9 TB/s eff. per card) | 4090-class | Consequence |
|---|---|---|---|---|
| 27B dense Q6 | ~22.5 GB | ~25–35 tok/s | ~40–50 | **50 tps needs MTP to hold its claim** |
| 35B-A3B Q6 | ~5–6 GB | high double digits+ | higher | throughput headroom, less depth |
| 122B-A10B Q4 | ~7 GB | (Tier C) | — | smarter *and* faster than dense 27B, when it fits |

Every number above is a ceiling estimate to be replaced by §7 measurements — `llama-bench`
plus the server `timings` object plus ApexRouter's own `tok_per_s_p50` through the proxy,
the same three-way corroboration MK1-CORE ACCEPTANCE used. Concurrent-slot behaviour
(continuous batching) is measured, not extrapolated.

## 5. The arcs

- **G-A · Garden accounting** (ApexRouter). Garden recipes: a *set* of residents plus a
  posture, planned by one `fit()` call against one VRAM ledger. `argv`/`FlagSupport` learns
  the MTP flags. Idle-unload lands via the charter's existing mk2 seed (llama.cpp router
  mode `--models-dir` / `--sleep-idle-seconds`) or supervisor stop/start — whichever the
  campaign shows is honest about cold-start cost. ComfyUI becomes a supervised endpoint
  *class* (lifecycle, port, health, VRAM reservation) — never a protocol ApexRouter parses.
- **G-B · Creative backends** (boundary decided, §6 G2). ApexRouter owns lifecycle,
  placement, tunnels and money. **Imaginarium owns protocol**: a ComfyUI graph adapter for
  the local/vast lane and a fal.ai queue adapter for the serverless lane, sitting beside
  the existing grok-imagine upstream — which fal also hosts, making a single-adapter
  unification worth designing for.
- **G-C · Colony wiring** (ApexOS-RS / CerebroCortex). Thinker and embedder are aliases on
  8888. The dreamer is *configured* onto the thinker alias — the welfare doctrine honoured
  by routing, not by co-hosting a second copy. Dream runs scheduled into idle windows.
  Pi nodes carry embeddinggemma-300M for local-fallback embedding.
- **G-D · The campaign** (§7). Data before design lock: G-A's shapes are drafted only
  after the campaign's numbers exist.

## 6. Seeded decisions — agreed 2026-08-01

| # | Decision |
|---|---|
| G1 | **Tier B (48 GB) is the design centre.** Tier A must degrade gracefully; Tier C is aspirational. |
| G2 | **ApexRouter = lifecycle, Imaginarium = protocol.** ApexRouter relays OpenAI-shaped bytes and supervises processes; it never learns ComfyUI's or fal's wire shapes. |
| G3 | **Dreamer = thinker** (ApexOS-RS model-welfare doctrine), implemented as an alias, scheduled into idle. |
| G4 | **unsloth quants preferred; ≥Q4 floor for the 27B thinker, Q6 preferred.** |
| G5 | **No invented benchmarks.** A number without a measurement is written as a bound and labelled. |
| G6 | **Campaign budget: the full remaining credit (~$7.73) is granted.** Every rent is operator-triggered with a `SpendApproval`; the ledger row precedes the billing call; everything else stays read-only; nothing that costs money is auto-destroyed. |
| **G7** | **Quality ladders, not one fixed quant.** Every garden resident is a *family* (base model + quant/mode matrix). Cheap work rides Q2–Q4 on small verified boxes; showpiece work rides high quant / fp8 on fat silicon. The recipe names the ladder rung; the offer search sizes the box. *Agreed 2026-08-07.* |
| **G8** | **Default studio demand (operator preference, 2026-08-07):** 2–4 concurrent **27B / 256k** Qwen slots (thinker alias stack) + a **high-quant video** lane + an **image** lane, all warm after launch-build-serve. Prefer **vast verified** hosts; geo preference **EU → Asia → USA** (not a hard filter until profile `geo` can express a ranked list — until then: profile default EU, fall through manually or via favorites ★). |
| **G9** | **Roster is live, not frozen at charter date.** New open weights enter as *candidates* with sources + size bounds; they become residents only after a measured cell (G5). H3 and Qwen3.8 enter as candidates 2026-08-07 (§2.5). FLUX.3 open weights are **tracked, not scheduled**, until self-hostable files exist. |
| **G10** | **MiniMax-H3 is a creative/omni resident (Comfy), not the text thinker.** Modes (at least FL2VA / Ref2VA) and the full community quant ladder (Q2…Q5 / pruned-fp8 GGUF) are first-class recipe dimensions. Cheap rung ≈ Q2–Q3 on mid cards; quality rung may want multi-GPU / H100-class / fp8-class VRAM — that is a *higher ladder recipe*, not a different product. |
| **G11** | **Thinker line of succession:** Qwen3.6-27B (current measured) → **Qwen3.8-27B** when open weights + unsloth (or equivalent) GGUFs exist and a cell re-measures ≥ the 3.6 baseline on the same protocol. Do not rename the alias (`auto` / studio-llm); only the recipe behind it moves. |

### 6.1 Amendments log (garden)

- **2026-08-07** — G7–G11 added (quality ladders, default studio demand + geo preference, live roster rule, MiniMax-H3 creative family, Qwen3.8 succession). §2.5 candidate roster. Does not change G1–G6; does not invent benchmarks for unreleased weights.

## 2.5 Candidate roster refresh — 2026-08-07 (not yet measured)

*House rule 7: sizes and speeds below are **labels or third-party claims** until a GARDEN-RUNS cell lands. Prefer unsloth / Comfy-Org / official MiniMaxAI repos when they exist.*

### Thinker (text / vision LLM)

| Candidate | Status 2026-08-07 | Notes for recipes |
|---|---|---|
| **Qwen3.6-27B** (+ MTP variants) | **Current resident** (R1–R4 measured) | Keep as default until 3.8 cell closes |
| **Qwen3.8-27B** | **Announced open-weight**, expected ~week of 2026-08-10 (Alibaba) | Day-zero: wait for unsloth GGUF ladder; re-run R1-style cell before flipping default recipes |
| **Qwen3.8-Max** | Open-weight flagship (same wave) | Tier C / multi-GPU only; not the default studio thinker |
| MiniMax **M3** (text MoE) | Separate product from H3; huge FP footprint | Optional high-end thinker experiment, **not** the default garden; do not confuse with H3 |

### Video / omni creative (ComfyUI)

| Candidate | Status 2026-08-07 | Notes |
|---|---|---|
| **MiniMax-H3** | Open weights ~2026-08-03 (`MiniMaxAI/MiniMax-H3`); Comfy-Org repack; community GGUF **Q2–Q5** + pruned-fp8 GGUF (≈11–24 GB class per mode file, third-party listings) | **Modes:** FL2VA (first/last-frame) · Ref2VA (omni-reference). License: community agreement — read before commercial use |
| Wan 2.2 TI2V-5B | **R3 resident** (measured on ★140330) | Keep as measured baseline video lane |
| LTX-2.3 / Wan heavier | Prior §2.2 | Swap-in / higher ladder only |

### Image (ComfyUI)

| Candidate | Status 2026-08-07 | Notes |
|---|---|---|
| Qwen-Image 20B fp8 | **R3 resident** | Keep measured default until a better measured cell |
| FLUX.2-klein family | §2.2 | Co-habit candidate on 48 GB |
| **FLUX.3** (BFL) | **API / early access**; **FLUX 3 Dev open-weight backbone announced, not generally downloadable for self-host yet** | **Do not** write a local recipe until weights + Comfy path exist. Hosted path stays Imaginarium (S14 spike: fal may already surface BFL) |
| Additional 2026 image open weights | Field moved fast | Add as candidates when HF/Comfy-Org has a pinable SHA + size; measure before default |

### Quality × price ladder (how recipes should be named)

Recipes are **rungs**, not one mega-config:

| Rung | Intent | Typical thinker | Typical H3 / video | Typical box class (illustrative) |
|---|---|---|---|---|
| **draft** | agent loops, layout, throwaway | 27B Q3–Q4, short/mid ctx | H3 Q2–Q3, short clips | 1× 24–32 GB verified, cheap |
| **daily** | default operator studio | 27B Q5–Q6, **256k**, 2–4 slots | high-quant Wan/H3 mid | 48–96 GB (2×4090 class / ★140330) |
| **show** | client delivery, print, long video | 27B Q6–Q8 or 3.8 when ready; or M3 experiment | H3 high quant / fp8 path; fat Wan | multi-GPU / H100-class as needed |

Search profiles for each rung: `verified` preferred; **geo preference EU → Asia → USA** (G8); ★/☠ favorites still win over anonymous market when the machine exists.

## 7. The campaign — Tier B measurement protocol

Each run: rent (operator-triggered) → recipe-driven setup → measure → **verified destroy**
→ ledger row closed. Pick hosts with DOWN ≥ 800 Mbps (a 25–40 GB download at 200 Mbps is
half an hour of billed silence). Findings land in `docs/GARDEN-RUNS.md` with raw numbers.

| Run | Rig (~$/hr) | Measures |
|---|---|---|
| R1 | 2× 3090 (~$0.25) | 27B Q6: plain vs **MTP** (`llama-bench` + `timings`), 1/2/4 concurrent slots via `compare`/`smoke`, KV posture *chat-heavy* |
| R2 | same box | *studio* posture: + FLUX.2-klein-9B Q6 under ComfyUI-GGUF; thinker degradation while generating; VRAM ledger truth vs `nvidia-smi` |
| R3 | 2× 4090 (~$0.30) | R1 core repeated — bandwidth scaling point |
| R4 | 1× 6000 Ada (~$0.25) | R1 core repeated — single-card 48 GB, no split overhead |
| R5 | stretch | 35B-A3B garden, or Tier A degradation on 1× 5090 ($0.21) |

Estimated spend: ~2 h × 4 boxes ≈ **$2–3 planned**, hard ceiling the credit itself.
Success criteria: (a) a posture on a $0.25/hr 48 GB rig that holds ≥50 tok/s/slot × 2
slots with MTP, (b) a measured studio posture where image generation costs the thinker a
*known* number of tok/s, (c) a VRAM ledger that predicted both within ±1 GB.

## 8. Risks and open questions

- **ComfyUI is a Python process on the fat node.** The colony stays Rust; the garden may
  *supervise* a Python process, contained in a venv/container the recipe names. Honesty
  over purity — this is lifecycle supervision, not linking.
- **LTX-2.3 wants generous system RAM** alongside VRAM; Tier B boxes must be sized for it.
- **MTP quality claim** ("no change in accuracy") is a vendor claim until §7 compares
  outputs on our own eval prompts.
- **Vast 2×3090 boxes vary** (NVLink, PCIe lanes, thermals). The campaign records the
  host's topology with every number; a result without its topology is not a result.
- **Classifier friction on spend calls** is expected; every rent is manually approved by
  the operator — treated as a feature of the money invariant, not a bug to engineer away.
