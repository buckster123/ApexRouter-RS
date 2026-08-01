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
