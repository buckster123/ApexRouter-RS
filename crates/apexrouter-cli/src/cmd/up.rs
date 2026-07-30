//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter up <model|recipe> [--alias A] [--yes]` — the one-command happy path. Resolves its positional in a **documented** order: exact recipe id -> exact model id -> unique case-insensitive prefix -> path on disk; an ambiguous prefix errors with the candidates listed.
