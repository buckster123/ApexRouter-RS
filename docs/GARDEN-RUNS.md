# GARDEN-RUNS.md — the campaign ledger

*Raw numbers from `docs/GARDEN.md` §7. A result without its host topology is not a result.
Money per run is recorded to the cent; the ApexRouter ledger row ids are the audit trail.*

---

## R1 — 2× RTX 3090, chat-heavy posture, MTP question — 2026-08-01

**Box:** vast instance `46448151`, offer `26660616`, Finland FI. 2× RTX 3090 (48 GB),
offer $0.2422/hr + 150 GB disk = **$0.2817/hr actual**. DOWN 862 Mbps, rel 0.997.
**Builder lane forced** (`[known_forks."garden-mtp"]` matches `unsloth/*MTP-GGUF` →
`ggml-org/llama.cpp@master` built at boot): the prebuilt `vastai-gguf:prebuilt` image's
llama-server predates the May-2026 MTP merge, and both cells must run the same build.
Build number as reported by the box: *(recorded at boot)*.
Topology: *(recorded below once measured — `nvidia-smi topo -m`, PCIe width, NVLink y/n)*

**Attempt 1 — a lemon, $0.02.** Offer `45761361` (Quebec, $0.1374/hr, the market's
cheapest 2×3090) stalled in `pulling`; the vast dashboard's `status_msg` had the truth:
`Error response from daemon: error creating temporary lease: write /var/lib/docker/
containerd/.../meta.db: input/output error` — host-side disk failure in the provider's
docker layer. Destroyed at ~7 min, verified gone. The offer relisted immediately at the
same price — the cheapest box on the market can be cheapest for a reason; `45761361` is
burned for this campaign.

**Attempt 2 — died twice at launch, $0.14, both kills named.** Instance `46448151`
(Finland, offer `26660616`, rel 0.997): container healthy, llama.cpp built from
`ggml-org/llama.cpp@master` in ~6 min (sm_86 + NCCL — the builder image is dual-3090-
correct), model downloaded — then `/var/log/launch.log` recorded:

1. `ggml_cuda_init: failed to initialize CUDA: forward compatibility was attempted on
   non supported HW`. The builder image carries **CUDA 12.8**; the host driver was older,
   and NVIDIA's forward-compat shim covers datacenter GPUs only — **GeForce hosts must
   have driver ≥ the image toolkit**, which is why every earlier H100 run sailed and this
   3090 box refused. Profile gate raised: `min_cuda 12.0 → 12.8` (and the too-old host
   vanished from the filtered offers, confirming the gate).
2. `error while handling argument "--flash-attn": unknown value for --flash-attn:
   '--temp'` — llama.cpp master changed `-fa` to take `on|off|auto`; the image's
   `launch.sh` still passes it bare. Core's own `argv.rs` knows the new form; the
   *image script* predates it. Until the image is republished: patch
   `/app/launch.sh` over SSH in the build window.

Also: the downloader's `--include "*Q6_K*.gguf"` glob matched **both** `Q6_K` (22.9 GB)
and `UD-Q6_K_XL` (26 GB) — 46 GB pulled for a 23 GB run. The include must pin the exact
filename (`hf files` already knows it). And `apexrouter vast log` failed with its parse
error at the exact moment the container log held the fatal line — the defect that hides
other defects; fix it first.

**Attempts 3 & 4 — the CDI cohort, ~$0.04 combined, the night's biggest finding.**
Instances `46450832` (Quebec, unverified) and `46451325` (Wisconsin, **verified, rel
0.9992**) both died before any of our code ran:

```
OCI runtime create failed: … failed to inject CDI devices:
unresolvable CDI devices D.<sha256>/gpu=0, D.<sha256>/gpu=1
```

The `D.<hash>/gpu=N` names are **vast's own dynamically-generated CDI specs**, resolved by
the host's NVIDIA container toolkit before the image's first instruction; our create body
requests no devices at all (`create_body` is image+env+onstart — verified). Both corpses
were **driver 580.x / CUDA 13.0 hosts**; the Finland box (older driver) had created its
container fine. Raising `min_cuda` to 12.8 had steered the search into vast's
newest-driver cohort, where their CDI plumbing is currently broken — `reliability2`
measures uptime history and sees none of this. **The dodge: target
`12.8 ≤ cuda_max_good < 13.0`** (driver 570.x — new enough for the image's CUDA 12.8
userspace, below the broken cohort). Exactly one verified in-band host existed at search
time: offer `46451418`, Poland, driver 570.181, rel 0.9967, DOWN 1528.

Tooling consequences filed: the offers table should render `cuda_max_good`/
`driver_version` (they are already on `Offer`); profiles need a **max**-cuda bound, not
just min; and a `machine_id` blocklist would encode burned hosts instead of eyeballs.

**Attempt 5 — instance `46451932`**, offer `46451418` (Poland), $0.3436 + disk =
$0.3830/hr. `launch.sh` patched over SSH in the build window (flash-attn value + tight
quant glob) before its exec line ran. Image-track note (operator directive): refresh the
`vastai-gguf` ghcr images from **unsloth's known-working llama.cpp builds per hardware
class**, folding in the flash-attn and include-glob fixes.

**Box (measured):** Threadripper PRO 5945WX (12c/24t) · 128 GB RAM · WRX80 Creator R2 ·
PCIe 4.0 ×16 to both cards · 2× RTX 3090 (24,135 MiB each seen by CUDA) · KC3000 NVMe
(vast `disk_bw` 254). Machine `10212`, host `51345` — the campaign's first ★ favorite.

**MTP status upstream (the night's strategic finding):** `--spec-type draft-mtp` merged
into llama.cpp master 2026-05-16 (PR #22673, build **b8991**) and was *gone from master
again by 2026-05-22* — reverted for rework and still absent 2026-08-01 (our fresh master
build lists `--spec-type [none|ngram-*]` only, and *loading* an MTP GGUF without the flag
dies on `missing tensor 'blk.64.ssm_conv1d.weight'` — the draft head read as a plain
layer). Unsloth's docs still say "latest llama.cpp". **MTP on mainline is not currently
shippable**; the era-correct build is `pull/22673/head` (= tag b8991), fetchable forever.

**Measurements — Qwen3.6-27B dense Q6_K (22.5 GB), layer-split, master `beb42ff`,
fa on, KV q8_0 (server) / f16 (bench):**

| Cell | Result |
|---|---|
| C1 `llama-bench` | **tg128 34.42 ± 0.03 tok/s · pp512 1238 ± 27 tok/s** |
| C1 through the full stack | `route test garden-r1`: **33.96 tok/s** (proxy → tunnel → box; −1.4% vs raw — the relay is honest) |
| C4 streams (server `-np 4`, 256-tok gens) | 1× **33.1**/slot · 2× **28.2**/slot (agg 56, 85% eff.) · 4× **19.4**/slot (agg 78, 2.35×) |
| C2 MTP on master | **blocked upstream** (revert above) → R1b on b8991, same box — below |
| C3 Q4_K_M | run on b8991 — below |

**R1b — the same box on llama.cpp b8991 (`pull/22673/head`), all cells one binary:**

| Cell | Result |
|---|---|
| Q6_K `llama-bench` | tg128 **34.38** ± 0.02 · pp512 1235 (master parity — zero build drift) |
| Q4_K_M `llama-bench` | tg128 **43.76** ± 0.03 · pp512 1370 (+27% for one quality step down — still under 50) |
| **Q6_K + `draft-mtp` n-max 2, 1 stream, prose** | **56.9-57.0 tok/s** (×1.72) · acceptance 150/210 = 71% |
| **same, tool-call workload** | **60.6-61.5 tok/s** (×1.83) · acceptance 157/194 = **81%** |
| MTP, 2 streams | 32.2-32.9/slot (agg ~65; ×1.15 vs plain) |
| MTP, 4 streams | ~19.9/slot (agg ~79; ×1.03 — spec-dec washes out where batching already saturates, matching the PR's "parallel decoding unoptimized" note) |

**R1 verdicts:**

1. **The ≥50 tok/s target is met on the people's 48 GB rig** — Qwen3.6-27B Q6_K + MTP,
   single stream: 57 prose / 61 tools. Two streams sit at ~32; four at ~20. A Tier-B
   chat-heavy posture is one *fast* slot or two *decent* ones, not four fast ones —
   four fast slots is MoE territory (35B-A3B / 122B-A10B), R2+ business.
2. **The quant-vs-MTP question, same silicon:** Q6+MTP 57 > Q4-plain 43.8 — at *higher*
   quality. MTP dominates the trade at 1-2 streams and is neutral at 4. Acceptance is
   workload-shaped (81% structured vs 71% prose), so agentic/tool traffic — the colony's
   traffic — is MTP's best case.
3. **MTP ships only from `pull/22673/head` (= b8991) today.** Mainline merged it
   2026-05-16, pulled it by 05-22, and current master cannot even load the MTP GGUFs.
   The refreshed `vastai-gguf` images should pin b8991 (or unsloth's known-good build)
   for MTP recipes, and re-test mainline when the rework lands.
4. Building b8991 from a shallow PR fetch trips the webui asset downloader (no version
   string → HF bucket 404 → configure clobbers `build/tools/ui/dist`). Stub all four
   assets (`index.html`, `bundle.js`, `loading.html`, `bundle.css`) in that dir *before*
   `cmake --build` and it skips the download entirely. The image refresh should bake this.
5. Operator lessons, paid in retries: `pkill -f llama-server` over SSH kills the SSH
   session's own shell (`stall.rs` documents this exact trap — use `pkill -f
   "llama-serve[r]"`), and `sed -i` never reaches a script bash is already executing.

**Money, verified on the account:** credit $7.73 → **$7.23** — the whole five-attempt
night cost **$0.50** (vast bills pro-rata), for: the full dense/quant/MTP matrix on
reference Tier-B metal, four
distinct host-failure modes named, the CDI cohort dodge, the ★ favorites proof, and
eleven tooling findings. End state: `auto` and `garden-r1` point at backend
`node-127.0.0.1` (disabled after teardown so the table compiles); re-point `auto` at
`local-carnice-9b-q6_k` when the local endpoint next runs.

**Verdict vs GARDEN §4:** the ~25-35 bound for 2×3090 dense-27B measured at its top edge
(34.4). **The ≥50 tok/s/slot target is not met by dense-27B Q6 plain at any concurrency
on 48 GB consumer metal** — the gap is exactly MTP-sized (×1.4-2.2 ⇒ 46-73 single,
39-62 at 2 streams) or MoE-sized (35B-A3B / 122B-A10B). R1b decides the MTP half.

**More field findings (tooling, this run):**

- **★ Favorites is viable end-to-end, proven live.** `machine_id` is stable, unique, and
  queryable upstream: `PUT /search/asks/` with `{"q":{"machine_id":{"eq":136817},…}}`
  returned exactly that machine's three ask-slots (1×/2×/4× 3090). The provider's
  `QueryOverrides.extra` already reaches the wire verbatim (`build_query` merges it into
  `q`), so a favorites store is: starred `machine_id`s (+ ☠ burned ones) in state, a
  `--favorite`/`--machine` search path, and ★/☠ markers in the offers table. The REST
  `offers/search` currently drops `extra` — that seam needs exposing.
- **A stale row poisons every route edit.** With `auto → local-carnice-9b-q6_k` stale
  (backend gone after restart), `route set garden-r1 …` — a *valid new alias* — was
  rejected with the *old* row's compile error. Whole-table atomicity is working exactly
  as designed; the UX should say "the blocker is another row" and offer it by name.
  Workaround: repoint `auto` first. (Restore later: `apexrouter route set auto --target
  local-carnice-9b-q6_k` once carnice runs again.)
- **`backend add --tag` silently no-ops** ("apexrouter-client has no PATCH verb in this
  build") — printed workaround, but a flag that prints instructions instead of doing the
  thing is a defect. And a vast-tunnel backend auto-names itself `node-127.0.0.1`,
  which will collide with the *next* tunneled box; backend ids need the instance id.
- **`sed -i` on a running bash script does nothing to the running instance** — bash holds
  the old inode's fd; the first relaunch still exec'd the unpatched args (and the fat
  double-download re-ran for the same reason). Patch-then-*relaunch*, or patch before
  the script starts. Operator lesson, but also an image lesson: config belongs in env,
  not in script text.

**Field findings (tooling, found on the way):**

- **Search profiles: `ls`/`show` resolve built-in defaults, `offers --profile` does not.**
  `profile ls` and `profile show rtx3090-2-4` both render the default profile, but
  `/v1/vast/offers/search` answers `404 no search profile rtx3090-2-4` — the search path
  resolves only persisted catalog profiles. Two surfaces disagreeing about what exists is
  the LocalRouter disease; either defaults resolve everywhere or they are labelled
  `(builtin, save before use)` in `ls`. Workaround used: `profile new` (persisted
  `garden-r1-3090x2`).
- **`vast ls` shows dph_total incl. disk ($0.2097) while the offers table shows GPU dph
  ($0.1374).** Both true, nothing lies, but the jump surprises; the offers table could
  carry an `est w/ disk` column given `--disk-gb` context. UX note, not a defect.
- **The lemon was invisible to our own surfaces.** `vast ls`/`watch` showed
  `loading/pulling` forever; the *dashboard* showed the containerd I/O error. The vast
  instance row carries `status_msg` — `BootPhase`/`vast ls` should surface it the moment
  it stops being routine, and `watch` should call a phase that regresses or repeats with
  an error-bearing `status_msg` **dead**, not pending. Without that, the operator burns
  money waiting on a host that already said it was broken.
- **`apexrouter vast log <id>` failed with `could not parse …/log: expected value at
  line 1 column 1`** while the instance was mid-pull — the endpoint got a non-JSON body
  and surfaced a parse error instead of the body or a clean refusal. Small, real, filed.

---

## R2a — 1× RTX 5090, the 256k question — 2026-08-01

**Box:** instance `46457080`, offer `46391330`, Belgium (datacenter: EPYC 9354, Genoa
board, static IP, 128 threads, drv 570.211). $0.4944/hr incl. disk. Preceded by a
$0.01 abort: a "verified" Korea 5090 on an i5-12400F/H610M that never started —
host-class (cpu/mobo) must be visible pre-rent, `reliability2` sees none of it.

**The ladder (Qwen3.6-27B, llama.cpp master + b8991 `pull/22673/head`):**

| Config | VRAM | tok/s (proxy-measured) |
|---|---|---|
| Q6_K · 1×256k · q8_0 KV · plain | 30,796 / 32,607 MiB | **59.5** |
| Q6_K-MTP · 1×256k · q8_0 KV | **OOM** (MTP ctx buffers) | — |
| Q6_K-MTP · 1×256k · **q4_0 KV** | 28,484 MiB (4.1 GB free) | **103-105 prose · 106.7 tools** (acc 80.5/82%) |
| Q4_K_M · 2×256k · q8_0 KV | **OOM** (KV alone asked 17.4 GB) | — |
| Q4_K_M · **2×256k** · q4_0 KV | 26,328 MiB (6.3 GB free) | **~58/slot** (agg 115) |

**KV scaling law, measured:** ~29-30 KiB/token at q8_0, ~34 incl. overhead at 512k —
the hybrid SSM/attention stack (the `ssm_conv1d` tensors) makes 256k a consumer-card
context. Halving to q4_0 KV is the fitting lever; its recall cost is unmeasured (queued).

**R2a verdicts:** a single 5090 carries the ApexOS requirement grid: **one Q6 slot at
256k doing 107 tok/s on tool-calls (MTP), or two Q4 slots at 256k doing 58 each — with
embedder (and klein-Q4) headroom in both postures.** The 3090-pair remains the budget
path; the 5090 is the single-card answer.

**The money finding (the expensive lesson): metered bandwidth.** Credit $7.23 → $3.41.
Box-hours were ~$0.60; the remaining ~$3.2 is ~84 GB of model downloads on a host that
meters inbound at a datacenter-typical ~$35-40/TB. `Offer.inet_down_cost` was **in the
row we held at rent time** and no surface rendered it: not the offers table, not the
rent quote, not `vast ls`. The rent quote must add `est. download = payload × 
inet_down_cost` and the offers table needs a `$/TB` column — this is precisely the
silent-cost class the product exists to kill. (Also: the fat-glob double download
became a *money* bug here, not just a time bug.)

**pkill trilogy, completed:** (1) `pkill -f llama-server` over ssh kills the ssh shell
(documented in `stall.rs`); (2) bracketing the pattern is not enough when *the same
compound command* contains the literal string later (the launch path) — pkill matches
the whole remote command line; (3) `sed -i` can never patch the running script (bash
holds the old inode) — the boot-run *always* exec's unpatched args; patch, then
relaunch. The image refresh obsoletes all three by fixing the script at build time.

**Curiosity for the llama.cpp watchers:** upstream tag `b8991` currently resolves to
master's tip (`beb42ff`) — build-number tags appear re-pointed or frozen; `pull/22673/
head` (`2dff7ff`) is the only stable name for the MTP tree. Recorded so nobody trusts
b-tags for pinning again.

---

## Locked recipes & the forward file (checkpoint 2026-08-01, post-R2a)

**The mission this ledger serves:** find the optimal fully-offline model for ApexOS.
**Current champion: Qwen3.6-27B, unsloth quants** — native vision, hybrid SSM stack
(256k on consumer VRAM), MTP heads (×1.7-1.8 on agentic traffic). The local field moves
fast; challengers on file: Qwen3.5-122B-A10B (Tier C), Qwen3.6-35B-A3B (throughput),
and **PrismML Bonsai-27B ternary** (`github.com/PrismML-Eng/Bonsai-demo`, built *from*
Qwen3.6-27B; 1.58-bit reportedly holds up, 1-bit ~60-vs-70% on benches but tool-calls
fine; **custom runtime, not llama.cpp-mainline — queued as its own cell, a 1× 3090 fits
it and would be the fun test**).

**Locked host doctrine (what survived five failures):**
1. `verified: true`, **EU/Asia geo**, rel ≥ 0.995 — and read `cpu_name`/`mobo_name`
   before renting: an i5 on an H610M "verified" board is still a lemon.
2. **Driver band `12.8 ≤ cuda_max_good < 13.0`** (570.x) — below vast's CDI-broken
   580/CUDA-13.0 cohort, above the GeForce forward-compat wall.
3. **Check `inet_down_cost` — unmetered or bust** for model-pulling runs; datacenter
   $38/TB turned $0.60 of box-hours into a $3.80 leg. Home boxes are usually free.
4. DOWN ≥ 800 Mbps, exact-filename downloads only, one quant per run.

**Locked machines:** ★ `machine_id 10212` (host 51345) — Threadripper PRO / WRX80 /
2×3090 / PCIe4 ×16 / 128 GB RAM, Poland, ~$0.34/hr, unmetered, flawless through R1/R1b.
☠ offer `45761361` (Quebec $0.137 2×3090) — containerd disk death, relists anyway.

**Locked llama.cpp refs:** plain models → master (any recent); **MTP → `pull/22673/head`
only** (`2dff7ff`; b-number tags currently lie — b8991 resolves to master's tip). Builder
webui workaround: stub `index.html,bundle.js,loading.html,bundle.css` in
`build/tools/ui/dist` before `cmake --build`.

**Gotchas index for future-me:** the boot-run always executes the *unpatched* launch.sh
(bash holds the old inode — patch then RELAUNCH, never trust `sed -i` mid-run); never
put the literal string `llama-server` anywhere in an ssh command that also runs `pkill
-f llama-serve[r]`; `vast log` is broken exactly when you need it (raw API
`instances/` → `status_msg` is the truth channel); our `ls`/`watch` phases lag the
dashboard; `--mmap` does nothing for VRAM at `-ngl 999`; q4_0 KV is the fitting lever
at 256k+ (recall cost unmeasured — measure before shipping it as default).

**Next cells on the board:** R2b — ★10212, Andre's posture (`-sm none`, LLM whole on
GPU0, GPU1 free for klein/Wan/embedder budget); 6000 Ada dream preview (bandwidth-check
the host first); Bonsai-27B on 1×3090; the 122B-A10B when a 96 GB rig or the ternary
treatment lands. Credit at checkpoint: **$3.41** (top-up promised before next round).

---

## R2b — 1× RTX 3090 on ★10212, the used-market card alone — 2026-08-01

**Box:** instance `46503103`, the star's 1× slot (its 2× slot was rented out), $0.1916/hr
incl. disk, inbound ~$0.13/TB (effectively unmetered — home-box pricing confirmed).
Topology: GPU0, CPU affinity 0-23, no NUMA. Whole run **$0.11**; credit $18.37 → $18.26
(topped up by operator between runs).

| Cell | Config | VRAM | Result |
|---|---|---|---|
| B1 edge | MTP-Q5_K_M · 128k · q8 KV | **OOM** | as predicted (19.8+3.8+bufs > 24) |
| **B1** | MTP-Q5_K_M · 128k · **q4 KV** | 22.3 GB | **54.9 prose / 58.8 tools tok/s** (acc 70.5/79.6%) |
| B3′ | + Qwen3-Embedding-8B Q6 on **CPU** (12 threads), port 8001 | +0 GPU | **1.0 texts/s** (dim 4096) — and the LLM held 59.1 during it: co-hab is clean, the 8B is just too heavy for CPU |
| **B4** | 35B-A3B UD-Q4_K_M · 32k · q4 KV · plain | 21.6 GB | **146-149 tok/s**, and **~148/slot at 2× too** (MoE batching ≈ free); pp 900+ |

**R2b verdicts:**

1. **One used 3090 clears the ≥50 target**: Q5+MTP at 55-59 with 128k ctx (q4 KV — the
   operator-blessed pinch setting). The entry rig is real.
2. **The flex-model doctrine, measured**: 27B-Q5+MTP for the accuracy/coding lane;
   **35B-A3B for everything lighter at ~148 tok/s regardless of slot count** — on the
   same card, swap-not-cohab (both want ~22 GB).
3. **Embedder placement rule**: CPU co-hab is architecturally clean (zero LLM impact)
   but the 8B does 1 text/s on CPU — background-trickle only. Tier-A gardens carry
   **embeddinggemma-300M** on CPU; the 8B earns GPU residency only where spare VRAM
   exists (5090-class postures).

---

## R2c — 2× RTX 5090, the top-x090 colony rig — 2026-08-01

**Box:** instance `46506506`, offer `25673350`, Taiwan (Xeon Platinum 8352V, 72 threads
to our slot, both GPUs same NUMA node, no NVLink — irrelevant to this posture). $0.8756/hr
+ **metered inbound $9.1/TB, priced *before* renting** (~$0.65 for 74 GB incl. the boot
run's fat glob) — the Belgium lesson operating as doctrine. Whole run ≈ **$1.00**;
credit $18.26 → $17.26.

**The posture (Andre's design): each service owns silicon — no splitting.**

| GPU | Resident | VRAM |
|---|---|---|
| 0 | thinker — 27B Q6-MTP · 256k · q4 KV | 28.5 / 32.6 GB |
| 1 | worker — 35B-A3B UD-Q4 · 64k **+** embedder — 8B Q6 | 30.1 / 32.6 GB |

**Under simultaneous load (all three lanes fired at once):**

| Lane | Result |
|---|---|
| Thinker via proxy | **97.8-103.8 tok/s** (82% acceptance; ~3% below its solo 107) |
| Worker (on-box) | **172-214 tok/s** (172 during the embed burst, 214 clean) |
| Embedder | **69.4 texts/s** on GPU — vs 1.0 texts/s on CPU: the 8B embedder is a GPU resident, full stop |

**R2c verdict — the consumer ladder is complete:**

| Rig (street) | What it runs |
|---|---|
| 1× 3090 (~€600 used) | accurate lane 55-59 @128k *or* flex lane ~148; 300M CPU embedder |
| 2× 3090 (~€1.2k) | one fast MTP slot / two decent, split posture pending (★10212's 2× slot) |
| 1× 5090 | the whole grid: 107 @256k or 2×256k @58 + embedder room |
| **2× 5090** | **the colony: 100+ thinker @256k, 200+ worker, 70/s embeddings — simultaneously** |

Creative co-residency (klein/Wan on GPU1's remaining budget) awaits the ComfyUI arc (R3)
— GPU1 as configured retains ~2.5 GB; the *studio* posture swaps the worker for klein+Wan.
Swarm note for the mandala system (wip): A3B at 172-214/slot on 5090-class silicon is
fan-out fuel — well-defined delegated tasks at ~thruput/cost no big model matches.

---

## R4 — the 122B on modded silicon, and the China playbook — 2026-08-01

**Box:** instance `46509449`, **★ machine `140330` / host `113492`** — Supermicro
X11DPG-OT dual-socket, Xeon Gold 6133 (80 threads to slot), 515 GB host RAM, 1.9 GB/s
NVMe, **2× RTX 4090 modded to 49,140 MiB each**, watercooled (27-29 °C), Guangdong.
$0.8361/hr incl. disk. Driver 580.159 — the CDI-corpse driver — **created its container
fine**: the 580-cohort curse is host-config, not driver law; the band rule stays as a
default, not a ban.

**The numbers (llama.cpp b8991, `draft-mtp` n-max 2, UD-Q4_K_M 78.3 GB, 64k ctx q8 KV,
VRAM 39.2 + 39.6 GB, ~9 GB headroom per card):**

| Cell | Result |
|---|---|
| prose 1× | **99-102 tok/s** (acc 80%) |
| tools 1× | **100-107 tok/s** (acc 87%) |
| route test through proxy+tunnel | 80 tok/s |

**A 122B MoE at triple the dense-27B's speed, on ~€2k of modded consumer cards.** The
garden's brain-tier is rentable at 84¢/hr today and ownable by mortals tomorrow.

**Field test (operator-driven):** a stock hermes-agent pointed at 8888 worked zero-config
(the promise held). Unconstrained creative-coding at 64k produced `~/Projects/
NeuralSymphony` — CerebroCortex memories → music (types=instruments, salience=dynamics,
links=harmony, Suno render): idea-quality exceptional *because grounded in the discovered
environment*; execution buggy and context-starved (hermes hit compaction spirals on pass
two). **Doctrine: 122B agentic coding wants 128k minimum, 256k preferred** — this box has
the headroom; rematch queued.

**The China playbook (paid for in hours):** huggingface.co is hard-blocked (Errno 101) —
`HF_ENDPOINT=https://hf-mirror.com` works but throttles per-connection (18 MB/s → 5-8,
and `hf_transfer` made it *worse*, 40 Mbit wedge); **aria2c -x8 straight off the mirror
= 4-6×**, resuming hf's `.incomplete` files after mapping them to shards **by etag ↔
filename-hash, proven via HEAD** (never guess the mapping). github.com is throttled-not-
blocked (retry loops win). **ModelScope answers in 1.6 s from inside — first choice next
CN run.** Vast's ssh-proxy reverse-listener never bound for this box (endless port-29448
failures): **direct host:port SSH is the CN doctrine**, and the daemon's `tunnel up` +
the UI Tunnel button both fail on the proxy path — the tunnel verb needs a direct-port
fallback (instance row carries ip + mapped port).

**More tooling findings:** `snapshot.instances` is `[]` while `/v1/vast/instances`
reports the fleet — the Fleet & cost page renders the snapshot, so rented boxes flash
(the panel's own fetch) then blank (next snapshot stomps it). Same root as the rent-job
gap: rentals never feed daemon state. — The rent pre-check tests membership in the
profile's **top-N-by-price**, so a constraint-satisfying offer loses to cheaper stock
boxes (the modded 96 GB rig was unrentable until a CN-scoped profile put it in-list);
and these hosts **re-mint ask ids per search snapshot**, so rent-by-id races: `--auto`
with a discriminating profile is the doctrine, and favorites must key `machine_id`. —
Sixth pkill act: the kill pattern and the relaunch text must not share a command line.

**Economics:** park-don't-destroy measured across our five hosts at **$0.12-0.19/GB/mo**
→ $3-6/week holds 100-150 GB of models + builds. `vast park`/`wake` verb pair filed
(stop reserves disk, not GPUs — restart contends). This box parks tonight per operator.

---

## mk1.1 — the campaign's defect list, closed — 2026-08-02

Every tooling defect this ledger filed with a reproduction is fixed, tested and
live-proven on the running daemon (CHARTER amendment 2026-08-02 records the decisions):

1. **The rent job finishes the chain.** `auto_tunnel`/`bind_alias` were write-only; the
   job now runs rent → boot → tunnel → endpoint record → backend → alias, and a failure
   after "healthy" alerts with the manual command instead of failing silently while the
   box bills. `rented_backend()` is the one constructor both the job and `Provisioner::up`
   build the row with.
2. **The Fleet & cost page no longer blanks.** `Snapshot.instances` serves a fleet cache
   (`AppState::fleet`) fed by a poller (`[providers.vast] fleet_poll_secs`, default 60)
   and by every handler that reads the fleet; totals carry live credit, parked-aware burn
   and burn-down. Verified live: instance 46509449 in `GET /v1/snapshot` with machine_id.
3. **`status_msg` is surfaced, and a repeating fatal line is death.** The boot watchdog
   streams changed status lines as instance log events (`watch` prints them), and an
   identical fatal-looking line repeating 120 s with no phase advance gets the expire
   treatment instead of burning the boot budget. The R1 lemon dies in 2 minutes now.
   `ls`'s NOTE shows progress chatter while booting and errors always — one helper pair
   on `VastInstance` (`status_note`/`status_looks_fatal`) feeds every surface.
4. **The pre-check tests constraints, not price-window membership.** A named offer
   outside the profile's top-N is fetched by id and judged by `constraint_failures()`,
   which names what actually failed; `--machine <machine_id>` pins a host through the
   re-mint churn (server-side `{"machine_id": {"eq": N}}`).
5. **SSH prefers the direct port.** `ssh_endpoint` reads the 22/tcp mapping
   (`ip:port`) before falling back to the `sshN.vast.ai` proxy — the CN doctrine, now
   the default for tunnels, diagnose and restart-download alike.
6. **Favorites are real.** `$STATE/favorites.json` keyed `machine_id`; `vast star|unstar|
   favorites`; ★/☠ + MACHINE + CPU columns in the offers table; ☠ machines excluded from
   anonymous auto-picks (with a banner) and warned about when named. ★ 10212 and ★ 140330
   are stored with their campaign notes.
7. **`vast park`/`wake` exist.** `stopped` is `BootPhase::Parked` (not Destroyed!), park
   verifies the stop and ledgers the weekly disk figure, wake is SpendApproval-gated and
   re-parks itself if GPUs are gone at boot-budget expiry. Park/Wake buttons on the web
   UI instance card.

Still open from the campaign file: image refresh (unsloth builds baked in, MTP pin,
HF_ENDPOINT/ModelScope), `backend add --tag` no-op, backend ids from instance ids on
tunnel adoption, `vast log` non-JSON body parse, R3 ComfyUI studio arc, Bonsai-27B cell.

---

## R3 — the studio cell: image + video on the ★ pair — 2026-08-02

**Box:** the same ★ `machine 140330` (instance `46509449`, Guangdong, 2× modded-48GB
4090, $0.8361/hr) — repurposed live, no re-rent: the 122B was stopped by pid (its 78 GB
GGUF kept on the 2 TB disk for instant rehire), and the studio moved into the freed VRAM.
Engine: **ComfyUI headless** (`e803f24`), driven purely through its HTTP API with fixed
workflow JSONs — the node UI never opened. Two instances, one per card, ports 8188/8189,
loopback-only, direct-port tunnels home: **each service owns silicon.**

**Setup, timed:** 4.5 min from bare box to both lanes answering (apt + venv + torch
2.13.0+cu130 via the tuna mirror + ComfyUI + deps). Models via **ModelScope** (1.2 s
answer from inside CN, ~19 MB/s aggregate): Wan 2.2 TI2V-5B fp16 + umt5-xxl fp8 + VAE;
Qwen-Image 20B fp8 + Qwen2.5-VL-7B fp8 + VAE. One trap: the fp8-scaled path JIT-compiles
a triton shim and needs **`python3.12-dev`** (Python.h) — first image job died in 1.9 s
without it. And the fetch globs matched three 20 GB Qwen fp8 variants instead of one —
the anchored-glob lesson applies to ModelScope `--include` patterns too.

**The numbers (proxy of record: ComfyUI's own `Prompt executed` line):**

| Lane | Model · workload | Cold | Warm, BOTH LANES FIRING | VRAM |
|---|---|---|---|---|
| video GPU0 | Wan 2.2 TI2V-5B fp16 · 1280×704 · 81 f @ 24 fps · 20 steps | 143.6 s | **95.2 s** (3.94 s/it) | 21.6 GB |
| image GPU1 | Qwen-Image 20B fp8-hq · 1328×1328 · 20 steps | 62.7 s | **29.3 s** | 30.5 GB |

**R3 verdicts:**

1. **Zero contention.** Simultaneous warm runs, both cards at 100%, 61/65 °C
   (watercooled): the video lane ran *faster* under contention than its solo cold run —
   separate processes on separate cards simply do not see each other. The no-splitting
   doctrine, validated for the studio posture.
2. **The ★ pair is a complete two-lane studio at $0.84/hr**: ~4 s/frame-batch video,
   sub-30 s stills, with headroom on both cards (27 GB free on GPU0 — the Wan 14B fp8
   pair fits; 18.6 GB on GPU1 — an embedder or a second image model fits).
3. **ComfyUI-as-engine works exactly as the arc predicted**: fixed workflow JSONs map
   onto ApexRouter's recipe concept; agents never see the graph. Front door remains
   Imaginarium (its local-provider arc is queued in that repo). sd.cpp stills bake-off
   still open.

**Filed on the way:** `python3.12-dev` belongs in any future studio image; ModelScope
include patterns need the same anchoring as hf; first-frame outputs land in
`ComfyUI/output/` and served via `/view` through the tunnel — no extra fileserver needed.
