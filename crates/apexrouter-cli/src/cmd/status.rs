//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter status [--json] [--watch]`. Class `ReadState`: with no daemon it serves from `$STATE` under `LOCK_SH` and tags the output `served_by: "offline"`.
