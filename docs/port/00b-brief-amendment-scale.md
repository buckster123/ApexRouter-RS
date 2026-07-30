# Brief amendment — target hardware is NOT this laptop

**Issued by Andre, 2026-07-30, mid-design.** This **overrides** any statement in the design brief or
in the proposals that says ApexRouter should assume a small, memory-constrained machine.

> "use the local compute on this laptop only as example, assume bigger rigs will run this in the future."

## What this changes

The laptop (Radeon 840M, 24 GB shared, Vulkan-only, ~4 tok/s on 27B) is the **development and smoke-test
box**, not the target. It is a *worked example*, useful because it is the machine in front of us and
the one the mk1 must demonstrably run on. It is **not** the design centre.

Design for the general case and let the laptop be the degenerate one:

| Do NOT assume | DO support |
|---|---|
| one GPU | **N GPUs**, heterogeneous, per-device selection, `-sm/--split-mode` + `-mg/--main-gpu`, `-dev` device lists, tensor-split ratios |
| Vulkan only | **Vulkan, CUDA, ROCm/HIP, Metal, CPU** as first-class backends; per-build capability detection rather than a hardcoded backend enum with one real member |
| a single llama.cpp build | **several builds** side by side (`~/llama.cpp/build-*`), each with different backend support; the user picks, or ApexRouter matches build→backend automatically |
| one local endpoint at a time | **multiple concurrent local endpoints**, each with its own port, model, device set and slot count — routed between by the proxy |
| small models | 100B+ locally, multi-hundred-GB VRAM pools, big context windows, `--parallel` slot counts in the tens |
| llama.cpp only, locally | vLLM / other OpenAI-compatible servers **run locally too**, not just on rented boxes |
| memory pressure everywhere | plenty of RAM/VRAM on the target rig — but still *measure* and *display* headroom rather than assuming either way |
| a single machine | local-network **remote nodes** are a natural extension (an OpenAI-compatible endpoint on another box on the LAN is just another backend). Do not architect this out even if mk1 does not ship a discovery mechanism for it. |

## Concrete consequences for the architecture

1. **Hardware detection must enumerate, not singularise.** A `Gpu` list with index, name, backend,
   total/free VRAM, driver — not a `backend: Backend` scalar. Query per-build (`llama-server
   --list-devices`) instead of assuming.
2. **Resource planning is a real feature.** "Will this model fit?" must be computed from *the rig's*
   VRAM across devices, not a constant. Offer layer-offload (`-ngl`) suggestions from measured free
   VRAM per device.
3. **Concurrency limits are configuration, not constants.** No hardcoded "one model at a time",
   no hardcoded thread counts, no assumption that starting a second endpoint will OOM.
4. **The GUI stays light** — that part of the original brief still holds, because a light GUI is good
   engineering, not because the machine is weak. But it must render a **many-endpoint, many-GPU**
   view without falling apart: lists that scale, no fixed-slot layouts, no "the endpoint" singular
   in the data model or the UI copy.
5. **Cost/telemetry** must aggregate across many simultaneous backends.
6. Nothing in the code should hardcode `840M`, `Vulkan`, `~4 tok/s`, `24 GB`, or a single model path.
   Those belong only in test fixtures and in `00-machine-ground-truth.md`.

## What stays true from the laptop ground truth

`00-machine-ground-truth.md` remains the authority for **the mk1 smoke test** — that test runs
`Carnice-9b-Q6_K.gguf` on `~/llama.cpp/build-vulkan/bin/llama-server`, because that is what exists
here. Keep the smoke test; drop the assumption.
