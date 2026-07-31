# Migration from LocalRouter

> Normative source: `ARCHITECTURE.md` §5.4 (migration and compatibility with `~/.vastai-gguf`),
> §5.3 (the state directory), §7 (the CLI surface). Where this document and `ARCHITECTURE.md`
> disagree, `ARCHITECTURE.md` wins. Where either disagrees with
> `docs/port/00-machine-ground-truth.md`, the ground truth wins.
>
> Everything below was run end-to-end against the real `~/.vastai-gguf` on the build machine, and
> is pinned by `crates/apexrouter-cli/tests/migrate_e2e.rs`.

> [!IMPORTANT]
> **`~/.vastai-gguf` is another tool's state directory. ApexRouter reads it and never writes it.**
> `migrate` — with or without `--apply` — opens nothing there for writing. If you want to satisfy
> yourself, hash it either side of a run; that is exactly what the e2e test does, on every
> invocation, for the copy *and* for the real directory.

---

## 1. What migration is for

LocalRouter kept its state in two places, one of them wrong:

```
~/.vastai-gguf/                     the state directory
├── config.toml                     [providers.together] base_url + api_key   (a REAL key)
├── .pinned_provider                {"provider","model_id","base_url"}
├── usage.log                       JSONL, legacy field names
├── local_instances/<name>.json     a saved llama-server, usually stale
└── local_logs/                     historical llama-server logs

<LocalRouter checkout>/             …and four state files written INTO THE REPO
├── .active_endpoint                what was last activated (four different shapes)
├── .last_instance                  a vast instance id that may STILL BE BILLING
├── .instance_history               <timestamp>\t<instance id> per line
├── .hf_pin                         a transient download wizard default
└── recipes.toml                    71 recipes, 19 gpu tiers, a docker image map
```

ApexRouter keeps everything under one XDG-ish state dir (`$APEXROUTER_HOME`, default
`~/.local/state/apexrouter`) and never writes into a repo directory. `apexrouter migrate` is the
one-way bridge: it **reads** both trees and **writes** only into `$APEXROUTER_HOME`.

The legacy tree stays readable afterwards, forever, while `[compat] read_legacy_state = true`:
`usage.log` keeps being merged into every usage aggregate, and `config.toml` remains step 3 of the
credential chain. Migration is not a cutover. Nothing is moved, copied out, or invalidated.

---

## 2. The three invocations

```sh
apexrouter migrate                       # print the plan. THE DEFAULT. Writes nothing.
apexrouter migrate --dry-run             # the same thing, said out loud
apexrouter migrate --dry-run --json      # the same thing, as a MigrationPlan
apexrouter migrate --apply               # import the rows the plan marks import/warn
apexrouter migrate --from ~/.vastai-gguf --localrouter ~/Projects/…/LocalRouter
```

A bare `apexrouter migrate` prints the plan and stops. Reading somebody else's state directory and
rewriting your own config on the strength of a verb with no adverb is not a defensible default —
and the plan is the thing you are supposed to read.

`migrate` is a `Need::Pure` verb: no daemon is contacted and none is started, because `--apply`
writes `config.toml`, `catalog.toml` and the ledger, and a daemon holding a copy of all three would
go stale the instant the import landed.

**`--from`** re-roots the *legacy half* of path resolution only: `$APEXROUTER_HOME`,
`$APEXROUTER_CONFIG` and `$XDG_CACHE_HOME` are pinned to what this process already resolved before
`$HOME` is moved, so your own state cannot move underneath you. A `--from` whose basename is not
`.vastai-gguf` is **refused**, not silently ignored — that directory name is the whole of the legacy
contract. **`--localrouter`** sets `$APEXROUTER_LOCALROUTER_DIR`; without it the checkout is probed
at `~/Projects/Inference/tools/LocalRouter`, `~/Projects/LocalRouter`, `~/LocalRouter`,
`~/src/LocalRouter`, and a directory only counts when it carries `endpoint_proxy.py` or
`recipes.toml`.

---

## 3. Reading the plan

Every row is `ACTION · WHAT · FROM · WHY`, and **every row carries a reason** — including each of
the 54 recipes that are deliberately not imported. That is the point: you are meant to be able to
read the reason and disagree with it.

| Action | Meaning |
|---|---|
| `import` | It will be written into `$APEXROUTER_HOME` by `--apply`. |
| `warn` | It will **also** be written, and there is something you should know first. |
| `skip` | Nothing will be written. Informational rows (credentials, `usage.log`, `.hf_pin`) are all `skip`. |

On the build machine, against the real trees, the plan is **107 rows: 16 `import`, 23 `warn`,
68 `skip`** — `apexrouter migrate --dry-run --json | jq '.items | group_by(.action)[] | {a: .[0].action, n: length}'`.

---

## 4. What `--apply` writes, and where it comes from

| Legacy artefact | Lands in | As |
|---|---|---|
| `config.toml` `[providers.*]` | `$APEXROUTER_HOME/config.toml` | `base_url` **verbatim** + an `api_key_env` **reference**. Only providers the config does not already carry. |
| `recipes.toml` `[docker]` | `config.toml` `[docker]` | the three image families ApexRouter publishes (`prebuilt`, `builder`, `vllm`). |
| `recipes.toml` `llama_cpp_repo`/`_ref` | `config.toml` `[known_forks.*]` | 7 mappings. Genuinely undiscoverable knowledge — a model that needs a fork of llama.cpp. |
| `recipes.toml` `[gpu_tiers.*]` | `catalog.toml` `[[profiles]]` | 19 `SearchProfile` seeds. `vram_gb` is **per GPU** and is multiplied by `num_gpus`; legacy `RTX_5090` names are rewritten to the live vocabulary `RTX 5090`. |
| `recipes.toml` `provider = "together"` rows | `catalog.toml` `[[recipes]]` | 7 `Managed` recipes, base URL verbatim. |
| `recipes.toml` `provider = "local"` rows | `catalog.toml` `[[recipes]]` | 3 local recipes, re-validated against what is actually on disk. |
| `local_instances/*.json` | `catalog.toml` `[[recipes]]` | the saved llama-server, as a local recipe. |
| `.pinned_provider` | `catalog.toml` `[[recipes]]` | one `Managed` recipe (the live file pins `deepseek-ai/DeepSeek-V4-Pro`). |
| `.last_instance` | `ledger.jsonl` | one row, `Confirmed`, `approval_source = "migrate"`. |
| `.instance_history` | `ledger.jsonl` | one `Destroyed` row per superseded id, with the destruction marked **assumed, never observed**. |

Nothing else is written. `usage.log`, `local_logs/`, `.active_endpoint`, `.hf_pin` and both
third-party credential files are read where they are and stay there.

`--apply` is safe to re-run. Providers, forks, recipes and profiles are inserted only when absent,
and a ledger row already carrying the import marker is not appended twice.

---

## 5. Credentials: a reference, never a copy

The real `~/.vastai-gguf/config.toml` on the build machine holds a live Together key. **It is not
copied.** What is imported is where the key lives:

```toml
[providers.together]
base_url    = "https://api.together.ai/v1"   # verbatim; api.together.xyz is never rewritten to .ai
api_key_env = "TOGETHER_API_KEY"             # a REFERENCE. The key stays where it was.
```

Three consequences worth internalising:

- The plan is printed, so **no structure that reaches it may be able to hold key material.**
  `LegacyActiveEndpoint` therefore records `api_key_present: bool` and discards the plaintext key
  that shape 3 of `.active_endpoint` embeds. The e2e test greps every byte of stdout, stderr and
  every written file for both keys.
- `~/.config/vastai/vast_api_key` and `~/.cache/huggingface/token` are read **in place, at their
  owners' conventional paths**. They are never copied into ApexRouter's state; the plan says so and
  marks them `skip`.
- Nothing breaks if you later delete the reference: the legacy `config.toml` remains step 3 of the
  credential chain for as long as `[compat] read_legacy_state` is on.

---

## 6. `usage.log` is merged, never copied

The legacy rows are read **in place** and merged into every usage aggregate. Copying them into
`$APEXROUTER_HOME/usage.jsonl` would double-count every row for as long as `read_legacy_state`
stays on, so the plan marks `usage log` as `skip` and says exactly that.

```sh
apexrouter usage --since all --by provider --json     # answers with nothing running
```

On the build machine that reports `"rows": 4` — the four legacy rows from 2026-05-02 — with the
legacy `vast-gguf` provider spelling intact. **No row can ever fail to load**: `epoch` is optional,
unknown fields survive via `flatten`, and the legacy `%Y-%m-%dT%H:%M:%SZ` local-time-with-a-lying-`Z`
timestamps parse leniently.

> [!WARNING]
> **`--apply` turns the usage mirror on.** The `usage mirror` row is a `Warn` you are meant to
> strike out, and the CLI has no way to strike a row — so a plain `--apply` keeps it and writes
> `[compat] mirror_usage_log = true`. From then on **the daemon appends every new usage row to
> `~/.vastai-gguf/usage.log`**, which is the one and only thing in ApexRouter that writes into the
> legacy directory. That is the case the setting exists for (the old LocalRouter TUI's `cost.py`
> view keeps working during a transition), but it is opt-**out** here and opt-in everywhere else.
>
> To decline it, after `--apply`:
>
> ```sh
> apexrouter config edit          # set [compat] mirror_usage_log = false
> apexrouter config show --json | jq .compat.mirror_usage_log
> ```
>
> An acceptance run once added 15 rows to the real `usage.log` and they had to be restored by hand.
> Check it.

---

## 7. Stale state is the normal case

The saved instance on the build machine points at `~/models/Qwen3.5-9B-Q4_K_M.gguf`, which was
deleted months ago. Two of the three local recipes in `recipes.toml` point at the same missing file.
None of that is an error:

```
warn  local instance  …/local_instances/local-qwen35-9b.json
      saved local endpoint `local-qwen35-9b` → recipe. model `~/models/Qwen3.5-9B-Q4_K_M.gguf`
      NO LONGER EXISTS; build `build-vulkan` still exists. A saved instance pointing at something
      you deleted is normal, not an error — `apexrouter recipe validate` reports it as a Warning
      with a fix.
```

The recipe is still imported, so the knowledge in it is not lost, and `apexrouter recipe validate`
is where you go to clean up. Migration that refused to run because somebody's model moved would be
useless on every machine that has been in service longer than a week.

---

## 8. What is deliberately **not** imported

- **The 54 `vast_gguf` recipes.** They are a frozen function: a hand-solved `(model, quant, gpu
  tier) → (ctx, parallel, kv_type, ngl)` table, computed once against GPU prices and llama.cpp
  behaviour that have both moved. `fit()` solves the same problem against the rig in front of it.
  Every one of the 54 gets its own plan row saying so, with the tier arithmetic spelled out
  (`2× 80 GB per GPU = 160 GB pooled`), so you can check the claim rather than take it.
- **The 7 `vllm` rows**, for the same reason.
- **`.active_endpoint`.** Informational only. The default route is set explicitly with
  `apexrouter route set default <alias>`, never inherited from a stale file. LocalRouter had four
  implementations of "what is active" that disagreed; ApexRouter has one `resolve()`.
- **`.hf_pin`.** A transient wizard default, not durable state. Re-pin with `apexrouter hf get`.
- **`[docker] prebuilt_legacy`.** It names an image family ApexRouter does not publish.

---

## 9. Verifying a migration

The whole procedure, hermetic and reversible, which is also what the e2e test does:

```sh
# 1. fingerprint the legacy tree
find ~/.vastai-gguf -type f -print0 | sort -z | xargs -0 sha256sum > /tmp/legacy.before

# 2. read the plan. This writes nothing, anywhere.
apexrouter migrate --dry-run

# 3. import, into a scratch state dir first if you want to look before you leap
APEXROUTER_HOME=/tmp/apexrouter-trial apexrouter migrate --apply

# 4. prove the legacy tree did not move
find ~/.vastai-gguf -type f -print0 | sort -z | xargs -0 sha256sum | diff /tmp/legacy.before -

# 5. read what landed
apexrouter recipe ls --json | jq '.data | length'      # every --json reply is an envelope
apexrouter profile ls --json | jq '.data | length'     # whose payload is `data` unless flattened
apexrouter vast ls --json                       # the seeded ledger rows: money stays visible
apexrouter usage --since all --by provider      # the legacy rows, merged in place
apexrouter config show --json | jq '.compat'    # check mirror_usage_log before you walk away
```

**Rollback is `rm -rf $APEXROUTER_HOME`** — or, less brutally, delete the imported rows with
`apexrouter recipe rm` / `apexrouter profile rm`. Migration is import-only; there is nothing to undo
on the legacy side because nothing on the legacy side was touched.

The `.last_instance` warning deserves a stop-and-read:

```
`.last_instance` still names vast instance 25731461. LocalRouter deletes that file when it
destroys an instance, so this one may STILL BE BILLING. It is imported as a Confirmed ledger row
so `apexrouter vast ls` keeps it visible until you verify it.
```

It is imported as `Confirmed`, **not** `Reconciled`: nobody has asked vast.ai whether that box is
alive. It stays in `ledger.active()` until a destroy is verified, which is the entire purpose of the
ledger — a leak must be visible.

---

## 10. Open items (as of the I-01 gate)

Recorded here rather than remembered, each with the owner it belongs to.

1. **`apply` can write two recipes under one id.** `core::migrate::apply` de-duplicates recipe ids
   against the catalog it is writing into, but not against the batch it is writing. On the real
   machine both `local_instances/local-qwen35-9b.json` and `recipes.toml#recipes.local-qwen35-9b`
   mint `local-qwen35-9b`, so `catalog.toml` ends up holding two recipes under that id: the second
   is unreachable (`recipe show` finds the first) and `recipe rm` deletes both. A measurable second
   symptom is that such a `catalog.toml` does not survive its own `toml_edit` round-trip byte-for-
   byte. Remove the collision and `--apply` is byte-idempotent from the first run.
   `crates/apexrouter-cli/tests/migrate_e2e.rs::apply_never_writes_two_recipes_under_the_same_id`
   is written and `#[ignore]`d; delete the attribute with the fix. Owner: unit C-16
   (`core/src/migrate.rs`) — the fix is to carry one `used ids` set across the whole survey, or to
   route the batch through `catalog::upsert_recipe`, which already generates unique ids.
2. **There is no way to strike a row through the CLI.** The plan exists so a human can delete rows
   before `--apply`, and `core::migrate::apply` honours that (a row downgraded to `Skip` is not
   written) — but no surface exposes it. `apexrouter migrate --apply` recomputes the plan and passes
   it whole. This is what makes §6's mirror warning necessary.
3. **`POST /v1/migrate` is documented and not built.** It is in `ARCHITECTURE.md` §6 and in
   `openapi/apexrouter-v1.yaml`, and `crates/apexrouter-server/tests/openapi_routes.rs` lists it in
   `PENDING` as owed by "I-01/S-08". No unit's file-ownership list in `BUILD-PLAN.md` §5 contains
   `crates/apexrouter-server/src/api/migrate.rs`, so it is documented, unowned and unbuilt. Building
   it needs a new `api` module **and** its one `.merge(…)` line in `server/src/lib.rs::v1_routes()`,
   which only S-01 may edit — see `CLAUDE.md`, "mount it, don't describe it". Its request body
   (`MigrateRequest { dry_run }`) also cannot express item (2) above, so the two should be designed
   together.
