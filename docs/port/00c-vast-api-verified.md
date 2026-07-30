# Vast.ai REST API — VERIFIED BY LIVE CALLS, 2026-07-30

Everything here was confirmed with real HTTP requests against Andre's account from this laptop.
**Where this file disagrees with `09-external-apis.md` (desk research), this file wins.**

## Auth

| | |
|---|---|
| Base URL | `https://console.vast.ai/api/v0` |
| Header | `Authorization: Bearer <api_key>` |
| Key on disk | `~/.config/vastai/vast_api_key` — 64 chars, plain text, no trailing newline handling assumptions |

Confirmed working: `GET /users/current/` → `200`, returns a large user object.
Fields worth surfacing in the GUI: `id`, `credit` (float USD — **$7.73 at survey time**), `balance`,
`can_pay`, `has_billing`. Note the object also echoes `api_key` — **never** render or log that field.

> Credit is only ~$7.73. A 2×H100 at $3.34/hr burns it in ~2.3 hours. The GUI **must** show credit
> and an estimated burn-down before confirming a rental, and must make destroy trivially reachable.

## Offer search — `PUT /api/v0/search/asks/`

Confirmed `200`. Request body is a single `q` object; response is `{"offers": [...]}`.

```jsonc
// verified request
{
  "q": {
    "gpu_name":  { "eq": "RTX 3090" },        // exact-match operator
    "num_gpus":  { "eq": 2 },
    "rentable":  { "eq": true },
    "verified":  { "eq": true },              // accepted; not echoed back on the offer
    "type": "ask",                            // required
    "order": [["dph_total", "asc"]],          // array of [field, dir] pairs
    "limit": 3
  }
}
```

Verified operators: `eq`, `in` (array). `order` takes `asc`/`desc`. `type` must be `"ask"` for
on-demand offers (`is_bid` / `min_bid` fields exist for interruptible bidding).

### Offer object — 100 fields. The ones that matter:

| Field | Example | Use |
|---|---|---|
| `id` / `ask_contract_id` | `43731729` | **the offer id you POST to create an instance** |
| `machine_id` | `142595` | host machine |
| `gpu_name` | `"RTX 3090"` | exact string, see vocabulary below |
| `num_gpus` | `2` | |
| `gpu_ram` / `gpu_total_ram` | `24576` / `49152` (MiB) | per-GPU vs pooled VRAM — size the model against `gpu_total_ram` |
| `dph_total` / `dph_base` | `0.305` / `0.301` | $/hour total vs compute-only |
| `storage_cost`, `inet_down_cost`, `inet_up_cost` | | extra charges — `dph_total` is not the whole bill |
| `cpu_ram`, `cpu_cores_effective`, `disk_space` | `85864` MiB, `383` GB | **`disk_space` gates model download size** |
| `cuda_max_good` | `13.2` | driver's max CUDA — gate images on this |
| `driver_version` | `595.84` | |
| `geolocation` / `geolocode` | `"Czechia, CZ"` | geo filtering is a substring/enum match on this |
| `inet_down` / `inet_up` | `561.8` / `551.0` Mbps | **the dominant factor in cold-start time** (model download) |
| `reliability2` | `0.9897` | filter `>= 0.98` |
| `direct_port_count` | `199` | how many ports the host can map — needed to expose a service directly |
| `static_ip` | `true` | |
| `rented` / `rentable` | `false` / `true` | |
| `dlperf`, `dlperf_per_dphtotal`, `total_flops` | | value-for-money ranking |
| `duration`, `end_date` | | how long the host will keep the machine up |

Full field list is in the workflow transcript; treat unknown fields as opaque and keep the raw JSON
so the GUI can show an "all fields" inspector without a schema change.

## Instances — `GET /api/v0/instances/`

Confirmed `200`: `{"instances_found": 0, "instances": []}` (no active rentals right now).
Note the envelope key is `instances`, and there is a sibling count field — don't assume a bare array.

## Live market snapshot (2026-07-30) — sanity anchors for the fixed tiers Andre asked for

| Tier | Cheapest verified on-demand | Notes |
|---|---|---|
| 2× RTX 3090 | **$0.305/hr**, Czechia, cuda 13.2, 49 GB pooled, 199 ports, 562/551 Mbps | |
| 2× H100 SXM | **$3.344/hr**, Montana US, cuda 13.1, 163 GB pooled, 256 ports | |
| 2× H100 SXM | $3.469/hr, Washington US, cuda 12.4 | |
| 2× H100 PCIE | $4.269/hr, UAE | PCIE is slower than SXM for tensor-parallel |

## `gpu_name` vocabulary — exact strings, verified from live 4-GPU offers

```
RTX 3060, RTX 3070, RTX 3080, RTX 3090,
RTX 4070 Ti, RTX 4070S, RTX 4070S Ti, RTX 4080, RTX 4080S, RTX 4090,
RTX 5060 Ti, RTX 5070 Ti, RTX 5080,
RTX PRO 4000, Tesla V100,
H100 SXM, H100 NVL, H100 PCIE
```

**Do not hardcode a GPU enum.** These strings change as the market changes. The fixed tiers Andre
asked for (2/3/4× RTX 3090, up to 2× H100) should be expressed as *query templates* over
`gpu_name` + `num_gpus`, and the GUI must also allow a free-form query so new cards work on day one.
A good default: fetch the distinct `gpu_name` values from a broad live search and populate the
dropdown from that, rather than from a constant.

## Implementation guidance

- Use `reqwest` + `rustls`, `PUT` with a serde-serialised `q`. Model the query as a builder producing
  `serde_json::Value` so arbitrary operators pass through.
- Deserialize offers into a struct with the ~25 fields above **plus `#[serde(flatten)] extra:
  Map<String, Value>`** so nothing is lost and no upstream field addition breaks parsing.
- **Do not shell out to the `vastai` CLI** — it is broken on this machine
  (`ModuleNotFoundError: No module named 'vastai'`).
- Creating an instance is a billing action: it must be behind an explicit confirmation in every
  surface (GUI, CLI, MCP), show `dph_total` + estimated total + current credit, and never be a
  default or an implicit consequence of another command.
