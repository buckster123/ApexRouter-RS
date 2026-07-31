//! OWNER: unit S-06 (cli/src/{main,cli,daemon,render}.rs, cli/src/cmd/{mod,status,serve,config,rig,models,fit,endpoint,route,switch,url,version,completions}.rs). Do not edit outside that unit.
//!
//! `apexrouter endpoint ls | show | logs | argv | start | stop | restart | adopt | rm`, plus
//! `endpoint vllm start`.
//!
//! `ls`, `show`, `logs` and `argv` answer with no daemon: an endpoint record is a fact on
//! disk, a log is a file, and the argv is a pure function of the record plus the build.
//! Everything that *changes* a child process goes through the daemon, because the daemon is
//! the only thing that may signal one.
//!
//! `endpoint start` is the whole MK1-CORE spine in one verb: resolve the model, choose the
//! build, hand the daemon a spec, and print the URL the agent should use.

use crate::cli::{split_list, EndpointCmd, EndpointStartArgs, VllmCmd};
use crate::cmd::{models, url, Ctx};
use crate::daemon::{Need, Serving};
use crate::render;
use apexrouter_client::NodeClient;
use apexrouter_core::argv;
use apexrouter_protocol::{
    ArgvPreview, BackendId, BuildId, EndpointRecord, EndpointSpec, GpuBackend, JobRecord,
    LlamaBuild, LocalLlamaSpec, LocalVllmSpec, NglPlan, RigSnapshot, SamplingMode, ServedBy,
    SplitMode, SplitPlan,
};
use std::time::Duration;

/// Run `apexrouter endpoint …`.
///
/// # Errors
/// A `$STATE` read failure, a daemon that will not answer, or an unresolvable argument.
pub async fn run(ctx: &Ctx, cmd: &EndpointCmd) -> anyhow::Result<()> {
    match cmd {
        EndpointCmd::Ls(args) => {
            let serving = ctx.serving(Need::ReadState).await?;
            let eps = load(ctx, &serving).await?;
            if args.json {
                return render::print_json(
                    serving.served_by(),
                    render::now_unix(),
                    serving.is_offline(),
                    &eps,
                );
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            render::print_table(
                &[
                    "ENDPOINT", "STATE", "MODEL", "PORT", "PID", "ADOPTED", "ALIASES", "UPTIME",
                ],
                eps.iter().map(row).collect(),
            );
            Ok(())
        }
        EndpointCmd::Show { id, json } => {
            let serving = ctx.serving(Need::ReadState).await?;
            let eps = load(ctx, &serving).await?;
            let e = find(&eps, id)?;
            if *json {
                return render::print_json(
                    serving.served_by(),
                    render::now_unix(),
                    serving.is_offline(),
                    &e,
                );
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            print_endpoint(ctx, &e);
            Ok(())
        }
        EndpointCmd::Logs { id, lines, follow } => {
            let serving = ctx.serving(Need::ReadState).await?;
            let eps = load(ctx, &serving).await?;
            let e = find(&eps, id)?;
            logs(ctx, &e, *lines, *follow).await
        }
        EndpointCmd::Argv { id, json } => {
            let serving = ctx.serving(Need::ReadState).await?;
            let preview = match &serving {
                Serving::Daemon(c) => {
                    c.get::<ArgvPreview>(&format!("/v1/endpoints/{id}/argv"))
                        .await?
                }
                _ => {
                    let eps = load(ctx, &serving).await?;
                    offline_argv(ctx, &find(&eps, id)?).await?
                }
            };
            if *json {
                return render::print_json(
                    serving.served_by(),
                    render::now_unix(),
                    serving.is_offline(),
                    &preview,
                );
            }
            if serving.is_offline() {
                render::print_offline_notice();
            }
            print_argv(&preview);
            Ok(())
        }
        EndpointCmd::Start(args) => start(ctx, args).await,
        EndpointCmd::Stop { id, all } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let ids = match (id, all) {
                (Some(id), _) => vec![id.clone()],
                (None, true) => client
                    .get::<Vec<EndpointRecord>>("/v1/endpoints")
                    .await?
                    .iter()
                    .map(|e| e.id.as_str().to_string())
                    .collect(),
                (None, false) => {
                    anyhow::bail!("name an endpoint, or pass --all")
                }
            };
            for id in ids {
                let _: serde_json::Value = client
                    .post(&format!("/v1/endpoints/{id}/stop"), &serde_json::json!({}))
                    .await?;
                render::print_line(&format!("stopped {id}"));
            }
            Ok(())
        }
        EndpointCmd::Restart {
            id,
            ctx: c,
            parallel,
        } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let rec = restart(&client, id, *c, *parallel).await?;
            render::print_line(&format!(
                "restarted {} on port {}{}",
                rec.id.as_str(),
                render::dash(rec.port),
                match rec.fit.as_ref() {
                    Some(f) => format!(" with ctx {} across {} slot(s)", f.ctx, f.parallel),
                    None => String::new(),
                }
            ));
            Ok(())
        }
        EndpointCmd::Adopt { id } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let rec: EndpointRecord = client
                .post(&format!("/v1/endpoints/{id}/adopt"), &serde_json::json!({}))
                .await?;
            render::print_line(&format!(
                "adopted {} (pid {})",
                rec.id.as_str(),
                render::dash(rec.proc.as_ref().map(|p| p.pid))
            ));
            Ok(())
        }
        EndpointCmd::Rm { id } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            client.delete(&format!("/v1/endpoints/{id}")).await?;
            render::print_line(&format!("removed {id}"));
            Ok(())
        }
        EndpointCmd::Vllm { cmd } => vllm(ctx, cmd).await,
    }
}

/// Every endpoint record: from the daemon when there is one, from `$STATE` when there is not.
///
/// # Errors
/// A `$STATE` read failure, or a daemon that will not answer.
pub async fn load(ctx: &Ctx, serving: &Serving) -> anyhow::Result<Vec<EndpointRecord>> {
    match serving {
        Serving::Daemon(c) => Ok(c.get::<Vec<EndpointRecord>>("/v1/endpoints").await?),
        Serving::Offline(store) => Ok(store.with_state_lock_shared(|| store.list_endpoints())?),
        Serving::None(_) => {
            Ok(apexrouter_core::store::Store::new(ctx.paths.clone()).list_endpoints()?)
        }
    }
}

/// One endpoint by id, or a message naming what there is.
///
/// # Errors
/// When no record has that id.
fn find(eps: &[EndpointRecord], id: &str) -> anyhow::Result<EndpointRecord> {
    eps.iter()
        .find(|e| e.id.as_str() == id)
        .cloned()
        .ok_or_else(|| {
            let known: Vec<&str> = eps.iter().map(|e| e.id.as_str()).collect();
            anyhow::anyhow!(
                "no endpoint `{id}`{}",
                if known.is_empty() {
                    " — there are none".to_string()
                } else {
                    format!(" — there is: {}", known.join(", "))
                }
            )
        })
}

/// `apexrouter endpoint restart [--ctx N] [--parallel N]`.
///
/// # Why this is not one POST
///
/// `POST /v1/endpoints/{id}/restart` replays **the record's own spec** — that is its
/// contract, and it is the right one for a plain restart. It reads no body, so
/// `--ctx`/`--parallel` used to be composed into a JSON object, sent, discarded by the
/// daemon, and then reported as success: the CLI printed `restarted … on port 8100` while
/// the new child's argv still said `-c 8192`. A no-op flag that reports success is worse
/// than an error, because the operator believes it and moves on.
///
/// New sizing is therefore a **new spec**, and a new spec is `DELETE` then `POST` — the
/// `endpoint rm` + `up --ctx` dance an operator had to know, done properly:
///
/// * the delete comes first because `allocate_id` hands a spec that differs from a record
///   already on disk a fresh id (`…-2`), and a restart that renames the endpoint is not a
///   restart;
/// * the alias bindings are read off the old record and re-applied to the new one, for the
///   same reason the daemon's own restart re-applies them: an unbound `auto` looks exactly
///   like a restart that worked, until the next request 404s;
/// * the failure message says the record is gone, because after the delete it is, and the
///   operator needs to know that before they retry.
///
/// # Errors
/// A daemon that will not answer, an endpoint that is not locally supervised, or a spec the
/// supervisor refuses — including a context that no longer fits in VRAM, which is the
/// commonest way this fails and is the honest answer rather than a silent downgrade.
pub async fn restart(
    client: &NodeClient,
    id: &str,
    ctx: Option<u32>,
    parallel: Option<u32>,
) -> anyhow::Result<EndpointRecord> {
    if ctx.is_none() && parallel.is_none() {
        return Ok(client
            .post(
                &format!("/v1/endpoints/{id}/restart"),
                &serde_json::json!({}),
            )
            .await?);
    }

    // The record is the only copy of the spec and of the alias bindings, so both are read
    // before anything is destroyed.
    let previous: EndpointRecord = client.get(&format!("/v1/endpoints/{id}")).await?;
    let spec = resized(&previous.spec, ctx, parallel, id)?;
    let aliases: Vec<String> = previous
        .alias_bindings
        .iter()
        .map(|a| a.as_str().to_owned())
        .collect();

    client.delete(&format!("/v1/endpoints/{id}")).await?;

    let path = match aliases.first() {
        Some(a) => format!("/v1/endpoints?alias={a}"),
        None => "/v1/endpoints".to_string(),
    };
    let rec: EndpointRecord = client.post(&path, &spec).await.map_err(|e| {
        anyhow::anyhow!(
            "{id} was stopped and its record removed, but it would not start again with the \
             new sizing: {e}\nnothing is running under that name now — `apexrouter up <model>` \
             starts it again with whatever fits"
        )
    })?;

    // One alias rides in on the query string; the daemon binds exactly one there. Anything
    // else is named rather than silently dropped.
    for extra in aliases.iter().skip(1) {
        render::print_line(&format!(
            "note: `{extra}` was bound to {id} and is not re-bound — \
             `apexrouter route set {extra} --target {}`",
            rec.id.as_str()
        ));
    }
    Ok(rec)
}

/// The same endpoint, resized. Pure: this is the whole of what `--ctx`/`--parallel` mean.
///
/// `None` leaves a dimension alone, so `--parallel 4` on its own keeps the context pool the
/// endpoint already had.
///
/// # Errors
/// A spec with no local sizing to change. A remote endpoint's context is set where it runs,
/// and pretending otherwise here would be the same defect one level along.
fn resized(
    spec: &EndpointSpec,
    ctx: Option<u32>,
    parallel: Option<u32>,
    id: &str,
) -> anyhow::Result<EndpointSpec> {
    match spec {
        EndpointSpec::LocalLlama(s) => {
            let mut s = s.clone();
            s.ctx = ctx.or(s.ctx);
            s.parallel = parallel.or(s.parallel);
            Ok(EndpointSpec::LocalLlama(s))
        }
        EndpointSpec::LocalVllm(s) => {
            let mut s = s.clone();
            s.ctx = ctx.or(s.ctx);
            // vLLM's `--max-num-seqs` is llama.cpp's `-np`: how many sequences share the
            // pool. There is no second meaning of `--parallel` here to confuse it with.
            s.max_num_seqs = parallel.or(s.max_num_seqs);
            Ok(EndpointSpec::LocalVllm(s))
        }
        EndpointSpec::Node(_) | EndpointSpec::Managed(_) | EndpointSpec::Vast(_) => {
            anyhow::bail!(
                "`{id}` is not a locally supervised endpoint, so its context pool is not ours \
                 to set — restart it without --ctx/--parallel, or change it where it runs"
            )
        }
    }
}

/// `apexrouter endpoint start` — the one-command happy path behind the MK1-CORE transcript.
///
/// # Errors
/// An unresolvable model, no usable build, or a daemon that refuses the spec.
async fn start(ctx: &Ctx, args: &EndpointStartArgs) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    let rig: RigSnapshot = client.get("/v1/rig").await?;
    let all: Vec<apexrouter_protocol::LocalModel> = client.get("/v1/models/local").await?;
    let model = models::resolve_model(&all, &args.model)?;

    let devices = args.devices.as_deref().map(split_list).unwrap_or_default();
    let build = choose_build(&rig, args.build.as_deref(), &devices)?;

    let model_path = model
        .primary_path()
        .ok_or_else(|| anyhow::anyhow!("{} has no shard to load", model.name))?
        .to_string();
    let spec = EndpointSpec::LocalLlama(LocalLlamaSpec {
        build: build.id.clone(),
        model_path,
        mmproj: args
            .mmproj
            .clone()
            .or_else(|| model.mmproj.first().map(|s| s.path.clone())),
        // What the endpoint advertises as its model id, and therefore what
        // `/v1/models` shows as `<backend>/<model>`.
        alias_flag: model.name.clone(),
        host: "127.0.0.1".to_string(),
        port: args.port,
        ctx: args.ctx,
        parallel: args.parallel,
        kv_type: args.kv.map(Into::into),
        ngl: parse_ngl(args.ngl.as_deref())?,
        split: SplitPlan {
            devices,
            mode: args.split_mode.map(Into::into).unwrap_or(SplitMode::Layer),
            main_gpu: args.main_gpu,
            tensor_split: args
                .tensor_split
                .as_deref()
                .map(|v| {
                    split_list(v)
                        .iter()
                        .map(|s| s.parse::<f32>().map_err(anyhow::Error::from))
                        .collect::<anyhow::Result<Vec<f32>>>()
                })
                .transpose()?,
        },
        mode: args
            .mode
            .map(Into::into)
            .unwrap_or_else(|| sampling_mode(&ctx.cfg.endpoints.default_mode)),
        flash_attn: None,
        api_key: None,
        extra_args: Vec::new(),
    });

    let mut query: Vec<String> = Vec::new();
    if let Some(a) = &args.alias {
        query.push(format!("alias={a}"));
    }
    if args.no_wait {
        query.push("no_wait=true".to_string());
    }
    if args.force {
        query.push("force=true".to_string());
    }
    let path = if query.is_empty() {
        "/v1/endpoints".to_string()
    } else {
        format!("/v1/endpoints?{}", query.join("&"))
    };

    let raw: serde_json::Value = client.post(&path, &spec).await?;
    if args.json {
        return render::print_json(ServedBy::Daemon, render::now_unix(), false, &raw);
    }

    // `?no_wait` answers with a JobRecord; everything else answers with the record itself.
    if let Ok(rec) = serde_json::from_value::<EndpointRecord>(raw.clone()) {
        print_endpoint(ctx, &rec);
        let (base, _) = url::proxy_base(ctx)?;
        render::print_line(&format!("{base}/v1"));
        return Ok(());
    }
    let job: JobRecord = serde_json::from_value(raw)?;
    render::print_line(&format!(
        "job {} ({:?}) — `apexrouter endpoint ls` when it finishes",
        job.id, job.state
    ));
    Ok(())
}

/// `apexrouter endpoint vllm start`.
///
/// # Errors
/// A daemon that will not answer, or a spec it refuses.
async fn vllm(ctx: &Ctx, cmd: &VllmCmd) -> anyhow::Result<()> {
    let VllmCmd::Start {
        model_id,
        tp,
        ctx: c,
        port,
        alias,
        no_wait,
        json,
    } = cmd;
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    let spec = EndpointSpec::LocalVllm(LocalVllmSpec {
        bin: "vllm".to_string(),
        model_id: model_id.clone(),
        tp: *tp,
        ctx: *c,
        quantization: None,
        kv_cache_dtype: None,
        enforce_eager: false,
        reasoning_parser: None,
        gpu_util: None,
        max_num_seqs: None,
        trust_remote: false,
        chunked_prefill: true,
        host: "127.0.0.1".to_string(),
        port: *port,
        devices: Vec::new(),
        extra_args: Vec::new(),
    });
    let mut query: Vec<String> = Vec::new();
    if let Some(a) = alias {
        query.push(format!("alias={a}"));
    }
    if *no_wait {
        query.push("no_wait=true".to_string());
    }
    let path = if query.is_empty() {
        "/v1/endpoints".to_string()
    } else {
        format!("/v1/endpoints?{}", query.join("&"))
    };
    let raw: serde_json::Value = client.post(&path, &spec).await?;
    if *json {
        return render::print_json(ServedBy::Daemon, render::now_unix(), false, &raw);
    }
    render::print_line(&raw.to_string());
    Ok(())
}

/// Tail an endpoint's log, from the file the record names.
///
/// # Errors
/// An unreadable log file.
async fn logs(ctx: &Ctx, e: &EndpointRecord, lines: usize, follow: bool) -> anyhow::Result<()> {
    let path = e
        .log_path
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ctx.paths.log_file(&e.id));
    for line in apexrouter_providers::local::supervisor::log_tail(&path, lines) {
        render::print_line(&line);
    }
    if !follow {
        return Ok(());
    }
    // Poll rather than `notify`: a log the daemon is appending to changes constantly, and a
    // watcher here would be a second mechanism for a job a 250 ms read already does.
    let mut at = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let len = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        // Rotation (copytruncate) makes the file shorter: start again from its beginning.
        if len < at {
            at = 0;
        }
        if len == at {
            continue;
        }
        let fresh = read_from(&path, at)?;
        at = len;
        for line in fresh.lines() {
            render::print_line(line);
        }
    }
}

/// Read a file from `offset` to its end.
///
/// # Errors
/// An unreadable file.
fn read_from(path: &std::path::Path, offset: u64) -> anyhow::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = String::new();
    f.take(4 * 1024 * 1024).read_to_string(&mut buf)?;
    Ok(buf)
}

/// The argv an endpoint would be launched with, computed here rather than asked for.
///
/// # Errors
/// When the record's build is no longer on the machine.
async fn offline_argv(ctx: &Ctx, e: &EndpointRecord) -> anyhow::Result<ArgvPreview> {
    match &e.spec {
        EndpointSpec::LocalLlama(spec) => {
            let rig = apexrouter_providers::local::supervisor::scan_rig(
                &ctx.cfg.endpoints,
                ctx.paths.cache(),
            )
            .await?;
            let build = rig
                .builds
                .iter()
                .find(|b| b.id == spec.build)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "build `{}` is no longer on this machine; `apexrouter rig` lists what is",
                        spec.build.as_str()
                    )
                })?;
            Ok(argv::plan_local(spec, build, None)?)
        }
        EndpointSpec::LocalVllm(spec) => Ok(argv::plan_local_vllm(spec)?),
        _ => anyhow::bail!(
            "`{}` is not a locally supervised endpoint, so it has no local argv",
            e.id.as_str()
        ),
    }
}

/// Pick the build: the named one, else the one that can see the requested devices, else
/// whatever `choose_build` prefers. A fallback is **printed**, never silent.
///
/// # Errors
/// When no build exists at all, or the named one does not.
fn choose_build(
    rig: &RigSnapshot,
    named: Option<&str>,
    devices: &[String],
) -> anyhow::Result<LlamaBuild> {
    if let Some(name) = named {
        return rig
            .builds
            .iter()
            .find(|b| b.id.as_str() == name || b.label == name)
            .cloned()
            .ok_or_else(|| {
                let known: Vec<&str> = rig.builds.iter().map(|b| b.id.as_str()).collect();
                anyhow::anyhow!("no build `{name}` — discovery found: {}", known.join(", "))
            });
    }
    let want = devices.first().and_then(|d| backend_of(d));
    let choice =
        apexrouter_core::discover::choose_build(&rig.builds, want.clone()).ok_or_else(|| {
            anyhow::anyhow!(
                "no llama.cpp build was discovered — `apexrouter rig` shows where it looked, \
                 and [endpoints] build_roots is what it searched"
            )
        })?;
    if !choice.exact {
        render::print_line(&format!(
            "note: no {} build; using {} ({})",
            want.as_ref()
                .map(render::variant)
                .unwrap_or_else(|| "matching".to_string()),
            choice.chosen.as_str(),
            choice
                .got
                .as_ref()
                .map(render::variant)
                .unwrap_or_else(|| "unknown backend".to_string())
        ));
    }
    rig.builds
        .iter()
        .find(|b| b.id == choice.chosen)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("chosen build `{}` vanished", choice.chosen.as_str()))
}

/// The compute backend a `-dev` token belongs to, by its documented prefix.
fn backend_of(device: &str) -> Option<GpuBackend> {
    let d = device.to_lowercase();
    if d.starts_with("vulkan") {
        Some(GpuBackend::Vulkan)
    } else if d.starts_with("cuda") {
        Some(GpuBackend::Cuda)
    } else if d.starts_with("rocm") {
        Some(GpuBackend::Rocm)
    } else if d.starts_with("hip") {
        Some(GpuBackend::Hip)
    } else if d.starts_with("sycl") {
        Some(GpuBackend::Sycl)
    } else if d.starts_with("metal") {
        Some(GpuBackend::Metal)
    } else if d.starts_with("cpu") {
        Some(GpuBackend::Cpu)
    } else {
        None
    }
}

/// `--ngl auto|all|N`.
///
/// # Errors
/// Anything else.
fn parse_ngl(v: Option<&str>) -> anyhow::Result<NglPlan> {
    match v {
        None | Some("auto") => Ok(NglPlan::Auto),
        Some("all") => Ok(NglPlan::All),
        Some(n) => n
            .parse::<u32>()
            .map(NglPlan::Layers)
            .map_err(|_| anyhow::anyhow!("--ngl takes `auto`, `all` or a layer count, not `{n}`")),
    }
}

/// `[endpoints] default_mode` as the enum. An unknown value is `Thinking`, which is the
/// documented default rather than a panic.
fn sampling_mode(s: &str) -> SamplingMode {
    match s.to_lowercase().as_str() {
        "coding" => SamplingMode::Coding,
        "nonthinking" => SamplingMode::Nonthinking,
        "raw" => SamplingMode::Raw,
        _ => SamplingMode::Thinking,
    }
}

/// One row of the endpoint table.
fn row(e: &EndpointRecord) -> Vec<String> {
    vec![
        e.id.as_str().to_string(),
        render::variant(&e.desired),
        model_label(e),
        render::dash(e.port),
        render::dash(e.proc.as_ref().map(|p| p.pid)),
        if e.adopted {
            "yes".to_string()
        } else {
            String::new()
        },
        e.alias_bindings
            .iter()
            .map(|a| a.as_str())
            .collect::<Vec<_>>()
            .join(","),
        if e.started_at_unix > 0 {
            render::human_secs(render::now_unix() - e.started_at_unix)
        } else {
            String::new()
        },
    ]
}

/// What an endpoint serves, in one cell.
fn model_label(e: &EndpointRecord) -> String {
    match &e.spec {
        EndpointSpec::LocalLlama(s) => s
            .model_path
            .rsplit('/')
            .next()
            .unwrap_or(&s.model_path)
            .to_string(),
        EndpointSpec::LocalVllm(s) => s.model_id.clone(),
        EndpointSpec::Node(s) => s.base_url.clone(),
        EndpointSpec::Managed(s) => s.model_id.clone().unwrap_or_else(|| s.base_url.clone()),
        EndpointSpec::Vast(s) => format!("vast:{}", s.instance_id.0),
    }
}

/// The detail view.
fn print_endpoint(ctx: &Ctx, e: &EndpointRecord) {
    render::print_line(&format!(
        "{}  {}  {}",
        e.id.as_str(),
        model_label(e),
        render::variant(&e.desired)
    ));
    render::print_line(&format!("  port     {}", render::dash(e.port)));
    if let Some(p) = &e.proc {
        render::print_line(&format!("  pid      {} ({})", p.pid, p.exe));
    }
    render::print_line(&format!(
        "  adopted  {}",
        if e.adopted { "yes" } else { "no" }
    ));
    if !e.alias_bindings.is_empty() {
        render::print_line(&format!(
            "  aliases  {}",
            e.alias_bindings
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(f) = &e.fit {
        render::print_line(&format!(
            "  fit      ctx {} · {} slot(s) · kv {} · weights {} · kv {} · compute {}",
            f.ctx,
            f.parallel,
            f.kv_type.as_flag(),
            render::human_mib(f.weights_mb),
            render::human_mib(f.kv_mb),
            render::human_mib(f.compute_mb)
        ));
    }
    let log = e
        .log_path
        .clone()
        .unwrap_or_else(|| ctx.paths.log_file(&e.id).display().to_string());
    render::print_line(&format!("  log      {log}"));
}

/// The argv preview: one token per line is unreadable, one line is unreadable too, so the
/// program goes on its own line and the args are grouped in pairs.
fn print_argv(p: &ArgvPreview) {
    render::print_line(&p.program);
    let mut i = 0;
    while i < p.args.len() {
        let flag = &p.args[i];
        let value = p.args.get(i + 1).filter(|v| !v.starts_with('-'));
        match value {
            Some(v) => {
                render::print_line(&format!("  {flag} {v}"));
                i += 2;
            }
            None => {
                render::print_line(&format!("  {flag}"));
                i += 1;
            }
        }
    }
    for (k, v) in &p.env {
        render::print_line(&format!("  env  {k}={v}"));
    }
    for w in &p.warnings {
        render::print_line(&format!("  warn {w}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{BuildId, FlagSupport, LlamaBuild};

    fn build(id: &str, backends: Vec<GpuBackend>) -> LlamaBuild {
        LlamaBuild {
            id: BuildId::parse(id).expect("id"),
            server_path: format!("/home/andre/llama.cpp/{id}/bin/llama-server"),
            label: id.to_string(),
            build_info: Some("b9199".to_string()),
            backends,
            devices: vec!["Vulkan0".to_string()],
            flags: FlagSupport::default(),
            probed_at_unix: 0,
        }
    }

    fn rig_with(builds: Vec<LlamaBuild>) -> RigSnapshot {
        RigSnapshot {
            builds,
            ..RigSnapshot::default()
        }
    }

    #[test]
    fn ngl_accepts_the_three_documented_forms() {
        assert_eq!(parse_ngl(None).expect("auto"), NglPlan::Auto);
        assert_eq!(parse_ngl(Some("auto")).expect("auto"), NglPlan::Auto);
        assert_eq!(parse_ngl(Some("all")).expect("all"), NglPlan::All);
        assert_eq!(parse_ngl(Some("32")).expect("n"), NglPlan::Layers(32));
        assert!(parse_ngl(Some("most")).is_err());
    }

    #[test]
    fn a_device_token_names_its_backend() {
        assert_eq!(backend_of("Vulkan0"), Some(GpuBackend::Vulkan));
        assert_eq!(backend_of("ROCm1"), Some(GpuBackend::Rocm));
        assert_eq!(backend_of("CUDA0"), Some(GpuBackend::Cuda));
        assert_eq!(backend_of("whatever"), None);
    }

    #[test]
    fn a_named_build_that_does_not_exist_lists_the_ones_that_do() {
        let rig = rig_with(vec![build("build-vulkan", vec![GpuBackend::Vulkan])]);
        let e = choose_build(&rig, Some("build-cuda"), &[]).expect_err("must fail");
        assert!(e.to_string().contains("build-vulkan"), "{e}");
    }

    #[test]
    fn no_builds_at_all_says_where_discovery_looked() {
        let e = choose_build(&rig_with(Vec::new()), None, &[]).expect_err("must fail");
        assert!(e.to_string().contains("build_roots"), "{e}");
    }

    #[test]
    fn a_device_selects_the_build_that_can_see_it() {
        let rig = rig_with(vec![
            build("build-cpu", vec![GpuBackend::Cpu]),
            build("build-vulkan", vec![GpuBackend::Vulkan]),
        ]);
        let chosen =
            choose_build(&rig, None, &["Vulkan0".to_string()]).expect("a vulkan build exists");
        assert_eq!(chosen.id.as_str(), "build-vulkan");
    }

    #[test]
    fn the_default_sampling_mode_survives_a_typo_in_config() {
        assert_eq!(sampling_mode("coding"), SamplingMode::Coding);
        assert_eq!(sampling_mode("THINKING"), SamplingMode::Thinking);
        assert_eq!(sampling_mode("nonsense"), SamplingMode::Thinking);
    }

    fn llama_spec() -> EndpointSpec {
        EndpointSpec::LocalLlama(LocalLlamaSpec {
            build: BuildId::parse("build-vulkan").expect("id"),
            model_path: "/models/Carnice-9b-Q6_K.gguf".to_string(),
            mmproj: None,
            alias_flag: "Carnice-9b-Q6_K".to_string(),
            host: "127.0.0.1".to_string(),
            port: Some(8100),
            ctx: Some(8192),
            parallel: Some(2),
            kv_type: None,
            ngl: NglPlan::All,
            split: SplitPlan {
                devices: vec!["Vulkan0".to_string()],
                mode: SplitMode::Layer,
                main_gpu: None,
                tensor_split: None,
            },
            mode: SamplingMode::Coding,
            flash_attn: None,
            api_key: None,
            extra_args: Vec::new(),
        })
    }

    /// The sizing of a spec, whichever kind it is.
    fn sizing(spec: &EndpointSpec) -> (Option<u32>, Option<u32>) {
        match spec {
            EndpointSpec::LocalLlama(s) => (s.ctx, s.parallel),
            EndpointSpec::LocalVllm(s) => (s.ctx, s.max_num_seqs),
            _ => unreachable!("only the local kinds are resized"),
        }
    }

    #[test]
    fn resizing_changes_only_the_dimension_that_was_named() {
        let before = llama_spec();

        let both = resized(&before, Some(4096), Some(4), "e").expect("resized");
        assert_eq!(sizing(&both), (Some(4096), Some(4)));

        // `--ctx` alone keeps the slot count, and vice versa.
        assert_eq!(
            sizing(&resized(&before, Some(4096), None, "e").expect("ctx only")),
            (Some(4096), Some(2))
        );
        assert_eq!(
            sizing(&resized(&before, None, Some(8), "e").expect("parallel only")),
            (Some(8192), Some(8))
        );

        // Everything else about the endpoint is untouched — the port most of all, because a
        // restart that moves the port breaks every agent config pointed at it.
        let EndpointSpec::LocalLlama(after) = both else {
            panic!("the kind must not change");
        };
        let EndpointSpec::LocalLlama(before) = before else {
            unreachable!("built as LocalLlama")
        };
        assert_eq!(after.port, before.port);
        assert_eq!(after.model_path, before.model_path);
        assert_eq!(after.build, before.build);
        assert_eq!(after.alias_flag, before.alias_flag);
        assert_eq!(after.split.devices, before.split.devices);
    }

    #[test]
    fn a_vllm_endpoint_resizes_its_sequence_limit() {
        let spec = EndpointSpec::LocalVllm(LocalVllmSpec {
            bin: "vllm".to_string(),
            model_id: "Qwen/Qwen3-8B".to_string(),
            tp: None,
            ctx: Some(8192),
            quantization: None,
            kv_cache_dtype: None,
            enforce_eager: false,
            reasoning_parser: None,
            gpu_util: None,
            max_num_seqs: None,
            trust_remote: false,
            chunked_prefill: true,
            host: "127.0.0.1".to_string(),
            port: None,
            devices: Vec::new(),
            extra_args: Vec::new(),
        });
        let after = resized(&spec, Some(16384), Some(4), "e").expect("resized");
        assert_eq!(sizing(&after), (Some(16384), Some(4)));
    }

    #[test]
    fn a_remote_endpoint_refuses_new_sizing_rather_than_pretending() {
        let spec = EndpointSpec::Node(apexrouter_protocol::NodeSpec {
            base_url: "http://192.168.1.20:8888".to_string(),
            credential: apexrouter_protocol::CredentialSource::None,
            label: "lan box".to_string(),
            declared_models: Vec::new(),
            protocol: apexrouter_protocol::Protocol::default(),
        });
        let e = resized(&spec, Some(4096), None, "lan-box").expect_err("must refuse");
        assert!(e.to_string().contains("not ours to set"), "{e}");
        assert!(e.to_string().contains("lan-box"), "{e}");
    }

    #[test]
    fn finding_a_missing_endpoint_lists_what_there_is() {
        let e = find(&[], "nope").expect_err("must fail");
        assert!(e.to_string().contains("there are none"), "{e}");
    }
}

#[cfg(test)]
mod restart_e2e {
    //! `endpoint restart --ctx N`, proved where it can actually be disproved: in the argv
    //! the child process was handed.
    //!
    //! The defect this guards was invisible from inside the CLI. `--ctx` was composed into
    //! a JSON body perfectly correctly, sent, and then **discarded** —
    //! `POST /v1/endpoints/{id}/restart` replays the record's own spec and reads no body —
    //! after which the CLI printed `restarted … on port 8100` and the operator believed it.
    //! Every unit test over the request-building code passed. The only question that
    //! catches it is *"what did the child actually get?"*, so that is the question this
    //! asks, of the fake `llama-server`'s launch record.
    //!
    //! **No GPU, no GGUF, no weights.** The real supervisor spawns
    //! `apexrouter-tests-support`'s fake, the real health gate passes it, and nothing in
    //! here is a benchmark.
    //!
    //! Hermeticity: one tempdir holds `$HOME`, `$STATE`, `$CACHE` and `config.toml`; every
    //! credential variable is cleared; discovery is pointed at the fake build tree alone and
    //! every outbound base URL at a closed loopback port.

    // The environment lock is a `std::sync::Mutex` held across `.await`, for the reason
    // `daemon::tests` gives: it must exclude the *synchronous* tests that take it too.
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use apexrouter_core::config::Config;
    use apexrouter_core::lockfile::DaemonLock;
    use apexrouter_core::paths::Paths;
    use apexrouter_protocol::{KvType, TriState};
    use apexrouter_server::{build_state, run as serve, shutdown, Shutdown};
    use apexrouter_tests_support::{FakeBuild, GgufSpec};
    use std::sync::MutexGuard;

    /// The size it starts at, and the size it is restarted at. Far enough apart that no
    /// rounding, clamping or defaulting could turn the first into the second.
    const CTX_BEFORE: u32 = 8_192;
    const CTX_AFTER: u32 = 3_072;
    const SLOTS_BEFORE: u32 = 2;
    const SLOTS_AFTER: u32 = 1;

    /// The credential variables, cleared: no test may resolve a real one, and the developer
    /// running `cargo test` is exactly the person who has them exported.
    const CLEARED: [&str; 6] = [
        "PROXY_PORT",
        "APEXROUTER_TOKEN",
        "VAST_API_KEY",
        "VASTAI_API_KEY",
        "HF_TOKEN",
        "TOGETHER_API_KEY",
    ];

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_restart_with_new_sizing_reaches_the_childs_argv() {
        let fake = FakeBuild::new();
        let model = fake.model("Fake-9b-Q6_K.gguf", &GgufSpec::default().sized_mb(1));
        let d = Daemon::boot(&fake).await;

        let started: EndpointRecord = d
            .client
            .post("/v1/endpoints?alias=auto", &spec(&fake, &model))
            .await
            .expect("the endpoint started");
        let id = started.id.as_str().to_owned();
        let first_port = started.port.expect("a port was allocated");

        let before = fake
            .records()
            .for_port(first_port)
            .expect("the fake wrote a launch record");
        assert_eq!(
            before.flag("-c"),
            Some(CTX_BEFORE.to_string().as_str()),
            "argv: {}",
            before.argv_line()
        );

        let after_rec = restart(&d.client, &id, Some(CTX_AFTER), Some(SLOTS_AFTER))
            .await
            .expect("the endpoint restarted");

        // A restart that renames the endpoint is not a restart.
        assert_eq!(after_rec.id.as_str(), id);
        let second_port = after_rec.port.expect("a port was allocated");

        // ---- the assertion the defect could not survive --------------------------------
        let after = fake
            .records()
            .wait_for_port(second_port, Duration::from_secs(15))
            .expect("the restarted child wrote a launch record");
        assert_eq!(
            after.flag("-c"),
            Some(CTX_AFTER.to_string().as_str()),
            "--ctx never reached the child. argv: {}",
            after.argv_line()
        );
        assert_eq!(
            after.flag_as::<u32>("-np"),
            Some(SLOTS_AFTER),
            "--parallel never reached the child. argv: {}",
            after.argv_line()
        );
        assert!(
            fake.records().all().len() >= 2,
            "only one launch was ever recorded, so this read the original child"
        );

        // Same endpoint, not a different one wearing its name.
        assert_eq!(after.flag("-m"), before.flag("-m"));
        let fit = after_rec.fit.as_ref().expect("a fit was solved");
        assert_eq!(fit.ctx, CTX_AFTER);
        assert_eq!(fit.parallel, SLOTS_AFTER);
        assert!(
            after_rec
                .alias_bindings
                .iter()
                .any(|a| a.as_str() == "auto"),
            "an unbound `auto` looks exactly like a restart that worked, until the next \
             request 404s: {:?}",
            after_rec.alias_bindings
        );

        d.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_plain_restart_still_replays_the_recorded_spec() {
        let fake = FakeBuild::new();
        let model = fake.model("Fake-9b-Q6_K.gguf", &GgufSpec::default().sized_mb(1));
        let d = Daemon::boot(&fake).await;

        let started: EndpointRecord = d
            .client
            .post("/v1/endpoints", &spec(&fake, &model))
            .await
            .expect("the endpoint started");
        let id = started.id.as_str().to_owned();

        // No flags: this must take the daemon's own restart route, and the sizing must be
        // the recorded one rather than a default that happens to look plausible.
        let rec = restart(&d.client, &id, None, None)
            .await
            .expect("the endpoint restarted");
        let record = fake
            .records()
            .wait_for_port(rec.port.expect("a port"), Duration::from_secs(15))
            .expect("a launch record");
        assert_eq!(record.flag("-c"), Some(CTX_BEFORE.to_string().as_str()));
        assert_eq!(record.flag_as::<u32>("-np"), Some(SLOTS_BEFORE));

        d.stop().await;
    }

    // -------------------------------------------------------------------------------------
    // fixtures
    // -------------------------------------------------------------------------------------

    /// The spec both tests start from.
    fn spec(fake: &FakeBuild, model: &std::path::Path) -> EndpointSpec {
        EndpointSpec::LocalLlama(LocalLlamaSpec {
            build: fake.build_id(),
            model_path: model.display().to_string(),
            mmproj: None,
            alias_flag: "fake-9b".to_string(),
            host: "127.0.0.1".to_string(),
            port: None,
            ctx: Some(CTX_BEFORE),
            parallel: Some(SLOTS_BEFORE),
            kv_type: Some(KvType::Q8_0),
            ngl: NglPlan::All,
            split: SplitPlan {
                devices: vec!["Vulkan0".to_string()],
                mode: SplitMode::Layer,
                main_gpu: None,
                tensor_split: None,
            },
            mode: SamplingMode::Coding,
            flash_attn: Some(TriState::Auto),
            api_key: None,
            extra_args: Vec::new(),
        })
    }

    /// A port nothing is listening on. Bound and released, which is the only portable way
    /// to ask the kernel for one.
    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
        let p = l.local_addr().expect("local_addr").port();
        drop(l);
        p
    }

    /// The real daemon — `build_state` and `run`, never a router assembled here — rooted in
    /// a tempdir, with discovery pointed at the fake build tree.
    struct Daemon {
        client: NodeClient,
        trigger: Shutdown,
        task: tokio::task::JoinHandle<anyhow::Result<()>>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
        _dir: tempfile::TempDir,
        /// Dropped last: released only once the environment has been put back.
        _guard: MutexGuard<'static, ()>,
    }

    impl Daemon {
        async fn boot(fake: &FakeBuild) -> Daemon {
            let guard = crate::daemon::testenv::lock();
            let vars = [
                "HOME",
                "APEXROUTER_HOME",
                "APEXROUTER_CONFIG",
                "APEXROUTER_URL",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
                "XDG_CONFIG_HOME",
            ];
            let mut saved: Vec<(&'static str, Option<std::ffi::OsString>)> = vars
                .iter()
                .chain(CLEARED.iter())
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
            saved.dedup_by_key(|(k, _)| *k);

            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            std::env::set_var("HOME", root);
            std::env::set_var("APEXROUTER_HOME", root.join("state"));
            std::env::set_var("APEXROUTER_CONFIG", root.join("config.toml"));
            std::env::set_var("XDG_STATE_HOME", root.join("xdg-state"));
            std::env::set_var("XDG_CACHE_HOME", root.join("xdg-cache"));
            std::env::set_var("XDG_CONFIG_HOME", root.join("xdg-config"));
            std::env::remove_var("APEXROUTER_URL");
            for k in CLEARED {
                std::env::remove_var(k);
            }
            let paths = Paths::resolve().expect("paths");
            paths.ensure_layout().expect("layout");

            let mut cfg = Config::default();
            cfg.server.proxy_bind = format!("127.0.0.1:{}", free_port());
            cfg.server.control_bind = format!("127.0.0.1:{}", free_port());
            cfg.server.drain_timeout_secs = 5;
            cfg.compat.mirror_usage_log = false;
            cfg.compat.read_legacy_state = false;
            // Discovery sees the fake and nothing else: no real `~/llama.cpp` is probed and
            // no real GGUF is enumerated.
            cfg.endpoints.build_roots = vec![fake.root().display().to_string()];
            cfg.endpoints.model_roots = vec![fake.root().join("models").display().to_string()];
            let base = free_port();
            cfg.endpoints.port_range = (base, base.saturating_add(30));
            cfg.supervisor.health_deadline_ms = 30_000;
            cfg.supervisor.health_interval_ms = 50;
            // HERMETICITY: every outbound root points at a closed loopback port.
            cfg.vast.base_url = "http://127.0.0.1:1/api/v0".to_string();
            for p in cfg.providers.values_mut() {
                p.base_url = "http://127.0.0.1:1/v1".to_string();
            }
            let control = format!("http://{}", cfg.control_bind());

            let lock = DaemonLock::acquire(&paths).expect("daemon lock");
            let state = build_state(paths, cfg, lock).await.expect("build_state");
            let (trigger, handle) = shutdown::channel();
            let mut task = tokio::spawn(serve(state, handle));

            let client = NodeClient::new(control, None);
            let mut up = false;
            for _ in 0..600 {
                if client.health().await.is_ok() {
                    up = true;
                    break;
                }
                if task.is_finished() {
                    match (&mut task).await {
                        Ok(Ok(())) => panic!("the daemon exited before serving /health"),
                        Ok(Err(e)) => panic!("the daemon failed to start: {e:#}"),
                        Err(e) => panic!("the daemon task panicked: {e}"),
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert!(up, "the control listener never answered /health");

            Daemon {
                client,
                trigger,
                task,
                saved,
                _dir: dir,
                _guard: guard,
            }
        }

        /// Drain, so no fake child outlives the test and the tempdir is not removed from
        /// under a running daemon, then put the environment back.
        async fn stop(self) {
            self.trigger.trigger();
            let _ = tokio::time::timeout(Duration::from_secs(20), self.task).await;
            for (k, v) in &self.saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}
