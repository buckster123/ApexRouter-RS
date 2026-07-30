# 07 — Known Issues, Agent Surface, and Intended Direction

**Sources read in full**

| File | Date | What it is |
|---|---|---|
| `LocalRouter/AUDIT_REPORT.md` | 2026-05-03 | Line-by-line audit of the **pre-refactor** single-file `vast_manager.py` (3064 lines, then living at `~/Projects/qwen36-vast/`) |
| `LocalRouter/ARCHITECTURE.md` | (same era) | Architectural map of that same single file — call graph + proposed module split |
| `LocalRouter/REVIEW_FINDINGS.md` | 2026-07-20 | Two-agent review of the **post-refactor** `localrouter/` package; fixed vs. deferred |
| `LocalRouter/docs/SKILL.md` | v0.3.0 | The agent-facing skill card — this is what ApexOS / Claude Code have been told LocalRouter *is* |
| `LocalRouter/.hermes/plans/2026-04-30-manager-flexibility.md` | 2026-04-30 | The author's own forward plan (4 phases + open questions) |

Cross-checked against live source (`localrouter/config.py`, `providers.py`,
`local_endpoint.py`, `cost.py`, `endpoint_proxy.py`, `pyproject.toml`,
`vast_up.sh`, `README.md`) so the wire/state formats below are the real ones,
not doc drift.

**Two timelines matter.** The AUDIT_REPORT bugs describe a file that no longer
exists in that form — the refactor to `localrouter/` fixed 3 of its 4 crash-class
bugs. They are still worth cataloguing because *the shape of the failure* tells
you what the Rust type system must make unrepresentable. The REVIEW_FINDINGS
items are current-generation and much more interesting: they are the bugs that
survived a rewrite, which means "write it more carefully" already failed as a
mitigation once.

---

# Part 1 — Known bugs, design flaws, rough edges

Each entry: **what actually went wrong** → **why the language/design allowed it**
→ **the structural fix in Rust** (a type, an ownership rule, an API shape — never
"remember to check").

## Class A — Money-safety (the expensive class)

### A1. Instance created but never recorded — silent billing leak
*Source: REVIEW_FINDINGS, P0, fixed on `review/correctness-fixes`.*

`vast_up.sh` ran `vastai create ... --raw 2>&1`, folding **stderr into the JSON
blob** handed to `jq`. The vastai CLI's habit of printing a status line before
the JSON made `jq` fail; `INST_ID` came out empty; the trailing `|| true`
swallowed the failure even under `set -o pipefail`. Net effect: a GPU box was
**running and billing** with no local record, invisible to `vast_down.sh`.

Fix applied: capture stdout only, slice from the first `{`, hard-error with
recovery steps if no ID parses, warn before overwriting a still-tracked
`.last_instance`, append every launch to `.instance_history`.

**Rust structural fix**
- Never shell out to `vastai` for anything that creates or destroys billable
  resources. Talk to the Vast REST API with `reqwest` + `serde` and a typed
  `VastInstance { id: InstanceId, .. }`. Parsing is `serde_json::from_slice`
  into a struct — a status-line prefix is a hard parse error, not an empty
  string.
- When shelling out is unavoidable, the wrapper returns
  `Result<Stdout, CmdError>` with **stdout and stderr as separate fields**.
  Make it impossible to merge them: no `2>&1` codepath exists in the API.
- Model the creation as a two-phase commit with a guard type:
  `let pending = ledger.reserve(&spec)?;` → API call → `pending.commit(id)?`.
  `impl Drop for PendingLaunch` writes an `orphan-suspect` record if the guard
  is dropped without commit — that closes the Ctrl-C window the review flags as
  still-open (see A3).
- `InstanceId` is a newtype over a validated `u64`/string. An empty ID is not
  constructible, so "empty ID silently accepted" is a compile-time impossibility.

### A2. `.last_instance` is a single slot — launching B orphans A
*Source: REVIEW_FINDINGS, deferred.* Mitigated with a warning + `.instance_history`
append, not fixed.

**Rust structural fix** — replace the single-slot file with an append-only
**instance ledger** (`instances.jsonl` or a small SQLite/`redb` table) keyed by
instance id, each row carrying `{id, provider, created_at, destroyed_at,
last_seen_status, est_cost_usd}`. "Active" becomes a *query* (`WHERE
destroyed_at IS NULL`), not a file that can hold exactly one thing. Then
`apexrouter vast list --live` and a startup reconciliation pass against the Vast
API can surface orphans automatically.

### A3. No `trap` over the create→record window
*Source: REVIEW_FINDINGS, deferred.* Ctrl-C in the sub-second gap between
`vastai create` returning and `.last_instance` being written still leaks a
billing instance.

**Rust structural fix** — the `PendingLaunch` Drop guard from A1, plus install a
`tokio::signal` handler that sets a shutdown flag rather than killing the
process mid-critical-section. Critical sections that must complete (ledger
writes) run inside a task the shutdown path `await`s. Crash-safety comes from
writing the reservation record *before* the API call, then marking it
`confirmed`/`failed` after — a crashed process leaves a `reserved` row that
reconciliation investigates.

### A4. A failed Together activation fell through into the **paid** GPU wizard
*Source: REVIEW_FINDINGS, reviewer M1, fixed.* The Together branch of
`menu_launch` was not terminal; a failed API-key test dropped the user into the
Vast rental flow.

**Rust structural fix** — the launch flow is a state machine over an enum, not
sequential ifs. `fn launch(provider: Provider) -> Result<Launched>` dispatches
to per-provider functions that each return; there is no shared fallthrough tail.
Any state transition that can spend money requires an explicit
`SpendApproval { max_usd_per_hour, confirmed_at }` value threaded as a
parameter — you cannot reach the rental call without constructing one.

### A5. Cost model is hardcoded and stale
*Source: AUDIT TOGETHER-03 / CQ-06.* Together pricing hardcoded at lines
718-726; `cost.py` still uses a flat `avg_rate = 0.88 / 1_000_000` for every
Together model and `hours_used * 0.50` for Vast.

**Rust structural fix** — pricing is **data**, not code: a `pricing.toml`
(overridable, refreshable from the provider's models endpoint) deserialized into
`PriceTable { per_provider: HashMap<ProviderId, ModelPricing> }`. Cost values
are a `Money` newtype (integer micro-dollars) so float dust cannot accumulate,
and any estimate produced from a *default/fallback* rate is tagged
`Estimate::Approximate` vs `Estimate::Metered` so the UI can't present a guess
as a fact.

### A6. Token counts logged as floats
*Source: AUDIT BUG-07.* `prompt_tokens=len(prompt.split()) * 1.3` wrote a float
into `usage.jsonl` where an int was expected.

**Rust structural fix** — `struct Usage { prompt_tokens: u32, completion_tokens: u32 }`.
The heuristic estimator returns `u32` by construction and is a distinct type
(`TokenCount::Estimated(u32)` vs `TokenCount::Reported(u32)`) so a downstream
cost report can say which it is.

## Class B — Secret handling

### B1. Client `Authorization` forwarded to third-party backends
*Source: REVIEW_FINDINGS, P1, fixed.* When the active provider had no auth of
its own (rented Vast box; Together with a missing key), the inbound client's
key was relayed straight to the third party.

**Rust structural fix** — make header **construction**, not filtering, the only
path. The proxy builds an outbound `HeaderMap` from an allowlist derived from
the inbound request plus a provider-supplied `Credential`; the inbound
`HeaderMap` is never cloned wholesale. Type it:
`fn outbound_headers(inbound: &HeaderMap, cred: Option<&Credential>) -> HeaderMap`
and unit-test that `authorization`/`proxy-authorization`/`cookie` never appear
unless `cred` provided them.

### B2. Wrong provider's key sent to Together
*Source: REVIEW_FINDINGS, P1, fixed.* The `config.toml` reader grabbed the
**first `api_key` line in the whole file**, regardless of section. This is a
hand-rolled line-scanning "TOML parser" — see the same root cause in E1. The
current `endpoint_proxy.py` still hand-parses `[providers.together]`
line-by-line rather than using `tomllib`, because the proxy avoids importing the
package.

**Rust structural fix** — one config type, one parser: `#[derive(Deserialize)]
struct Config { providers: HashMap<ProviderId, ProviderConfig> }` via `toml`.
Keys are reachable only as `cfg.providers[&id].api_key` — there is no "first
api_key in the file" expression to write. The proxy and the TUI **share the same
crate module**; there is no second, weaker parser.

### B3. API keys in plaintext, and in `ps` output
*Source: AUDIT SEC-03 / SEC-04.* Keys written unprotected to
`~/.vastai-gguf/config.toml` and to `.active_endpoint`; keys passed as `curl -H`
arguments (visible in `ps`); `HF_TOKEN` visible in `ps` locally and stored in
the Vast instance env (REVIEW_FINDINGS, deferred — partly inherent to the
create API).

**Rust structural fix**
- A `Secret<String>` newtype whose `Debug`/`Display` print `***` and whose only
  accessor is `expose()`. Then a key can never land in a log line by accident.
- Config writes go through a helper that creates the file `0600` and writes
  atomically (tmp + `fs::rename`); the mode is set at `OpenOptions` time, not
  chmod-after.
- **No `curl`/subprocess for anything carrying a credential** — `reqwest` puts
  the header in-process, so it never reaches an argv. This also kills SEC-04
  permanently rather than masking it (the original "fix" was literally printing
  `Bearer ***` into the command, which broke the request — see C4).
- `.active_endpoint` stores a *reference* to the credential (provider id +
  keyring/config lookup), never the key material itself.
- Optional: `keyring` crate behind a feature flag; file-with-0600 as fallback.

## Class C — Process, subprocess, and shell

### C1. Shell injection via `capture()` / `run()`
*Source: AUDIT SEC-01 (MAJOR).* Both helpers used `shell=True` with interpolated
strings — `f"vastai show instance {inst_id} --raw"` where `inst_id` came from a
file on disk. `.last_instance` containing `; rm -rf /` executes.
Related: VAST-01 (user-typed `max_price` interpolated into a shelled offer
search), and the still-deferred `EXTRA_ARGS`/`MODEL_*` interpolated into the
remote `ONSTART_CMD`.

**Rust structural fix** — `std::process::Command` takes argv as a `Vec`; there
is no shell unless you explicitly invoke `sh -c`. Ban that: a single
`exec::run(program, args: &[&OsStr])` wrapper, `#![deny]` a clippy lint or a
grep-based CI check for `"sh", "-c"`. All values that reach argv are typed
newtypes with validating constructors (`InstanceId`, `Price`, `GpuName`), so the
"where did this string come from" question is answered by the type.

### C2. `ssh_run()` used `repr()` to quote a remote command
*Source: REVIEW_FINDINGS, fixed with `shlex.quote`.* `repr()` is *Python* syntax,
not shell syntax — it mangled newlines, which broke `_restart_launch()`'s remote
heredoc, and it was an injection surface for API-supplied `ssh_host`/`ssh_port`.

**Rust structural fix** — do not build remote command strings by interpolation
at all. Either (a) use the `ssh2`/`russh` crate and send an exec request with a
properly constructed command, or (b) write the remote script to stdin of
`ssh host bash -s` so no quoting layer exists. If a string must be built, use a
dedicated `shell_quote` fn with property tests — not the language's debug
formatter.

### C3. A hung subprocess could take down the whole TUI
*Source: REVIEW_FINDINGS, fixed.* `helpers.capture()` raised `TimeoutExpired`;
a hung `vastai`/`ssh` propagated up and killed the app. Fixed by returning
rc 124.

**Rust structural fix** — every external call is `async` with an explicit
`tokio::time::timeout(dur, ..)` and returns
`Result<Output, ExecError::Timeout | ::Spawn | ::NonZero>`. There is no
timeout-less overload in the API, so "forgot the timeout" cannot compile. The
UI never blocks: commands run on a task, the TUI polls a channel.

### C4. Broken f-string in `proxy_status_detail()` — masked key sent as literal
*Source: AUDIT BUG-02 (P0) + ARCHITECTURE §9.3.* An unterminated f-string
concatenated into `Authorization: Bearer ***\n   https://api.together.ai/v1/models`.
Two failures at once: a near-syntax error, and a test that *could never pass*
because `***` was literally sent.

**Rust structural fix** — this class evaporates: string concatenation errors of
this kind are compile errors, and once credentials go through `Secret` +
`reqwest` (B3) there is no place to accidentally send the redaction token.
Corollary rule: **redaction happens at the display boundary only**, never in the
value that gets used.

### C5. `gpu_choices` referenced but never defined — Vast launch crashed
*Source: AUDIT BUG-01 (P0), fixed by the refactor.* The entire paid-launch flow
was dead on arrival with `NameError`.

**Rust structural fix** — use-before-definition is a compile error. The general
lesson for the port: the wizard's per-step state should be an explicit struct
built up step by step (`LaunchDraft { gpu: Option<GpuTierId>, .. }` →
`LaunchSpec` via a `TryFrom` that checks completeness), so "a field the flow
forgot to populate" is caught at the `TryFrom` boundary with a good message.

### C6. `menu_diagnose` crashed when there was no Vast instance
*Source: AUDIT BUG-04, fixed by the refactor.* `inst_id` could be `None` and was
handed to `get_instance_json(None)`.

**Rust structural fix** — `Option<InstanceId>` forces the match. Better: the
diagnostics module is generic over
`enum ActiveEndpoint { Local(..), Vast(..), Together(..), Vllm(..) }` and each
variant implements a `Diagnose` trait, so there is no "Vast-shaped code running
against a local endpoint" path at all.

### C7. `os.kill(pid, 0)` `PermissionError` uncaught
*Source: REVIEW_FINDINGS, deferred (reviewer N3), in `is_local_running`,
start/stop.* Cross-user PID reuse raises instead of returning a bool. A related
one **was** fixed in `tunnel_running()` (treat `PermissionError` as alive).

**Rust structural fix** — a single `ProcessHandle::is_alive() -> Liveness` where
`enum Liveness { Alive, Dead, Unknown(io::Error) }`. Callers must handle
`Unknown`; the ambiguity is in the type instead of in an exception nobody
catches. Additionally, store `{pid, start_time_ticks}` (from
`/proc/<pid>/stat` field 22) in the instance metadata so PID reuse is
*detectable* rather than merely survivable.

### C8. File descriptor leak on local server spawn
*Source: AUDIT EDGE-05 / LOCAL-01.* `open(log_file, "w")` handed to `Popen`
without a context manager.

**Rust structural fix** — `File` is RAII; `Stdio::from(file)` consumes it. The
leak is not expressible.

### C9. PID-file race, stale "running" status
*Source: AUDIT EDGE-04; REVIEW_FINDINGS deferred N13 (process killed externally
still shows "running").*

**Rust structural fix** — never trust the stored status field. `status()` is a
computed function of (pid alive ∧ start-time matches ∧ health endpoint answers),
cached with a TTL. The metadata file stores *facts* (`pid`, `started_at`,
`port`), never a derived `status: "running"` string that can go stale. Take an
exclusive `flock` on the instance metadata file for start/stop to make the
check-then-act atomic.

### C10. No port-conflict detection for local instances
*Source: AUDIT LOCAL-03.* Two recipes on the same port: the second fails
confusingly.

**Rust structural fix** — bind-probe the port (or actually reserve it) before
spawning, and return a typed `LaunchError::PortInUse { port, held_by: Option<InstanceName> }`.
Cross-check the ledger for an instance already holding that port and name it in
the error.

### C11. Backend auto-detection picks the wrong binary
*Source: AUDIT LOCAL-02.* `if target_backend is "rocm"` but no path contains
"rocm", the `or (preferred is None)` clause let *any* binary (e.g. a Vulkan one)
be selected as though it satisfied the request.

**Rust structural fix** — separate "find an exact match" from "fall back":
```
fn select_binary(bins: &[Binary], want: Backend) -> BinaryChoice
enum BinaryChoice { Exact(Binary), Fallback { got: Binary, wanted: Backend }, None }
```
The `Fallback` variant must be rendered to the user ("no ROCm build found, using
Vulkan"). A silent substitution has no representation. (Extra relevance here:
on this laptop Vulkan-vs-ROCm selection is exactly the decision that matters —
see the workspace CLAUDE.md.)

## Class D — HTTP proxy correctness

### D1. Decompressed body relayed with stale `Content-Encoding`/`Content-Length`
*Source: REVIEW_FINDINGS, P1, fixed via `_relay_headers()` on both buffered and
streaming paths.* aiohttp decompresses by default; the copied headers then lied
about the body, breaking clients.

**Rust structural fix** — either disable transparent decompression
(`reqwest::ClientBuilder::no_gzip()/no_brotli()/no_deflate()` and relay bytes
untouched, which is the correct behaviour for a transparent proxy), or
decompress and rebuild headers from the *actual* body. Encode the choice once:
a `RelayMode { Passthrough, Rewrite }` enum, and construct the response headers
from a function of the mode — never by copying the upstream map. Hop-by-hop
headers (`connection`, `keep-alive`, `transfer-encoding`, `te`, `trailer`,
`upgrade`, `proxy-*`) get stripped by the same function.

### D2. Fresh `ClientSession` per request — no keep-alive
*Source: REVIEW_FINDINGS, deferred.*

**Rust structural fix** — one `reqwest::Client` (it is an `Arc` internally) in
app state, cloned per request. Connection pooling comes free. Make the client a
field of the router state so a per-request constructor is not in scope.

### D3. Proxy has no auth
*Source: SKILL.md Pitfall 6 — documented, intentional.* `localhost:8888` is
unauthenticated by design, meant to sit behind an SSH tunnel.

**Rust design decision (redesign, don't just port)** — bind to `127.0.0.1` by
default and *refuse* to bind a non-loopback address unless an
`--allow-remote --token <t>` pair is given. Optional bearer-token check
controlled by config. Keep "no token needed on loopback" as the default so the
documented `OPENAI_API_KEY=not-needed` flow keeps working (see Part 2).

### D4. Health/status truth is scattered
`resolve_target()` in `endpoint_proxy.py` reimplements provider resolution
independently of `providers.get_active_endpoint()` — including a *second*
config parser (B2) and a different fallback rule (proxy falls back to the Vast
tunnel at `127.0.0.1:8800`; the TUI's `get_active_endpoint()` returns `None`).

**Rust structural fix** — one `EndpointResolver` in a shared crate, used by both
the daemon and the CLI. Different fallback semantics between two components that
claim to answer the same question is a bug factory.

## Class E — Config and state-file integrity

### E1. Hand-rolled TOML parser
*Source: AUDIT EDGE-01 (MAJOR).* The original `_load_toml()` silently dropped or
mangled booleans, multiline strings, inline tables, dotted keys, `"""` strings.
Fixed in the package (`config.py` now uses `tomllib`) — **but the proxy still
line-scans `config.toml` by hand** (B2), so the class isn't dead.

**Rust structural fix** — `toml` + `serde` with `#[serde(deny_unknown_fields)]`
on the recipe/tier structs. A typo'd key becomes a load-time error naming the
key, instead of a silently-ignored field that produces a mysterious runtime
default. Exactly one deserialization site in the workspace.

### E2. `load_config()` `sys.exit(1)` from inside a render f-string
*Source: REVIEW_FINDINGS, deferred (reviewer N12).* `local_menus` calls
`load_config()` inside an f-string during a menu render; delete `recipes.toml`
mid-session and the app hard-exits from a draw call.

**Rust structural fix** — config is loaded once into `Arc<Config>` held in app
state and reloaded only through an explicit `reload() -> Result<..>` (optionally
file-watched). Rendering borrows `&Config` and cannot fail. **No library code
ever exits the process** — `Result` all the way to `main`, exit codes decided in
exactly one place.

### E3. Recipe validation holes
*Source: REVIEW_FINDINGS, deferred.* `r.get("label", r["name"])` → `KeyError` if
a hand-edited recipe has neither (N5); duplicate labels silently resolve to the
first recipe (N6); the Edit path skips `validate_recipe` (N10); Ctrl-C at
"Image type:" stores `None`, then Save crashes (N8).

**Rust structural fix**
- `struct Recipe { name: RecipeName, label: Option<String>, .. }` with
  `fn display(&self) -> &str { self.label.as_deref().unwrap_or(self.name.as_str()) }`.
  `name` is non-optional in the type, so N5 cannot occur.
- Load-time validation builds a `HashMap<RecipeName, Recipe>` and **errors on
  duplicates**; menus select by `RecipeName`, never by display label. N6 dies.
- One `TryFrom<RecipeDraft> for Recipe` used by *both* create and edit paths —
  it is not possible to save an unvalidated recipe because `Recipe` is the only
  thing the writer accepts. N10 dies.
- Prompt cancellation returns `Result<T, Cancelled>` and propagates with `?`, so
  a cancelled prompt cannot deposit a `None` into a draft that later gets saved.
  N8 dies.

### E4. Non-atomic, relative-path state writes
*Source: REVIEW_FINDINGS, deferred.* `vast_down.sh` uses a **relative**
`.last_instance` path — safe only because the TUI sets cwd; run by hand from
elsewhere it silently targets the wrong (or no) file.

**Rust structural fix** — all state paths resolved once at startup into a
`Paths` struct from `$XDG_STATE_HOME`/`$XDG_CONFIG_HOME` (with the legacy
`~/.vastai-gguf` honoured for migration). No code takes a relative path. Writes
are tmp-file + `fs::rename` (atomic on the same filesystem) so a crash mid-write
never leaves a truncated ledger.

### E5. `.active_endpoint` is a single mutable file with two writers
Both the TUI (`providers.py`, `local_endpoint.py`) and the proxy's
`POST /switch` write it, with slightly different key sets (`activated_at` vs
`switched_at`; the local variant carries `pid` and sometimes an inline
`api_key`).

**Rust structural fix** — the daemon owns the state; the CLI mutates it via the
daemon's API (or via an flock'd, versioned file when the daemon is down). One
`#[derive(Serialize, Deserialize)] enum ActiveEndpoint` with a `schema_version`
field so both writers are literally the same code and old files are migratable.

## Class F — Blocking I/O, UX, and structure

### F1. Blocking network I/O on every main-menu render
*Source: AUDIT MENU-06 + REVIEW_FINDINGS deferred (reviewer M2).*
`show_status()` calls `vastai show instance` **twice** and curls endpoints on
every single main-menu draw. Crash risk is gone (C3), but it is still slow on a
bad network.

**Rust structural fix** — the render path is pure and reads a
`Arc<RwLock<StatusSnapshot>>`. A background task refreshes it on an interval and
on demand; the UI shows staleness ("as of 4s ago") and never awaits. Nothing in
the draw path may perform I/O — enforce by making the draw function take
`&StatusSnapshot` and nothing else (no client, no config path, no runtime
handle).

### F2. `console.clear()` on every loop iteration destroys scrollback
*Source: AUDIT MENU-03.*

**Rust structural fix** — use `ratatui` with an alternate screen: the TUI owns a
frame buffer and leaves the user's scrollback untouched on exit. Long output
(logs, diagnostics) goes to a scrollable pane, not to a cleared terminal.

### F3. 13 flat main-menu items, no shortcuts, inconsistent "← Back"
*Source: AUDIT MENU-01/02/04, BUG-05 (double "← Back", fixed).*

**Rust structural fix** — a declarative menu tree
(`struct MenuNode { key: char, label: &str, action: Action, children: Vec<MenuNode> }`).
Back/quit are provided by the navigation stack, never by an item a screen
appends itself — so double-back is not constructible. Hotkeys are derived from
the tree with a startup assertion that they're unique per level.

### F4. Monolith and duplication
*Source: AUDIT CQ-01/02/03, TOGETHER-01, DEAD-01..05.* 3064-line file;
295-line `menu_launch()`; the HTTP request boilerplate copy-pasted ≥6 times;
`run_together_completion()` existed but was **never called** while two other
sites inlined its logic; two `run()` definitions with unreachable dead code
between them; `format_cost_comparison()` computed then discarded in favour of a
hardcoded `"$0.00x"` (later fixed, N2).

**Rust structural fix** — workspace crates:
`apexrouter-core` (config, recipes, ledger, cost), `apexrouter-providers`
(a `Provider` trait: `list_models`, `chat_completion`, `health`, `price`),
`apexrouter-proxy`, `apexrouter-tui`, `apexrouter-cli`. Duplicate HTTP handling
disappears into one client helper. `#![deny(dead_code, unused)]` +
`cargo clippy -D warnings` in CI makes DEAD-01..05 and the unused-imports item
(N14) compile failures. `unreachable code after return` is a rustc warning by
default — promote it to an error.

### F5. Diagnostics is a 160-line linear script
*Source: AUDIT MENU-05.* Want only rate limits? You still wait through SSH
probes.

**Rust structural fix** — `enum Check` with a registry; run selected checks
concurrently (`futures::join_all`) with per-check timeouts and stream results as
they land. Each check reports `CheckResult { name, status, detail, took }`.
`apexrouter diagnose --only rate-limits` becomes trivial.

### F6. Crash-on-bad-input in prompts
*Source: REVIEW_FINDINGS, fixed (reviewer M4).* `float()` on price input crashed
on `0,50` or `abc` — **and lost unsaved edits** in the tier editor. Also
`provider_menus` failed to `mkdir` before writing the pin file (M5).

**Rust structural fix** — a validating prompt combinator
`prompt_parse::<T: FromStr>(msg, validator) -> Result<T, Cancelled>` that
re-prompts on parse failure. Editor state lives in a draft struct that is only
consumed on successful save, so a parse error cannot destroy work. All writers
call `create_dir_all(parent)` inside the same atomic-write helper (E4) — not at
each call site.

## Class G — SSH / shell hygiene (accepted Vast tradeoffs, still worth fixing)

*Source: AUDIT SEC-02; REVIEW_FINDINGS deferred.*

- `StrictHostKeyChecking no` unconditionally in `vast_tunnel.sh` and elsewhere.
- Fixed `ControlPath` **shared across instances** — two tunnels collide.
- `pgrep -n ssh` to capture the tunnel PID — a race that can capture someone
  else's ssh.
- `HF_TOKEN` in `ps` locally and in the Vast instance env.

**Rust structural fix** — manage the tunnel in-process (`russh` port forward) or,
if spawning `ssh`, then: `StrictHostKeyChecking=accept-new` with a
project-scoped `UserKnownHostsFile`; `ControlPath` templated per instance id;
**capture the PID from `Child::id()`** instead of pgrep — the race is created
entirely by not holding the handle. Write `{pid, start_time}` (C7) to the tunnel
state file. Tokens go into the instance env via the create API body (still
visible Vast-side — inherent), never via a local argv.

## Class H — Things explicitly checked and found *fine* (don't re-flag)
*Source: REVIEW_FINDINGS §"Not a problem".*
- `vast_down.sh` preserves `.last_instance` on a failed destroy (correct — don't
  "clean up" on failure; the Rust ledger should likewise only mark
  `destroyed_at` after a confirmed destroy).
- Proxy streaming consumes the body before the session closes (no truncation).
- `_clean_headers` strips `host` correctly.
- CI `build.yml` has no injectable inputs or secrets.

---

# Part 2 — The documented agent/CLI surface (compat target)

This is what SKILL.md v0.3.0 tells agents. Anything here is already "known" by
ApexOS / Claude Code / Hermes configs on this machine and must keep working (or
be superseded by a clean superset with a documented migration).

## 2.1 The load-bearing contract: the proxy

> "All providers route through a unified OpenAI-compatible proxy at `localhost:8888`."

```bash
export OPENAI_BASE_URL=http://localhost:8888/v1
export OPENAI_API_KEY=not-needed      # local proxy, no auth

hermes config set providers.localrouter.base_url http://localhost:8888/v1
hermes config set providers.localrouter.api_key not-needed
```
Health check: `curl http://localhost:8888/health`

**Actual routes** (verified in `endpoint_proxy.py::create_app`):

| Method | Path | Behaviour |
|---|---|---|
| `GET` | `/health` | `{"ok": true, "provider": "<id>", "uptime": <secs>}` |
| `GET` | `/providers` | per-provider `{available, url}` for `vast-gguf`, `together`, (+local) |
| `POST` | `/switch` | body `{"provider": "together"\|"vast-gguf"\|"local", ...}` → rewrites `.active_endpoint`; `together` accepts `api_key`/`base_url`/`model_id`, `local` requires `name`, `vast-gguf` deletes the file |
| `*` | `/{tail:.*}` | transparent forward to the active backend (so `/v1/chat/completions`, `/v1/models`, `/v1/completions`, `/v1/embeddings` all work) |

**Port requirement: keep all four, byte-compatible.** `POST /switch` is the only
*programmatic control* an agent currently has over LocalRouter — everything else
is TUI-only. Port it exactly, then extend.

Default port **8888** (proxy) and **8800** (Vast SSH tunnel local port; remote
8000 → local 8800). Local llama-server instances default to **8100**. These
numbers are baked into agent configs and shell aliases — keep them as defaults.

## 2.2 The invocation surface — and its big gap

```bash
localrouter                 # entry point
python3 -m localrouter      # equivalent
```

**Verified in `pyproject.toml` and `localrouter/__main__.py`: there are no
subcommands, no flags, no headless mode.** `[project.scripts]` maps
`localrouter → localrouter.menus.main:main`, and `main()` immediately drops into
an interactive questionary loop. Everything SKILL.md documents as a "workflow"
is an arrow-key path through a TUI:

```
localrouter → Launch → Vast GGUF → pick GPU tier → pick recipe → GEO → go
localrouter → Launch → vLLM → pick cluster tier → pick recipe → GEO → go
localrouter → Local → Launch → pick a local recipe
localrouter → Editor → Recipes / GPU Tiers / Docker Images
```
Post-launch: **Watch** (boot progress), **Tunnel → up**, **Smoke**, **Proxy**.

**This is the single biggest port opportunity.** An agent today cannot start a
model, cannot tear one down, cannot list recipes — it can only consume the proxy
and hit `/switch`. The Rust version should be **CLI-first with the TUI as one
front-end**, e.g.:

```
apexrouter serve                      # daemon: proxy + supervisor
apexrouter status [--json]
apexrouter recipes list|show|validate [--json]
apexrouter local start <recipe> | stop <name> | list | logs <name>
apexrouter vast launch <recipe> [--max-price] [--geo] [--yes] | destroy | list | watch
apexrouter tunnel up|down|status
apexrouter switch <provider> [--model]     # same semantics as POST /switch
apexrouter smoke [--provider]
apexrouter usage [--since] [--json]
apexrouter diagnose [--only <check>]
apexrouter tui                        # the interactive experience
```
Rules: every command supports `--json` with a stable schema; every
money-spending command requires `--yes` or an interactive confirm; exit codes
are meaningful. Then MCP is a thin wrapper over the same command layer (one
tool per verb), not a third implementation.

## 2.3 Providers (the four the skill promises)

| id | what | notes |
|---|---|---|
| `local` | llama.cpp on own GPU (Vulkan / ROCm / CUDA / CPU) | on this laptop: **Vulkan**, per workspace CLAUDE.md |
| `vast_gguf` | GGUF on rented Vast.ai GPUs via llama.cpp | the money path |
| `vllm` | tensor-parallel multi-GPU (DeepSeek V4 Flash 284B / Pro 1.6T) | added after the .hermes plan |
| `together` | Together AI managed, "229+ models" | |

Internal provider **string ids that appear in state files and on the wire**:
`"together"`, `"local"`, `"vast-gguf"` (hyphen! — `resolve_target`, `/switch`,
`/health` all use `vast-gguf`, while the recipe field uses `vllm` and SKILL.md
writes `vast_gguf`). **Port note: keep `vast-gguf` on the wire**, and normalise
the recipe-side spelling with a serde alias rather than "fixing" it.

## 2.4 `recipes.toml` — the data contract

Project-root file; **70 recipes, 19 GPU tiers, 4 docker images** as of v0.3.0
(28 KB). Sections: `[[recipes]]`, `[gpu_tiers.<key>]`, `[docker]`.

Recipe (llama.cpp / Vast GGUF):
```toml
[[recipes]]
name        = "qwen36-27b-q6-5090"        # unique slug (the real key)
label       = "Qwen3.6-27B  Q6_K  96K"    # display name
gpu         = "5090"                       # must match a gpu_tiers key
model_repo  = "unsloth/Qwen3.6-27B-GGUF"
model_quant = "UD-Q6_K_XL"                 # substring match against filenames
ctx         = 98304
parallel    = 1
kv_type     = "q8_0"                       # q8_0 | q4_0 | bf16
llama_cpp_repo = "fairydreaming/llama.cpp" # optional custom fork
llama_cpp_ref  = "deepseek-dsa"            # optional branch/ref
description = "..."                         # optional
```
Recipe (vLLM):
```toml
provider = "vllm"; model_id = "deepseek-ai/DeepSeek-V4-Pro"
image_type = "vllm"; kv_cache_dtype = "fp8"; reasoning_parser = "deepseek_r1"
```
GPU tier:
```toml
[gpu_tiers.h100-sxm-2x]
vast_names = ["H100_SXM", "H100_SXM5"]     # list — offer search ORs them
label      = "2× H100 SXM 160GB"
max_price  = "7.00"                         # STRING, not float
vram_gb    = 80
num_gpus   = 2
image_type = "builder"                      # prebuilt | builder | vllm
min_disk_gb = 100                           # optional
min_cuda   = "12.9"                         # optional (B200)
```

**Port requirement: read the existing `recipes.toml` unmodified.** It is 28 KB
of tuned config and the user edits it by hand. Note the traps: `max_price` is a
*string*; `model_quant` is a *substring match*, not a filename; `gpu` is a
foreign key into `gpu_tiers` that nothing currently validates (make that a
load-time check); `image_type` lives on both recipes and tiers (recipe wins).
Keep the auto-backup-on-save behaviour of the editor.

Fixed enums the wizard uses (`config.py`): `GEOS = EU_NORDIC | EU | US | ANY`;
`MODES = thinking | coding | nonthinking`; `KV_TYPES = q8_0 | q4_0 | bf16`;
`SAMPLING_PRESETS` map each mode to concrete llama-server flags (thinking: temp
1.0 / top-p 0.95 / presence 1.5; coding: 0.6 / 0.95 / 0.0; nonthinking: 0.7 /
0.80 / presence 1.5 + `--chat-template-kwargs {"enable_thinking":false}`).

## 2.5 State and data files (compat surface)

| Path | Format | Notes |
|---|---|---|
| `<root>/.active_endpoint` | JSON | `{provider, model_id?, base_url?, endpoint?, name?, host?, port?, pid?, model_path?, api_key?, activated_at\|switched_at}` — read by the proxy **and** external tools |
| `<root>/.last_instance` | text | single Vast instance id — replace with a ledger, but keep reading it for migration |
| `<root>/.instance_history` | append log | added by the 2026-07 fix |
| `<root>/.hf_pin` | text | HF model pin from the browser |
| `/tmp/vastai-gguf-tunnel.pid` | text | tunnel pid — hard-coded path, shared |
| `~/.vastai-gguf/config.toml` | TOML | `[providers.together] api_key, base_url` |
| `~/.vastai-gguf/local_instances/<name>.json` | JSON | `{name, pid, port, host, binary, model_path, backend, started_at, status}` |
| `~/.vastai-gguf/local_logs/` | text | llama-server logs |
| `~/.vastai-gguf/usage.jsonl` (`USAGE_LOG`) | JSONL | `{timestamp, provider, model_id, prompt_tokens, completion_tokens, cost_usd}` |

Documented one-liner an agent/user may already be using:
`python3 -c "from localrouter.cost import format_usage_summary; print(format_usage_summary())"`
→ the Rust equivalent must be `apexrouter usage`.

Env vars honoured: `TOGETHER_API_KEY`, `HF_TOKEN`, plus `OPENAI_BASE_URL` /
`OPENAI_API_KEY` on the client side.

## 2.6 External CLI/API dependencies

- `vastai` CLI (`pip install vastai && vastai set api-key <key>`) — used for
  `search offers --raw`, `show instance --raw`, `create`, `destroy`. **Prefer
  the REST API in Rust** (see A1); if the CLI is kept, treat its stdout as
  untrusted and never merge stderr.
- `ssh` — tunnel + remote diagnostics.
- HuggingFace API — `_hf_list_files(repo_id, token)` for the model browser.
- Together AI REST — `/models`, `/chat/completions`.
- Docker images (Vast side):
  `ghcr.io/buckster123/vastai-gguf:{prebuilt,builder,vllm}`.
- Shell scripts that are themselves a contract (agents/users invoke them
  directly): `vast_up.sh`, `vast_down.sh`, `smoke.sh`, `tools/vast_tunnel.sh`,
  and container-side `launch.sh` / `launch_vllm.sh`.

`vast_up.sh` env-var contract (defaults in parentheses) — the Rust launcher
should either keep the script and set these, or reproduce the semantics exactly:
`GPU (5090)`, `MODEL (dense)`, `KV_TYPE`, `MODE (thinking)`, `MIN_DISK_GB (60)`,
`PARALLEL`, `IMAGE_TYPE (prebuilt)`, `MIN_CUDA (12.8)`, `NUM_GPUS (1)`,
`MAX_PRICE` (per-GPU defaults: 6000pro 1.60, h100 3.50, a100 2.00, h200 5.50,
b200 9.00), `DOCKER_IMAGE`, `MODEL_REPO`, `MODEL_QUANT`, `CTX`, `VAST_NAMES`,
`HF_TOKEN`, `EXTRA_ARGS`.

## 2.7 Pitfalls the skill already documents (carry them into the Rust docs)

1. DeepSeek V4 on llama.cpp is not upstream — recipes build from
   `fairydreaming/llama.cpp @ deepseek-dsa` via `llama_cpp_repo`/`llama_cpp_ref`;
   remove when merged.
2. The vLLM image must be built and pushed to GHCR first.
3. Multi-GPU tensor parallel without NVLink is painfully slow.
4. Builder images compile at boot (~1 min on H100 per SKILL.md; `config.py`'s
   `cold_start_estimate` says ~12-18 min — **the two disagree; measure and pick
   one number**). Prebuilt is faster but SM89+SM120 only.
5. Split GGUFs: `launch.sh` uses `find -maxdepth 2` for subdirectory shards.
6. The proxy has no auth — loopback + SSH tunnel only (see D3).

---

# Part 3 — Intended direction (from `.hermes/plans/2026-04-30-manager-flexibility.md`)

The author's own plan, written *before* the vLLM provider existed. Phases 1 and
3 largely shipped (v0.3.0 has 19 tiers incl. datacenter, `vast_names` lists,
`min_cuda`, `num_gpus`, and `Dockerfile.builder` exists). Phase 4 is mostly
**not** done and is the clearest statement of where the author wanted to go.

**Framing goal:** work across the full Vast inventory — consumer (4090/5090),
datacenter (H100/H200/B200/A100), Blackwell — *without a separate codebase per
card*. The stated problem: the manager was locked to SM89+SM120 by a fat-binary
image, and the GPU filter / tiers were hardcoded.

**Phase 1 — dynamic GPU tiers + offer search** (shipped): `vast_names` as a
**list** per tier so the offer search ORs them (`gpu_name in [H100_SXM,H100_SXM5]`);
`VAST_NAMES` env var drives `vast_up.sh`'s filter with the old `case` as
fallback; `browse_offers()` forwards `vast_names` from tier config; optional
`min_cuda` per tier (B200 needs 12.9+).

**Phase 2 — two-image strategy + runtime SM compile** (shipped in part):
`prebuilt` stays for consumer cards (fast pull); `builder` ships only the CUDA
dev toolchain and `launch.sh` detects `compute_cap` via `nvidia-smi`, then
compiles llama.cpp with `-DCMAKE_CUDA_ARCHITECTURES="${SM}-real"`. A
`Dockerfile.builder-b200` on a CUDA 12.9 base was planned. Risk called out by
the author: *compile failure = dead (billing) instance* — "log everything, fail
loudly".
→ **Rust implication:** the boot watcher must be able to detect a failed remote
compile and offer to destroy the instance. That's the A1/A3 ledger plus a
`BootPhase { Pulling, Compiling, Downloading, Loading, Healthy, Failed(reason) }`
state machine — the current "Watch" screen is a log tail.

**Phase 3 — datacenter recipes** (shipped): usable-VRAM budgets the author
worked from — H100 80GB → ~73 GB usable, H200 141GB → ~135 GB, A100 80GB →
~77 GB, B200 192GB → ~185 GB.

**Phase 4 — manager UX polish (NOT done — port this as a feature, not a fix)**
1. **Show SM arch in the offer table** (`cuda_max_good` as proxy, or
   `compute_cap` if exposed); flag CUDA ≥ 13.0 hosts with a warning.
2. **Image-type + cold-start estimate in the launch summary**
   (`Image  ghcr.io/.../builder  (~8-12 min compile)`).
3. **"What fits here?" helper** — for a selected tier, a VRAM budget breakdown
   per compatible recipe (weights + KV + overhead), flagging < 2 GB headroom.
   *This is the single highest-value unbuilt feature in the plan* and it is a
   pure function — perfect for Rust: `fn vram_budget(recipe, tier) -> Budget`
   with unit tests, callable from CLI (`apexrouter fit <tier>`), TUI, and MCP.
4. **Multi-GPU in offer search** via `num_gpus` on recipes (the field now
   exists on tiers).

**Open questions the author left unanswered — inherit them as TODOs:**
1. Does `vastai search offers --raw` expose `compute_cap`? (`| jq '.[0] | keys'`)
2. B200 / SM100 needs CUDA 12.9+ *and* patched llama.cpp — verify support before
   shipping B200 recipes.
3. ccache on a mounted Vast volume to skip recompiles on a repeat host — judged
   probable over-engineering.
4. `cpu_name` filter / known-bad-CPU blocklist ("Poland was wonky partly due to
   host CPU quality"); EPYC and Xeon Scalable are fine, some older Xeons are not.
   → In Rust this is just another typed offer filter; cheap to add, and it maps
   to a `HostQuality` scoring function over the offer JSON.
5. Datacenter vs consumer tab (`hosting_type`) — different SLA/billing? unknown.

**Direction summary:** the author was steadily moving *config-out-of-code* —
tiers, images, GPU names, CUDA minimums and price caps all becoming data in
`recipes.toml` with the manager as a generic engine over that data. The Rust
port should finish that arc: a strongly-typed schema for the data, a pure
planning layer (fit/cost/filter) over it, and thin I/O adapters at the edges.

---

# Appendix — Rust design directives, condensed

| # | Directive | Kills |
|---|---|---|
| 1 | No `sh -c` anywhere; argv-only `Command` wrapper; CI grep to enforce | C1, VAST-01 |
| 2 | Vast REST via `reqwest`+`serde`, not the CLI; stdout/stderr never merged | A1 |
| 3 | Append-only instance ledger + `PendingLaunch` Drop guard | A1, A2, A3 |
| 4 | `Secret<String>` with redacting `Debug`; keys never in argv; 0600 atomic writes | B1, B3, C4 |
| 5 | Outbound headers *constructed*, never copied; one `RelayMode` | D1, B1 |
| 6 | One config/state schema + one deserializer, shared by daemon, CLI, TUI | B2, D4, E1, E5 |
| 7 | Every external call async with a mandatory timeout; UI never blocks | C3, F1 |
| 8 | Render path is a pure fn of a cached snapshot; no I/O, no `exit()` in libs | E2, F1 |
| 9 | `TryFrom<Draft>` is the only way to build a `Recipe`; dup names error at load | E3 |
| 10 | Liveness is computed (pid + start_time + health), never a stored string | C7, C9 |
| 11 | CLI-first with `--json` on everything; TUI and MCP are front-ends over it | §2.2 gap |
| 12 | Keep the proxy wire contract byte-compatible (`:8888`, 4 routes, `vast-gguf`) | agent compat |
