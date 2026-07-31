//! OWNER: unit M-01 (cli/src/mcp/**, cli/src/cmd/mcp.rs). Do not edit outside that unit.
//!
//! The tool definitions and their JSON schemas. All names are prefixed `apexrouter_`,
//! because three MCP servers share `~/Projects/.mcp.json`.
//!
//! Descriptions are **long and operational**: an agent should get from `apexrouter_status`
//! to a working `OPENAI_BASE_URL` without reading a doc.
//!
//! The money tool is deliberately shaped as a refusal that doubles as a dry run:
//! `apexrouter_vast_rent` without `confirm` and `max_usd_per_hour` returns `isError: true`
//! **carrying the full cost preview and the current credit**, and creates nothing.
//!
//! Two properties the 2026-07-28 revision asks for and this module supplies for free:
//! tools are returned in a **deterministic order** (sorted by name, asserted by a test), and
//! every name is inside the 1–128 character `[A-Za-z0-9_.-]` charset.

use serde_json::{json, Value};

/// The prefix every tool carries, so three MCP servers can share one `.mcp.json`.
pub const PREFIX: &str = "apexrouter_";

/// An object schema. `additionalProperties: false` everywhere — a typo in an argument name
/// should be a validation failure at the client, not a silently ignored field here.
fn obj(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

/// The schema for a tool that takes nothing at all.
fn nothing() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

/// A `{"type": "string"}` leaf with its description.
fn str_of(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

/// An `{"type": "integer"}` leaf with its description.
fn int_of(desc: &str) -> Value {
    json!({ "type": "integer", "description": desc })
}

/// A `{"type": "number"}` leaf with its description.
fn num_of(desc: &str) -> Value {
    json!({ "type": "number", "description": desc })
}

/// A `{"type": "boolean"}` leaf with its description.
fn bool_of(desc: &str) -> Value {
    json!({ "type": "boolean", "description": desc })
}

/// A `{"type": "array", "items": {"type": "string"}}` leaf with its description.
fn strs_of(desc: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": desc })
}

/// One tool definition, in the shape both eras of `tools/list` accept.
///
/// `title` is a 2026-07-28 addition that legacy clients ignore, so it costs nothing to
/// carry and gives a modern client something better than a snake_case name to render.
fn tool(name: &str, title: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": schema,
    })
}

/// Every tool, **sorted by name**, which the 2026-07-28 revision asks for so a client can
/// cache the list and an LLM can hit its prompt cache.
///
/// The order is asserted by a test rather than trusted to whoever edits this vector next.
pub fn definitions() -> Vec<Value> {
    let mut all = vec![
        tool(
            "apexrouter_status",
            "Router status",
            "The 'what is my inference situation' call, and the one to make FIRST in a \
             session. Returns the OpenAI-compatible base URL to put in OPENAI_BASE_URL, the \
             model string to put in \"model\", every alias and where it currently points, \
             each backend's health and throughput, the rig summary, in-flight requests, \
             24 h spend and remaining vast.ai credit. Works with the daemon down, in which \
             case `served_by` is \"offline\" and `stale` is true: the routing table, the \
             endpoint records and the aliases are still facts, but health, tok/s, free VRAM \
             and spend are left at zero rather than invented.",
            nothing(),
        ),
        tool(
            "apexrouter_models",
            "Model catalogue",
            "The aggregated model list: for every routable backend, the exact string to put \
             in \"model\", the alias that reaches it, advertised context, vision and \
             tool-calling support, price per Mtok and the live median tok/s. **Make this \
             call before choosing a `model` string** — sending an id no backend advertises \
             is the single most common way a request 404s. With the daemon down it falls \
             back to the local GGUF inventory, which tells you what COULD be started but \
             not what is running.",
            nothing(),
        ),
        tool(
            "apexrouter_rig",
            "Hardware inventory",
            "GPUs with free and total VRAM and which endpoints hold them, the discovered \
             llama.cpp builds with their compute backends and `-dev` device tokens, plus \
             host RAM, swap and CPU thread count. Note two things before reasoning about \
             the numbers: one physical card enumerated by two builds appears as two rows \
             (`ROCm0` and `Vulkan0` can be the same silicon, and the VRAM budget is computed \
             per backend, never summed across them), and free VRAM may legitimately exceed \
             total on an APU because of GTT accounting — never compute `total - free`.",
            nothing(),
        ),
        tool(
            "apexrouter_fit",
            "Fit solver",
            "Will this model fit, at what context and how many parallel slots, and what is \
             the arithmetic? Pure, instant and side-effect free — no process is started and \
             nothing is written. Returns a verdict, the chosen ctx/parallel/KV type, the \
             layer-offload plan and a `why[]` carrying every term of the sum, so a refusal \
             can be argued with. Works with the daemon down: the VRAM budget is measured \
             live from the rig minus what running endpoints already reserve. Call this \
             before `apexrouter_up` when the model is large or the box is busy.",
            obj(
                json!({
                    "model": str_of(
                        "Model id, name, unique case-insensitive prefix, or a path to a \
                         .gguf on disk."),
                    "ctx": int_of(
                        "Total context pool to solve for. Omit to let the solver search for \
                         the largest that fits."),
                    "parallel": int_of("Slots sharing that pool. Omit for the solver's choice."),
                    "kv": str_of(
                        "KV cache element type: f32, f16, bf16, q8_0, q5_1, q5_0, q4_1, \
                         q4_0 or iq4_nl. Omit to let the solver choose."),
                    "devices": strs_of(
                        "`-dev` tokens such as [\"Vulkan0\"]. They must all belong to ONE \
                         compute backend — a process uses one build. Omit for the build the \
                         launcher would pick."),
                }),
                &["model"],
            ),
        ),
        tool(
            "apexrouter_up",
            "Start and bind in one call",
            "The one-call happy path: pick a llama.cpp build, solve the fit, spawn \
             `llama-server`, wait on the health gate, bind an alias, and return the base URL \
             and the model string to use. This is what you want 90% of the time; \
             `apexrouter_endpoint_start` is the same thing with every knob exposed. Needs a \
             running daemon, because it supervises a child process that must outlive this \
             call — with none, it returns an error telling you to run `apexrouter serve \
             --detach`. If it fails, `apexrouter_logs` on the returned id is the next call.",
            obj(
                json!({
                    "model": str_of(
                        "Model id, name, unique prefix, or a path to a .gguf on disk."),
                    "alias": str_of(
                        "Bind this alias once the endpoint is Ready, so clients can keep \
                         sending a stable `model` string."),
                    "ctx": int_of("Total context pool. Omit to let the fit solver size it."),
                    "parallel": int_of("Slots sharing that pool."),
                    "devices": strs_of("`-dev` tokens, all from one compute backend."),
                    "wait": bool_of(
                        "Default true: block until the endpoint answers its health gate. \
                         false returns a JobRecord immediately and you poll \
                         `apexrouter_status`."),
                }),
                &["model"],
            ),
        ),
        tool(
            "apexrouter_endpoint_start",
            "Start an endpoint (full control)",
            "Full control when `apexrouter_up` is too opinionated: you supply the whole \
             `EndpointSpec` — the tagged union with `kind` of local_llama, local_vllm, vast, \
             node or managed — and it is posted verbatim. Use `apexrouter_up` unless you \
             need a specific build, port, sampling preset, tensor split or mmproj. Needs a \
             running daemon.",
            obj(
                json!({
                    "spec": json!({
                        "type": "object",
                        "description":
                            "An EndpointSpec. `kind` selects the variant; for `local_llama` \
                             the fields are build, model_path, alias_flag, host, port, ctx, \
                             parallel, kv_type, ngl, split, mode.",
                    }),
                    "alias": str_of("Bind this alias once it is Ready."),
                    "no_wait": bool_of(
                        "Return a JobRecord immediately instead of waiting for the health \
                         gate."),
                    "force": bool_of(
                        "Start even when the fit solver refuses on VRAM. Skips that one \
                         admission check and nothing else."),
                }),
                &["spec"],
            ),
        ),
        tool(
            "apexrouter_endpoint_stop",
            "Stop an endpoint",
            "Stop a running endpoint. The record is kept, so it can be restarted; the CLI's \
             `endpoint rm` is what forgets it entirely. Identify it by `id`, or by `alias` \
             and the endpoint currently bound to that alias is the one that stops. Needs a \
             running daemon.",
            obj(
                json!({
                    "id": str_of("Endpoint id, as `apexrouter_status` lists it."),
                    "alias": str_of(
                        "Alternative to `id`: stop whichever endpoint this alias is bound to."),
                    "mode": json!({
                        "type": "string",
                        "enum": ["drain", "now"],
                        "description":
                            "drain (default) lets in-flight requests finish; now signals \
                             immediately.",
                    }),
                }),
                &[],
            ),
        ),
        tool(
            "apexrouter_swap",
            "Swap the model behind an alias",
            "Atomically re-point a stable alias at a different model or backend: one call \
             instead of start, wait, re-route, stop. Clients keep sending the same `model` \
             string across the swap. `to` is either a backend id that already exists or an \
             EndpointSpec to bring up first. Needs a running daemon.",
            obj(
                json!({
                    "alias": str_of("The alias whose target changes. Clients never see it move."),
                    "to": json!({
                        "description":
                            "A backend id (string) that already exists, or an EndpointSpec \
                             object to start and swap onto.",
                    }),
                    "mode": str_of(
                        "How to overlap old and new. Omit to let the daemon choose based on \
                         whether both fit in VRAM at once."),
                }),
                &["alias", "to"],
            ),
        ),
        tool(
            "apexrouter_logs",
            "Tail a log",
            "The tail of an endpoint's or rented instance's log. **This is the call to make \
             when a start failed** — the reason is almost always in the last 50 lines, and \
             guessing at it from the error string wastes a turn. Works with the daemon down \
             for local endpoints, because the log file is on disk either way.",
            obj(
                json!({
                    "id": str_of("Endpoint id or vast.ai instance id."),
                    "tail": int_of("How many trailing lines. Default 200."),
                }),
                &["id"],
            ),
        ),
        tool(
            "apexrouter_backend_set",
            "Quarantine or re-tag a backend",
            "Quarantine a degraded backend without touching any route: disable it and every \
             alias that selects it simply stops choosing it, failing over to the next target \
             in the chain. Also the way to re-tag a backend so tag selectors pick it up. \
             Needs a running daemon.",
            obj(
                json!({
                    "id": str_of("Backend id."),
                    "enabled": bool_of(
                        "false takes it out of the routing table immediately; true puts it \
                         back."),
                    "drain": bool_of(
                        "true stops sending it new requests but lets in-flight ones finish."),
                    "tags": strs_of(
                        "Replace the tag set, e.g. [\"local\", \"tools\", \"vision\"]. Omit \
                         to leave tags alone."),
                }),
                &["id"],
            ),
        ),
        tool(
            "apexrouter_route_set",
            "Point an alias",
            "Create or re-point an alias and its ordered failover chain. Effective on the \
             next request — nothing restarts and no in-flight request is disturbed. Target \
             syntax is `<backend-id>[:<upstream-model>]`, `tag:<tag>[:<model>]` or \
             `glob:<pattern>[:<model>]`, and order is the chain order. Needs a running \
             daemon.",
            obj(
                json!({
                    "alias": str_of("The string clients put in \"model\"."),
                    "targets": strs_of(
                        "Ordered targets: `local-carnice`, `tag:rented`, \
                         `glob:vast-*:my-model`. First entry is tried first."),
                    "strategy": json!({
                        "type": "string",
                        "enum": ["first_healthy", "round_robin", "least_busy", "cheapest"],
                        "description": "How to pick among healthy targets. Default first_healthy.",
                    }),
                    "failover": bool_of(
                        "May a retry go to a DIFFERENT backend? There is never a retry after \
                         the first upstream byte either way."),
                    "default": bool_of(
                        "Make this the default alias, which is what an absent or legacy \
                         `model` field resolves to."),
                }),
                &["alias", "targets"],
            ),
        ),
        tool(
            "apexrouter_recipe_list",
            "List saved launch plans",
            "Every saved recipe — a named, re-runnable launch plan with its model, sizing \
             and device selection already decided — and every saved vast.ai search profile. \
             Read from disk, so it works with the daemon down. Pair with \
             `apexrouter_recipe_run` to launch one.",
            nothing(),
        ),
        tool(
            "apexrouter_recipe_save",
            "Save a launch plan",
            "Create or replace a recipe: a named launch plan you or the human can re-run \
             without re-deciding ctx, parallel, KV type and devices. This is the agent half \
             of building recipes in the GUI — what you save here appears there. Needs a \
             running daemon so the recipe is validated against the live rig before it is \
             stored.",
            obj(
                json!({
                    "recipe": json!({
                        "type": "object",
                        "description":
                            "A Recipe object. An `id` that already exists replaces it; a new \
                             or absent one creates.",
                    }),
                }),
                &["recipe"],
            ),
        ),
        tool(
            "apexrouter_recipe_run",
            "Launch a saved plan",
            "Instantiate a saved recipe into a running endpoint, optionally binding an alias \
             to it. Equivalent to `apexrouter_up` with every decision already made. Needs a \
             running daemon.",
            obj(
                json!({
                    "id": str_of("Recipe id, from `apexrouter_recipe_list`."),
                    "alias": str_of("Bind this alias once the endpoint is Ready."),
                    "no_wait": bool_of(
                        "Return a JobRecord instead of waiting for the health gate."),
                }),
                &["id"],
            ),
        ),
        tool(
            "apexrouter_usage",
            "Tokens, cost and throughput",
            "Tokens, cost and tok/s over a window, grouped how you ask. Metered and \
             estimated figures are marked as such and never silently blended: one estimated \
             row demotes the total to an estimate rather than the total quietly claiming to \
             be metered. Reads the append-only usage log, so it works with the daemon down.",
            obj(
                json!({
                    "since": str_of(
                        "`all`, a duration like `30m`, `24h`, `7d`, `4w`, or an absolute \
                         timestamp. Default 24h."),
                    "by": json!({
                        "type": "string",
                        "enum": ["provider", "model", "backend", "alias", "day"],
                        "description": "How to bucket the rows. Default provider.",
                    }),
                }),
                &[],
            ),
        ),
        tool(
            "apexrouter_smoke",
            "Is this endpoint actually working?",
            "Four named probes against one alias or base URL — the models list, a short \
             warm-up, a tool-calling probe with `tool_choice: auto`, and a throughput run — \
             each pass/fail with TTFT and tok/s READ from the upstream's own `timings` \
             object rather than stopwatched. The call to make before you commit a long agent \
             run to an endpoint you have not used yet. Needs a running daemon.",
            obj(
                json!({
                    "alias": str_of(
                        "Alias to probe. Resolved exactly as a real request would be."),
                    "base_url": str_of(
                        "Probe a raw OpenAI-compatible URL instead, bypassing the routing \
                         table."),
                }),
                &[],
            ),
        ),
        tool(
            "apexrouter_diagnose",
            "Run the check registry",
            "Run the diagnostic check registry — builds, devices, model roots, credentials, \
             listeners, tunnels, rented boxes — and return each check's verdict together \
             with its remedy. Pass `only` to run a single check by id instead of the whole \
             registry. Needs a running daemon.",
            obj(
                json!({
                    "only": str_of("Run just this check id instead of the whole registry."),
                }),
                &[],
            ),
        ),
        tool(
            "apexrouter_hf_search",
            "Search HuggingFace",
            "Search HuggingFace for GGUF repositories. Read-only and free. Returns repo ids, \
             downloads and likes; follow with `apexrouter_hf_files` to see the actual \
             quantisations and their authoritative sizes. Needs a running daemon, which \
             holds the HuggingFace credential.",
            obj(
                json!({
                    "q": str_of("Search terms, e.g. \"qwen3 gguf\"."),
                    "limit": int_of("Row cap. Default 20."),
                }),
                &["q"],
            ),
        ),
        tool(
            "apexrouter_hf_files",
            "List a repo's quantisations",
            "The files in one HuggingFace repo, grouped by quantisation, with sizes from \
             `paths-info` — authoritative byte counts, not the listing's frequently-absent \
             ones. Multi-part shards are grouped and summed, so the number you see is the \
             number that must fit on disk and in VRAM. Needs a running daemon.",
            obj(
                json!({ "repo": str_of("Repo id, e.g. \"unsloth/Qwen3-8B-GGUF\".") }),
                &["repo"],
            ),
        ),
        tool(
            "apexrouter_hf_get",
            "Download weights",
            "Download a quantisation into the local model root, resuming a partial file and \
             verifying the size on completion. This closes the discovery-to-launch dead-end: \
             a HuggingFace row can become a local endpoint without leaving the session. \
             Costs bandwidth and disk, not money. Needs a running daemon.",
            obj(
                json!({
                    "repo": str_of("Repo id."),
                    "quant": str_of(
                        "Quantisation to fetch, e.g. \"Q4_K_M\" or \"UD-Q4_K_XL\". Omit only \
                         if `files` names exact paths."),
                    "files": strs_of("Exact file paths in the repo, instead of `quant`."),
                    "no_wait": bool_of(
                        "Return a JobRecord immediately; poll `apexrouter_status` for \
                         progress. Recommended — a 20 GB download outlives any sane tool \
                         timeout."),
                }),
                &["repo"],
            ),
        ),
        tool(
            "apexrouter_vast_offers",
            "Search the vast.ai market",
            "Live, read-only vast.ai market search. **Free and safe: this creates nothing \
             and spends nothing.** Returns offers with $/hr, GPU model and count, \
             reliability, network speed and disk, plus any relaxations the search had to \
             apply to return rows at all — a widened query you did not notice is how you \
             rent the wrong box. Needs a running daemon, which holds the vast credential.",
            obj(
                json!({
                    "profile": str_of(
                        "A saved search profile id, from `apexrouter_recipe_list`."),
                    "gpu": str_of("Exact `gpu_name`, e.g. \"RTX_4090\" or \"H100_SXM\"."),
                    "num_gpus": int_of("Minimum GPU count."),
                    "geo": str_of("Region filter, e.g. \"EU\" or \"US\"."),
                    "max_price": num_of("Ceiling on $/hr (`dph_total`)."),
                    "limit": int_of("Row cap. Default 20."),
                }),
                &[],
            ),
        ),
        tool(
            "apexrouter_vast_rent",
            "Rent a box (SPENDS MONEY)",
            "**This tool spends real money.** Without BOTH `confirm: true` and a positive \
             `max_usd_per_hour` it creates nothing and instead returns an error carrying the \
             full cost preview — $/hr, projected 1 h and 24 h totals, the daemon's hard \
             ceiling and the remaining account credit — so the refusal doubles as a dry run \
             that shows you the bill. Ask the human before sending `confirm: true`; a 2xH100 \
             burns a small balance in a couple of hours. The rent itself is still subject to \
             the daemon-side ceiling and, when configured, to a human approval that must be \
             granted out of band.",
            obj(
                json!({
                    "profile": str_of(
                        "Saved search profile to rent the cheapest match from. Mutually \
                         exclusive with `offer_id`."),
                    "offer_id": int_of(
                        "A specific offer id from `apexrouter_vast_offers`. Prefer this: a \
                         profile can widen between the search and the rent."),
                    "launch": json!({
                        "type": "object",
                        "description":
                            "The ContainerLaunch contract: image, model, runtime, ports and \
                             env. HF_TOKEN goes in the env map, never in an onstart string.",
                    }),
                    "confirm": bool_of(
                        "Must be exactly true to create anything. Absent or false returns \
                         the cost preview and creates nothing."),
                    "max_usd_per_hour": num_of(
                        "Your ceiling in $/hr. Required alongside `confirm`. The daemon's \
                         own ceiling still applies and is the lower of the two."),
                    "auto_tunnel": bool_of(
                        "Bring up an `ssh -L` tunnel as soon as the box is reachable. The \
                         default posture is tunnel-only, and it is the right one."),
                    "bind_alias": str_of("Bind this alias to it once it is healthy."),
                }),
                &["launch"],
            ),
        ),
        tool(
            "apexrouter_vast_destroy",
            "Destroy a rented box",
            "Tear down a rented vast.ai instance, verify it is gone before forgetting it, \
             and return the accrued cost. Requires `confirm: true`; without it you get the \
             instance's current state and accrued cost and nothing is destroyed. Destroying \
             stops the meter — an instance nobody is using is still billing.",
            obj(
                json!({
                    "id": str_of("Instance id, as `apexrouter_status` lists it."),
                    "confirm": bool_of(
                        "Must be exactly true. Absent or false destroys nothing."),
                }),
                &["id"],
            ),
        ),
        tool(
            "apexrouter_compare",
            "Race one prompt across aliases",
            "Run one prompt across N aliases **in parallel** and report, per alias, latency, \
             TTFT, tok/s, real prompt and completion token counts as the upstream reported \
             them, estimated cost, and the first 200 characters of each answer. The call to \
             make when the human asks 'which of these should I use'. Needs a running daemon.",
            obj(
                json!({
                    "aliases": strs_of("Two or more aliases to race."),
                    "prompt": str_of("The prompt every alias receives, verbatim."),
                    "max_tokens": int_of("Completion cap per alias. Default 200."),
                }),
                &["aliases", "prompt"],
            ),
        ),
    ];
    all.sort_by(|a, b| name_of(a).cmp(name_of(b)));
    all
}

/// The `name` field of a definition, or `""` for a value that is not one.
fn name_of(v: &Value) -> &str {
    v.get("name").and_then(Value::as_str).unwrap_or("")
}

/// Every tool name, in the same deterministic order as [`definitions`].
pub fn names() -> Vec<String> {
    definitions()
        .iter()
        .map(|t| name_of(t).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_architecture_tool_is_present_and_prefixed() {
        // ARCHITECTURE.md §8, the table, in the order it is written there.
        let expected = [
            "apexrouter_status",
            "apexrouter_models",
            "apexrouter_rig",
            "apexrouter_fit",
            "apexrouter_up",
            "apexrouter_endpoint_start",
            "apexrouter_endpoint_stop",
            "apexrouter_swap",
            "apexrouter_logs",
            "apexrouter_backend_set",
            "apexrouter_route_set",
            "apexrouter_recipe_list",
            "apexrouter_recipe_save",
            "apexrouter_recipe_run",
            "apexrouter_usage",
            "apexrouter_smoke",
            "apexrouter_diagnose",
            "apexrouter_hf_search",
            "apexrouter_hf_files",
            "apexrouter_hf_get",
            "apexrouter_vast_offers",
            "apexrouter_vast_rent",
            "apexrouter_vast_destroy",
            "apexrouter_compare",
        ];
        let have = names();
        for want in expected {
            assert!(have.iter().any(|n| n == want), "§8 tool {want} is missing");
        }
        assert_eq!(have.len(), expected.len(), "an undocumented tool crept in");
        for n in &have {
            assert!(n.starts_with(PREFIX), "{n} is not prefixed `{PREFIX}`");
        }
    }

    #[test]
    fn names_obey_the_2026_charset_and_length_rule() {
        for n in names() {
            assert!(
                (1..=128).contains(&n.len()),
                "{n} is outside the 1..=128 range"
            );
            assert!(
                n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'),
                "{n} uses a character outside [A-Za-z0-9_.-]"
            );
        }
    }

    #[test]
    fn the_list_is_sorted_so_a_client_can_cache_it() {
        let have = names();
        let mut sorted = have.clone();
        sorted.sort();
        assert_eq!(have, sorted, "tools/list must be in a deterministic order");
    }

    #[test]
    fn every_description_is_long_and_operational() {
        for t in definitions() {
            let name = name_of(&t).to_string();
            let d = t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                d.len() > 160,
                "{name}'s description is {} chars — §8 asks for long and operational",
                d.len()
            );
        }
    }

    #[test]
    fn every_schema_is_a_closed_object() {
        for t in definitions() {
            let name = name_of(&t).to_string();
            let s = t.get("inputSchema").cloned().unwrap_or_default();
            assert_eq!(
                s.get("type").and_then(Value::as_str),
                Some("object"),
                "{name}"
            );
            assert_eq!(
                s.get("additionalProperties").and_then(Value::as_bool),
                Some(false),
                "{name} must reject unknown arguments rather than ignore them"
            );
        }
    }

    #[test]
    fn the_money_tool_documents_the_refusal_and_names_both_gates() {
        let rent = definitions()
            .into_iter()
            .find(|t| name_of(t) == "apexrouter_vast_rent")
            .expect("the rent tool exists");
        let d = rent
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        assert!(d.contains("spends real money"), "{d}");
        assert!(d.contains("confirm"), "{d}");
        assert!(d.contains("max_usd_per_hour"), "{d}");
        assert!(d.contains("creates nothing"), "{d}");

        // `confirm` is deliberately NOT required: the refusal-with-a-preview IS the
        // documented dry run, and a schema that forces `confirm` makes it unreachable.
        let required = rent
            .pointer("/inputSchema/required")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(
            !required.iter().any(|v| v.as_str() == Some("confirm")),
            "confirm must be optional so the dry-run refusal is reachable"
        );
    }
}
