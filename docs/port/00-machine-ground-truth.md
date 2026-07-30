# Machine ground truth — verified 2026-07-30

Facts verified **by running the commands on Andre's laptop**. Implementers: trust this file over
anything inferred from LocalRouter's Python source, which is older than the current toolchain.

## Host

| Thing | Value |
|---|---|
| CPU | Ryzen AI 5 340 (12 threads, `nproc` = 12) |
| GPU | AMD Radeon 840M Graphics (RADV KRACKAN1), Vulkan `apiVersion 1.4.335`, driver `radv` |
| GPU memory (as llama.cpp sees it) | `Vulkan0: 20992 MiB, 19518 MiB free` (shared with system RAM) |
| RAM | 22 GiB usable, 8 GiB swap — **swap was 5.5 GiB used at survey time**, memory is tight |
| Rust | `rustc 1.97.0` / `cargo 1.97.0` |
| Second Vulkan device | `llvmpipe` (software) — must be excluded when picking a device |

ROCm build exists (`~/llama.cpp/build-rocm`) but the 840M is not a supported ROCm target. **Vulkan
is the backend of choice.** Do not default to ROCm.

## llama.cpp

Builds present (all have a `llama-server` binary):

```
~/llama.cpp/build/bin/llama-server
~/llama.cpp/build-vulkan/bin/llama-server     <- the one to use
~/llama.cpp/build-rocm/bin/llama-server
~/llama.cpp/build-mtp/bin/llama-server
~/llama.cpp/build-zaya1/bin/llama-server
```

Version: **`b9199 (39cf5d619)`, built with GNU 15.2.0**. This is much newer than what LocalRouter
was written against. Verified flag details that differ from naive assumptions:

| Flag | Reality in b9199 |
|---|---|
| `-fa, --flash-attn` | takes `[on\|off\|auto]`, **default `auto`** — no longer a bare boolean |
| `--jinja / --no-jinja` | **default is enabled** — do not pass `--jinja` blindly, it is already on |
| `--webui / --no-webui` | DEPRECATED, superseded by `--ui/--no-ui` |
| `-ctk/-ctv` | allowed: `f32, f16, bf16, q8_0, q4_0, q4_1, iq4_nl, q5_0, q5_1` (default `f16`) |
| `-ngl` | accepts an exact number **or** a non-numeric form; `-1`/auto semantics apply |
| `-np, --parallel` | default `-1` (auto), not 1 |
| `-c, --ctx-size` | default `0` = take from model |
| `-dev, --device` | comma-separated device list, e.g. `Vulkan0` |
| `-sm, --split-mode` | `{none,layer,row,tensor}` |
| `--slots` | **enabled by default** in this build |
| `--props` | disabled by default (POST /props needs it) |
| `--metrics` | disabled by default (Prometheus endpoint needs it) |
| `-a, --alias` | comma-separated model aliases surfaced through the API |
| `--api-key` / `--api-key-file` | supported |
| reasoning | `--reasoning-format`, `-rea/--reasoning [on\|off\|auto]`, `--reasoning-budget N` |

`llama-server --help` is 635 lines; probe it at runtime rather than hardcoding a flag list if you
need something not in this table. **Feature-detect** by grepping `--help` output before emitting a
flag that may not exist in the user's build.

## Models

```
/home/andre/models/carnice-9b/Carnice-9b-Q6_K.gguf     6.9 GB   <- the only complete GGUF
/home/andre/models/qwen36-35b-a3b/                     .cache only — incomplete download
/home/andre/models/results/                            benchmark output, not models
```

**The mk1 end-to-end smoke test must use `Carnice-9b-Q6_K.gguf` on `build-vulkan`.** It is the only
real model on the box. Discovery must therefore:

- recurse into subdirectories of `~/models` (models are in per-model folders, not flat)
- ignore `.cache` directories and non-GGUF files
- tolerate a directory that contains no usable model

Note LocalRouter's saved instance references `~/models/Qwen3.5-9B-Q4_K_M.gguf`, which **no longer
exists** — stale state files are the normal case, not an edge case. Validate paths on load.

## Existing LocalRouter state on disk (must be readable / migratable)

```
~/.vastai-gguf/
├── config.toml                       [providers.together] base_url + api_key   (REAL KEY PRESENT)
├── .pinned_provider                  {"provider","model_id","base_url"}
├── usage.log                         JSONL: timestamp, epoch, provider, model_id,
│                                     prompt_tokens, completion_tokens, cost_usd
├── local_instances/<name>.json       {name,pid,port,host,binary,model_path,backend,
│                                      started_at,status,stopped_at}
└── local_logs/
```

Plus, badly, `.active_endpoint` / `.last_instance` / `.hf_pin` written **into the LocalRouter repo
directory itself** — a design flaw. ApexRouter keeps all state under one XDG-ish state dir.

## Vast.ai

> **The `vastai` Python CLI on this machine is BROKEN.**
> ```
> $ vastai --help
> ModuleNotFoundError: No module named 'vastai'
> ```
> The launcher script at `~/.local/bin/vastai` survives but its package is gone.

Consequence for the design: **ApexRouter-RS must speak the Vast.ai REST API directly over HTTPS**
(reqwest + rustls) and must not shell out to the `vastai` CLI. Shelling out was LocalRouter's
approach and it is already broken on the only machine that runs it. If a CLI fallback is kept at
all, it is strictly optional and must never be on the primary path.

SSH is still needed for the tunnel; `ssh` is present and working: `OpenSSH_10.2p1`.

## Credentials present on the box (all three integrations are live-testable)

| Credential | Location | Status |
|---|---|---|
| Vast.ai API key | `~/.config/vastai/vast_api_key` (64 bytes, plain) | present — read this path, it is the `vastai` CLI's own convention |
| HuggingFace token | `~/.cache/huggingface/token` (37 bytes, plain) | present |
| Together AI key | `$TOGETHER_API_KEY` **and** `~/.vastai-gguf/config.toml` | present in both |

Credential resolution order should be: explicit config value → ApexRouter config file → the
conventional third-party path above → environment variable. Never log or echo these; never write
them into the new config file if they were sourced from elsewhere.
