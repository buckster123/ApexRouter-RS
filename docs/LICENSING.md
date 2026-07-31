# Licensing

ApexRouter-RS is split so that the **headless node is permissively licensed** and the optional
native GUI — the only GPL-encumbered piece — is never linked into it. This is charter decision
**D12** and it is enforced by the workspace layout, not by discipline.

## Headless stack (default)

Dual-licensed **MIT OR Apache-2.0**, at your option:

| Crate | Binary / output |
|---|---|
| `apexrouter-protocol` | library (the wire types) |
| `apexrouter-core` | library |
| `apexrouter-router` | library |
| `apexrouter-providers` | library |
| `apexrouter-client` | library (the SDK) |
| `apexrouter-server` | library (`pub fn api_router` — ApexOS can mount it) |
| `apexrouter-cli` | `apexrouter` — the CLI, the daemon (`serve`) and the MCP stdio server |
| `ui-web/{index.html,app.js,style.css}` | embedded into `apexrouter-server` by `rust-embed` |

These are the workspace's `default-members`. `cargo build`, `cargo test`, `cargo clippy` and
`cargo build --release` at the workspace root touch **only** this set, so the ordinary build never
compiles or links anything GPL.

The embedded web UI is first-party source with **no vendored third-party JavaScript, no CDN and no
build step** — three plain files. There is nothing in it whose licence you have to trace.

## Native Slint app

`crates/apexrouter-slint` (binary `apexrouter-ui`) is **GPL-3.0-only**, taken deliberately under
Slint's GPL licensing option, and is `publish = false`.

- It is a member of the workspace but **not** of `default-members`, so it is only ever built when
  you name it (`cargo build -p apexrouter-slint`). CI never links it — clippy runs on the seven
  headless crates by name, and `cargo test --workspace` is run with
  `--exclude apexrouter-slint` there.
- It is an **edge client**: it depends only on `apexrouter-protocol` and `apexrouter-client` and
  talks the same HTTP/WebSocket API as everything else. No GPL code flows the other way — nothing
  in the headless crates depends on it.
- **Distributing the native GUI binary requires GPL-3.0 compliance.** Distributing the headless node
  (the `apexrouter` binary, or the libraries) does not, because that crate is not linked into it.

If a commercial Slint licence is ever acquired, the licence field in
`crates/apexrouter-slint/Cargo.toml` is the single line to change; nothing else in the workspace
assumes GPL.

## Third-party services and binaries

ApexRouter-RS is an independent client. It bundles none of these and is not affiliated with any of
them; your use is subject to each provider's own terms and your own account and API keys.

| Dependency | Relationship |
|---|---|
| **vast.ai** | REST client (`providers/src/vast/`). Rentals bill your account. |
| **together.ai** | REST client (`providers/src/together.rs`). Metered. |
| **Hugging Face** | REST client (`providers/src/hf.rs`) for search, `paths-info` sizing and downloads. **Model weights carry their own licences** — a GGUF you download through `apexrouter hf get` is governed by its repository's licence and any gated-repo agreement you accepted, not by this project's. |
| **llama.cpp** (`llama-server`) | **Spawned as a separate process**, never linked. ApexRouter builds an argv and supervises the child; no llama.cpp code is compiled into or linked against any crate here, so its licence does not propagate. The same is true of `vllm` and of `ssh`. |
| **Anthropic / OpenAI API shapes** | ApexRouter implements the wire *formats* (`/v1/chat/completions`, `/v1/messages`). It contains no code from either vendor. |

Rust dependency licences are the usual permissive set (MIT/Apache-2.0/BSD/Unicode-3.0); run
`cargo deny check licenses` or `cargo about` if you need the audited manifest — neither is wired
into CI today.

## Licence texts

The workspace declares `license = "MIT OR Apache-2.0"` in `[workspace.package]` and
`license = "GPL-3.0-only"` in `crates/apexrouter-slint/Cargo.toml`. The corresponding
`LICENSE-MIT`, `LICENSE-APACHE` and `LICENSE-GPL` files at the repository root are **not yet
present** — no mk1 work unit owns them (`BUILD-PLAN.md` §5). Add them before the first public
release; until then the SPDX expressions above are the authoritative statement of intent.
