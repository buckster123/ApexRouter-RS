//! OWNER: unit S-08 (cli/src/cmd/{vast,tunnel,hf,provider,recipe,profile,usage,smoke,doctor,compare,backend,swap,up,approvals,token,open,env,migrate}.rs). Do not edit outside that unit.
//!
//! `apexrouter vast account | offers | gpu-names | rent | ls | watch | log | diagnose | restart-download | destroy`. Money verbs require `--yes` and print $/hr, the estimated total and the **current credit** before acting.
//!
//! # The money gate
//!
//! `rent` and `destroy` are the two verbs that move money, and neither has an interactive
//! prompt: `--yes` is required, it is a flag, and a flag is auditable in a shell history in
//! a way that "press y" is not. Before `rent` acts it prints the four numbers from
//! `providers::vast::rent::preview` — **$/hr, estimated total, current credit and
//! burn-down** — plus every warning that quote carries. `--dry-run` stops right there: no
//! reservation, no ledger row, no call.
//!
//! The preview is computed from the *same pure function* the 409 body of
//! `POST /v1/vast/instances`, the web UI's rent drawer and the Slint dialog use. Two
//! implementations of "what will this cost?" would eventually disagree, and the one a human
//! read would be the wrong one.
//!
//! # Why `ls` reads the ledger
//!
//! §7 classifies `vast ls` as `ReadState`. A box that is billing must stay visible when the
//! daemon is not running — that is the entire reason the ledger is an append-only file
//! written *before* the billing call rather than a cache of what the API last said.

use crate::cli::{parse_geo, VastCmd, VastOffersArgs, VastRentArgs};
use crate::cmd::Ctx;
use crate::daemon::{Need, Serving};
use crate::render;
use apexrouter_client::NodeClient;
use apexrouter_core::argv::{plan_container, ContainerLaunchInput};
use apexrouter_core::ledger::Ledger;
use apexrouter_protocol::{
    BootPhase, CheckResult, ContainerRuntime, Event, FavoriteHost, FavoriteVerdict, GeoFilter,
    LedgerRow, LedgerState, Offer, OfferSearchResult, ProfileId, SamplingMode, ServedBy,
    VastAccount, VastInstance,
};
use apexrouter_providers::vast::rent::{preview, RentPreview, PREVIEW_HOURS};
use futures_util::StreamExt;
use std::collections::HashMap;

/// Run `apexrouter vast …`.
///
/// # Errors
/// A daemon that will not answer, a money verb without `--yes`, or an unknown instance.
pub async fn run(ctx: &Ctx, cmd: &VastCmd) -> anyhow::Result<()> {
    match cmd {
        VastCmd::Account(args) => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let a: VastAccount = client.get("/v1/vast/account").await?;
            if args.json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &a);
            }
            render::print_line(&format!("credit    ${:.2}", a.credit));
            if let Some(b) = a.balance {
                render::print_line(&format!("balance   ${b:.2}"));
            }
            render::print_line(&format!("can pay   {}", render::dash(a.can_pay)));
            Ok(())
        }
        VastCmd::GpuNames(args) => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let names: Vec<String> = client.get("/v1/vast/gpu-names").await?;
            if args.json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &names);
            }
            for n in &names {
                render::print_line(n);
            }
            Ok(())
        }
        VastCmd::Offers(args) => offers(ctx, args).await,
        VastCmd::Rent(args) => rent(ctx, args).await,
        VastCmd::Ls { orphans, json } => list(ctx, *orphans, *json).await,
        VastCmd::Watch { id } => watch(ctx, *id).await,
        VastCmd::Log { id, follow } => log(ctx, *id, *follow).await,
        VastCmd::Diagnose { id, json } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let results: Vec<CheckResult> = client
                .get(&format!("/v1/vast/instances/{id}/diagnose"))
                .await?;
            if *json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &results);
            }
            crate::cmd::doctor::print_results(&results);
            Ok(())
        }
        VastCmd::RestartDownload { id } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let _: serde_json::Value = client
                .post(
                    &format!("/v1/vast/instances/{id}/restart-download"),
                    &serde_json::json!({}),
                )
                .await?;
            render::print_line(&format!("restarted the model download on instance {id}"));
            Ok(())
        }
        VastCmd::Destroy { id, all, yes } => destroy(ctx, *id, *all, *yes).await,
        VastCmd::Star {
            machine_id,
            burn,
            note,
        } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let verdict = if *burn { "skull" } else { "star" };
            let row: FavoriteHost = client
                .put(
                    &format!("/v1/vast/favorites/{machine_id}"),
                    &serde_json::json!({ "verdict": verdict, "note": note }),
                )
                .await?;
            render::print_line(&format!(
                "machine {} remembered as {}{}",
                row.machine_id,
                verdict,
                row.note
                    .as_deref()
                    .map(|n| format!(": {n}"))
                    .unwrap_or_default()
            ));
            Ok(())
        }
        VastCmd::Unstar { machine_id } => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            client
                .delete(&format!("/v1/vast/favorites/{machine_id}"))
                .await?;
            render::print_line(&format!("machine {machine_id} forgotten"));
            Ok(())
        }
        VastCmd::Favorites(args) => {
            let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
            let rows: Vec<FavoriteHost> = client.get("/v1/vast/favorites").await?;
            if args.json {
                return render::print_json(ServedBy::Daemon, render::now_unix(), false, &rows);
            }
            render::print_table(
                &["MARK", "MACHINE", "GPU", "GEO", "NOTE"],
                rows.iter()
                    .map(|f| {
                        vec![
                            verdict_mark(f.verdict).to_owned(),
                            f.machine_id.to_string(),
                            f.gpu_name.clone().unwrap_or_default(),
                            f.geolocation.clone().unwrap_or_default(),
                            f.note.clone().unwrap_or_default(),
                        ]
                    })
                    .collect(),
            );
            if rows.is_empty() {
                render::print_line("(no host verdicts yet — `apexrouter vast star <machine_id>`)");
            }
            Ok(())
        }
        VastCmd::Park { id, yes } => park(ctx, *id, *yes).await,
        VastCmd::Wake { id, yes } => wake(ctx, *id, *yes).await,
    }
}

/// `★` for a star, `☠` for a burn — the two glyphs the campaign ledger already uses.
fn verdict_mark(v: apexrouter_protocol::FavoriteVerdict) -> &'static str {
    match v {
        apexrouter_protocol::FavoriteVerdict::Star => "★",
        apexrouter_protocol::FavoriteVerdict::Skull => "☠",
    }
}

/// `vast park <id>` — stop the GPUs, hold the disk. The confirm shows the weekly figure.
///
/// # Errors
/// A missing `--yes`, or a daemon/instance that will not park.
async fn park(ctx: &Ctx, id: u64, yes: bool) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    if !yes {
        // Quote the disk cost before asking for the flag, so `--yes` is informed consent.
        let live: Vec<VastInstance> = client.get("/v1/vast/instances").await?;
        let inst = live.iter().find(|i| i.id.0 == id);
        let weekly = inst.and_then(apexrouter_providers::vast::weekly_disk_usd);
        let disk = inst.and_then(|i| i.disk_space).unwrap_or_default();
        anyhow::bail!(
            "parking stops the GPU bill but keeps the disk ({disk:.0} GB at {}); \
             re-run with --yes",
            weekly.map_or_else(
                || "an unpriced rate, typically $0.12-0.19/GB/month".to_owned(),
                |w| format!("~${w:.2}/week")
            )
        );
    }
    let receipt: serde_json::Value = client
        .post(
            &format!("/v1/vast/instances/{id}/park"),
            &serde_json::json!({}),
        )
        .await?;
    let weekly = receipt
        .get("weekly_disk_usd")
        .and_then(serde_json::Value::as_f64);
    render::print_line(&format!(
        "instance {id} parked — GPUs released, disk held{}",
        weekly
            .map(|w| format!(" at ~${w:.2}/week"))
            .unwrap_or_default()
    ));
    render::print_line(&format!("wake it with: apexrouter vast wake {id} --yes"));
    Ok(())
}

/// `vast wake <id>` — restart a parked box. **Resumes the hourly bill.**
///
/// # Errors
/// A missing `--yes`, or a daemon/instance that will not start.
async fn wake(ctx: &Ctx, id: u64, yes: bool) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    if !yes {
        let live: Vec<VastInstance> = client.get("/v1/vast/instances").await?;
        let dph = live.iter().find(|i| i.id.0 == id).and_then(|i| i.dph_total);
        anyhow::bail!(
            "waking instance {id} resumes billing{} — re-run with --yes",
            dph.map(|d| format!(" at ${d:.4}/hr")).unwrap_or_default()
        );
    }
    let job: serde_json::Value = client
        .post(
            &format!("/v1/vast/instances/{id}/wake?confirm=true&source=cli"),
            &serde_json::json!({}),
        )
        .await?;
    render::print_line(&format!(
        "waking instance {id} (job {}) — follow with `apexrouter vast watch {id}`",
        job.get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
    ));
    Ok(())
}

/// `vast offers` — one search path, whether the query came from a profile or from flags.
///
/// # Errors
/// An unknown profile, an unusable `--geo`, or a daemon that will not answer.
async fn offers(ctx: &Ctx, args: &VastOffersArgs) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    let body = offer_query(ctx, args)?;
    let result: OfferSearchResult = client.post("/v1/vast/offers/search", &body).await?;
    // Host verdicts, so the table marks a ★ machine before the operator re-derives it and
    // a ☠ one before they re-learn it. Best effort: no verdicts is an empty map.
    let marks: HashMap<u64, FavoriteVerdict> = client
        .get::<Vec<FavoriteHost>>("/v1/vast/favorites")
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|f| (f.machine_id, f.verdict))
                .collect()
        })
        .unwrap_or_default();

    if args.json {
        return render::print_json(ServedBy::Daemon, render::now_unix(), false, &result);
    }
    // A relaxation is a banner, never a footnote: the whole documented bug was a search
    // that quietly widened and rented from a set the operator had not seen.
    for r in &result.relaxations {
        render::print_line(&format!("relaxed: {r}"));
    }
    render::print_table(
        &[
            "MARK", "OFFER", "MACHINE", "GPU", "N", "VRAM", "$/HR", "$/HR/GPU", "REL", "DOWN",
            "DL$/TB", "DISK", "CPU", "GEO",
        ],
        result
            .offers
            .iter()
            .take(args.limit as usize)
            .map(|o| offer_row(o, &marks))
            .collect(),
    );
    if result.offers.is_empty() {
        render::print_line(
            "(no offer matched — `apexrouter vast gpu-names` lists the live vocabulary)",
        );
    }
    Ok(())
}

/// The `POST /v1/vast/offers/search` body: `{profile}` or an explicit query.
///
/// # Errors
/// An unusable `--geo`, or a `--profile` that is not a valid id.
fn offer_query(_ctx: &Ctx, args: &VastOffersArgs) -> anyhow::Result<serde_json::Value> {
    if let Some(p) = &args.profile {
        let id = ProfileId::parse(p)
            .map_err(|e| anyhow::anyhow!("`{p}` is not a valid profile id: {e}"))?;
        let mut body = serde_json::json!({ "profile": id.as_str() });
        if let Some(m) = args.machine {
            body["machine_id"] = serde_json::Value::from(m);
        }
        return Ok(body);
    }
    let geo = args
        .geo
        .as_deref()
        .map(parse_geo)
        .transpose()?
        .unwrap_or(GeoFilter::Any);
    let mut body = serde_json::json!({
        "gpu_names": args.gpus,
        "num_gpus_min": args.num_gpus.unwrap_or(1),
        "num_gpus_max": args.num_gpus.unwrap_or(1),
        "max_dph": args.max_price,
        "geo": geo,
        "limit": args.limit,
    });
    if let Some(m) = args.machine {
        // The raw-query arm carries it in `extra`, the passthrough the wire query merges
        // into `q` verbatim — the same shape the favorites doctrine proved live.
        body["extra"] = serde_json::json!({ "machine_id": { "eq": m } });
    }
    Ok(body)
}

/// `vast rent` — **the** money verb.
///
/// # Errors
/// A missing `--yes`, an offer that cannot be quoted, a launch the container builder
/// refuses, or a daemon that declines the rental.
async fn rent(ctx: &Ctx, args: &VastRentArgs) -> anyhow::Result<()> {
    if args.offer_id.is_none() && !args.auto && args.machine.is_none() {
        anyhow::bail!(
            "name an offer id, pass --auto to take the cheapest the profile matches, or \
             --machine <machine_id> to take a known host's current ask"
        );
    }
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;

    let profile = ProfileId::parse(&args.profile)
        .map_err(|e| anyhow::anyhow!("`{}` is not a valid profile id: {e}", args.profile))?;
    let launch = container(ctx, args)?;

    // ---- the quote, from the same pure function every other surface quotes with ----------
    let offer = pick_offer(&client, &profile, args.offer_id, args.machine).await?;
    let request = serde_json::json!({
        "profile": profile.as_str(),
        "offer_id": args.offer_id,
        "launch": launch,
        "confirm": false,
        "max_usd_per_hour": args.max_hourly,
        "auto_tunnel": true,
        "bind_alias": args.alias,
    });
    let account: Option<VastAccount> = client.get("/v1/vast/account").await.ok();
    let quote = quote_for(&offer, args.max_hourly, account.as_ref().map(|a| a.credit));

    if args.json && args.dry_run {
        return render::print_json(ServedBy::Daemon, render::now_unix(), false, &quote);
    }
    print_quote(&quote);

    if args.dry_run {
        render::print_line("--dry-run: nothing was reserved and nothing was rented.");
        return Ok(());
    }
    if !args.yes {
        anyhow::bail!(
            "renting offer {} starts a bill at ${:.4}/hr — re-run with --yes once the \
             numbers above are what you expect",
            offer.id,
            offer.dph_total
        );
    }

    // ---- the approval is explicit, and it is the only thing that unlocks the spend -------
    let mut body = request;
    body["confirm"] = serde_json::Value::Bool(true);
    body["offer_id"] = serde_json::Value::from(offer.id);
    let path = match args.no_wait {
        true => "/v1/vast/instances?no_wait=true",
        false => "/v1/vast/instances",
    };
    let raw: serde_json::Value = client.post(path, &body).await?;

    if args.json {
        return render::print_json(ServedBy::Daemon, render::now_unix(), false, &raw);
    }
    render::print_line(&raw.to_string());
    Ok(())
}

/// The container contract for a rental, built by the **same** `core::argv` builder that
/// produces the local argv — which is why the `--top-k 20` divergence between two
/// hand-maintained tables cannot recur.
///
/// # Errors
/// A llama.cpp launch without `--model-repo`/`--quant`, or a vLLM launch without
/// `--model-id`.
fn container(ctx: &Ctx, args: &VastRentArgs) -> anyhow::Result<serde_json::Value> {
    let runtime = if args.model_id.is_some() {
        ContainerRuntime::Vllm
    } else {
        ContainerRuntime::LlamaCpp
    };
    let input = ContainerLaunchInput {
        runtime,
        image_type: None,
        model_repo: args.model_repo.clone(),
        model_quant: args.quant.clone(),
        model_id: args.model_id.clone(),
        ctx: args.ctx,
        parallel: None,
        kv_type: None,
        mode: SamplingMode::Thinking,
        mmproj: None,
        disk_gb: args.disk_gb,
        tp: None,
        quantization: None,
        kv_cache_dtype: None,
        enforce_eager: false,
        reasoning_parser: None,
        // Tunnel-only is the default posture (§9.5); a public direct port needs a minted
        // per-instance key and is not something a CLI flag should turn on by accident.
        expose_public: false,
        // The HF token goes in the env MAP, never in `onstart` — vast persists that string
        // and echoes it back in `show instance`. The daemon injects it; the CLI does not
        // read a credential it has no reason to hold.
        hf_token: None,
    };
    let (launch, preview) = plan_container(&input, &ctx.cfg)?;
    for w in &preview.warnings {
        render::print_line(&format!("note: {w}"));
    }
    Ok(serde_json::to_value(launch)?)
}

/// The offer to quote and rent: the named one, the pinned machine's cheapest ask, or the
/// cheapest the profile matches.
///
/// A named offer travels in the search body, where the daemon re-checks it **by the
/// profile's constraints** — not by membership in the top-N-by-price window, which once
/// refused a fully compliant machine because cheaper stock boxes outbid it out of the list
/// (GARDEN-RUNS.md, R4). A refusal now names the constraint that actually failed.
///
/// # Errors
/// A search that matched nothing, a named offer that is gone (ask ids re-mint), or one
/// that genuinely fails a profile constraint.
async fn pick_offer(
    client: &NodeClient,
    profile: &ProfileId,
    offer_id: Option<u64>,
    machine: Option<u64>,
) -> anyhow::Result<Offer> {
    let mut body = serde_json::json!({ "profile": profile.as_str() });
    if let Some(id) = offer_id {
        body["offer_id"] = serde_json::Value::from(id);
    }
    if let Some(m) = machine {
        body["machine_id"] = serde_json::Value::from(m);
    }
    let result: OfferSearchResult = client.post("/v1/vast/offers/search", &body).await?;
    for r in &result.relaxations {
        render::print_line(&format!("relaxed: {r}"));
    }
    match offer_id {
        // The daemon either appended it (constraints pass) or refused with the failing
        // constraint named; absence here means the ask id itself is gone.
        Some(id) => result
            .offers
            .iter()
            .find(|o| o.id == id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "offer {id} was not found — ask ids re-mint on some hosts; re-run \
                     `apexrouter vast offers --profile {}`, or pin the host with --machine",
                    profile.as_str()
                )
            }),
        None => cheapest(&result.offers).ok_or_else(|| {
            let scope = machine
                .map(|m| format!(" on machine {m}"))
                .unwrap_or_default();
            anyhow::anyhow!(
                "`{}` matched no rentable offer{scope} — `apexrouter profile show {}` \
                 shows the floors it is asking for",
                profile.as_str(),
                profile.as_str()
            )
        }),
    }
}

/// The cheapest rentable offer, by all-in `$/hr`.
///
/// Rentable-only, because "cheapest" including something already taken is a quote a human
/// cannot act on.
pub fn cheapest(offers: &[Offer]) -> Option<Offer> {
    offers
        .iter()
        .filter(|o| o.rentable.unwrap_or(true) && !o.rented.unwrap_or(false))
        .min_by(|a, b| {
            a.dph_total
                .partial_cmp(&b.dph_total)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
}

/// Quote one offer against a ceiling and the account's credit.
///
/// A thin wrapper over `providers::vast::rent::preview` so the CLI cannot grow its own
/// arithmetic: this exists to name the hour window, not to compute anything.
pub fn quote_for(offer: &Offer, max_hourly: f64, credit: Option<f64>) -> RentPreview {
    // `RentRequest` here is a *quote input*, not the body that gets posted; only
    // `max_usd_per_hour` and `offer_id` participate in the arithmetic.
    let req = apexrouter_protocol::RentRequest {
        profile: None,
        offer_id: Some(offer.id),
        launch: apexrouter_protocol::ContainerLaunch {
            runtime: ContainerRuntime::LlamaCpp,
            image: String::new(),
            image_type: apexrouter_protocol::ImageType::Prebuilt,
            disk_gb: 0,
            env: Default::default(),
            onstart: String::new(),
            host: "127.0.0.1".to_string(),
            port: 8000,
            expose_public: false,
        },
        confirm: false,
        max_usd_per_hour: max_hourly,
        auto_tunnel: true,
        bind_alias: None,
    };
    preview(offer, &req, credit, PREVIEW_HOURS)
}

/// The four numbers, then every warning the quote carries.
///
/// `credit: None` prints as "could not be read" rather than `$0.00`: we could not ask, and
/// saying so is the honest answer.
pub fn print_quote(q: &RentPreview) {
    render::print_line(&format!(
        "offer {}  {} x{}  ${:.4}/hr",
        q.offer_id, q.gpu_name, q.num_gpus, q.dph_total
    ));
    render::print_line(&format!("  $/hr         ${:.4}", q.dph_total));
    render::print_line(&format!(
        "  est total    ${:.2} over {:.1} hour(s)",
        q.est_total_usd, q.est_hours
    ));
    match q.credit {
        Some(c) => render::print_line(&format!("  credit       ${c:.2}")),
        None => render::print_line("  credit       (could not be read)"),
    }
    if let Some(h) = q.burn_down_hours {
        render::print_line(&format!("  burn-down    {h:.1} hours at this rate"));
    }
    render::print_line(&format!("  ceiling      ${:.4}/hr", q.max_usd_per_hour));
    for w in &q.warnings {
        render::print_line(&format!("  WARNING      {w}"));
    }
}

/// `vast ls` — live instances from the daemon, the ledger when there is none.
///
/// # Errors
/// A ledger that cannot be read.
async fn list(ctx: &Ctx, orphans: bool, json: bool) -> anyhow::Result<()> {
    let serving = ctx.serving(Need::ReadState).await?;
    if let Serving::Daemon(c) = &serving {
        let live: Vec<VastInstance> = c.get("/v1/vast/instances").await?;
        if json {
            return render::print_json(ServedBy::Daemon, render::now_unix(), false, &live);
        }
        render::print_table(
            &[
                "INSTANCE", "STATUS", "PHASE", "GPU", "N", "$/HR", "UPTIME", "GEO", "NOTE",
            ],
            live.iter().map(instance_row).collect(),
        );
        render::print_line(&format!(
            "burn ${:.4}/hr across {} instance(s)",
            // `.max(0.0)` normalises float negative zero, which renders as "$-0.0000".
            apexrouter_providers::checks::burn_per_hour(&live).max(0.0),
            live.len()
        ));
        let parked: Vec<&VastInstance> = live
            .iter()
            .filter(|i| i.phase() == BootPhase::Parked)
            .collect();
        if !parked.is_empty() {
            let weekly: f64 = parked
                .iter()
                .filter_map(|i| apexrouter_providers::vast::weekly_disk_usd(i))
                .sum();
            render::print_line(&format!(
                "plus {} parked instance(s) holding disk{}",
                parked.len(),
                if weekly > 0.0 {
                    format!(" at ~${weekly:.2}/week")
                } else {
                    String::new()
                }
            ));
        }
        return Ok(());
    }

    // Offline: the ledger. `active()` is every row without a verified destroy, which is
    // exactly the set that may still be billing.
    let rows = Ledger::open(&ctx.paths)?.active()?;
    let rows: Vec<LedgerRow> = if orphans {
        rows.into_iter()
            .filter(|r| r.state != LedgerState::Destroyed)
            .collect()
    } else {
        rows
    };
    if json {
        return render::print_json(serving.served_by(), render::now_unix(), true, &rows);
    }
    render::print_offline_notice();
    render::print_table(
        &["INSTANCE", "STATE", "GPU", "N", "$/HR", "SINCE", "NOTE"],
        rows.iter().map(ledger_row).collect(),
    );
    Ok(())
}

/// `vast watch <id>` — the boot state machine, live off the WebSocket.
///
/// # Errors
/// A daemon that will not answer, or a subscription that cannot be established.
async fn watch(ctx: &Ctx, id: u64) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    let mut stream = Box::pin(client.subscribe().await?);
    render::print_line(&format!("watching instance {id} (ctrl-c to stop)"));
    while let Some(item) = stream.next().await {
        // An `Err` from this stream is a blip, not a terminator: the client reconnects.
        let Ok(event) = item else { continue };
        match event {
            Event::BootProgress {
                backend,
                phase,
                line,
            } if backend.as_str().contains(&id.to_string()) => {
                render::print_line(&format!(
                    "{}  {}",
                    render::variant(&phase),
                    line.unwrap_or_default()
                ));
                if phase.is_terminal() {
                    return Ok(());
                }
            }
            // The boot watcher streams a *changed* status line here without re-broadcasting
            // the phase — this is where the dashboard's words reach the terminal.
            Event::LogLine {
                source: apexrouter_protocol::LogSource::Instance { id: from },
                line,
            } if from.0 == id => {
                render::print_line(&format!("        {line}"));
            }
            Event::VastFleetChanged { instances, credit } => {
                if let Some(i) = instances.iter().find(|i| i.id.0 == id) {
                    render::print_line(&format!(
                        "{}  {}{}{}",
                        render::variant(&i.phase()),
                        i.actual_status.clone().unwrap_or_default(),
                        i.status_note()
                            .map(|n| format!("  {n}"))
                            .unwrap_or_default(),
                        credit
                            .map(|c| format!("  credit ${c:.2}"))
                            .unwrap_or_default()
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// `vast log <id>` — the container log, tailed or followed.
///
/// # Errors
/// A daemon that will not answer, or an instance whose log cannot be requested.
async fn log(ctx: &Ctx, id: u64, follow: bool) -> anyhow::Result<()> {
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    if !follow {
        let text: serde_json::Value = client.get(&format!("/v1/vast/instances/{id}/log")).await?;
        match text.as_str() {
            Some(s) => {
                for line in s.lines() {
                    render::print_line(line);
                }
            }
            None => render::print_line(&text.to_string()),
        }
        return Ok(());
    }
    let mut stream = Box::pin(
        client
            .sse(&format!("/v1/vast/instances/{id}/log?follow=1"))
            .await?,
    );
    while let Some(item) = stream.next().await {
        let Ok(event) = item else { continue };
        if let Event::LogLine { line, .. } = event {
            render::print_line(&line);
        }
    }
    Ok(())
}

/// `vast destroy` — the other money verb. Verifies before forgetting.
///
/// # Errors
/// A missing `--yes`, neither an id nor `--all`, or a daemon that will not answer.
async fn destroy(ctx: &Ctx, id: Option<u64>, all: bool, yes: bool) -> anyhow::Result<()> {
    if !yes {
        anyhow::bail!("destroying an instance is irreversible — pass --yes");
    }
    let client = ctx.serving(Need::Mutate).await?.into_daemon()?;
    let ids = match (id, all) {
        (Some(one), _) => vec![one],
        (None, true) => client
            .get::<Vec<VastInstance>>("/v1/vast/instances")
            .await?
            .iter()
            .map(|i| i.id.0)
            .collect(),
        (None, false) => anyhow::bail!("name an instance id, or pass --all"),
    };
    if ids.is_empty() {
        render::print_line("(nothing is rented)");
        return Ok(());
    }
    for id in ids {
        // `?confirm=true` is the server's own gate; `--yes` is this one. Both, on purpose.
        client
            .delete(&format!("/v1/vast/instances/{id}?confirm=true"))
            .await?;
        render::print_line(&format!("destroyed {id}"));
    }
    Ok(())
}

/// One row of the offers table.
///
/// `MARK` carries the operator's remembered verdict on the physical machine; `CPU` is the
/// host-class read the R2a lemon taught (an i5 on an H610M is "verified" too), truncated —
/// the operator needs the class, not the SKU suffix.
fn offer_row(o: &Offer, marks: &HashMap<u64, FavoriteVerdict>) -> Vec<String> {
    vec![
        o.machine_id
            .and_then(|m| marks.get(&m))
            .map(|v| verdict_mark(*v).to_owned())
            .unwrap_or_default(),
        o.id.to_string(),
        o.machine_id.map(|m| m.to_string()).unwrap_or_default(),
        o.gpu_name.clone(),
        o.num_gpus.to_string(),
        render::human_mib(o.pooled_vram_mb()),
        format!("{:.4}", o.dph_total),
        format!("{:.4}", o.dph_total / o.num_gpus.max(1) as f64),
        o.reliability2
            .map(|r| format!("{r:.3}"))
            .unwrap_or_default(),
        o.inet_down.map(|d| format!("{d:.0}")).unwrap_or_default(),
        // $/GB on the wire, $/TB for humans. Below fifty cents a terabyte is "~0": the
        // operator is choosing hosts, not auditing rounding.
        o.inet_down_cost
            .map(|c| {
                let per_tb = c * 1000.0;
                if per_tb < 0.5 {
                    "~0".to_owned()
                } else {
                    format!("{per_tb:.1}")
                }
            })
            .unwrap_or_default(),
        o.disk_space.map(|d| format!("{d:.0}")).unwrap_or_default(),
        o.cpu_name
            .as_deref()
            .map(|c| {
                let flat = c.split_whitespace().collect::<Vec<_>>().join(" ");
                let mut out: String = flat.chars().take(22).collect();
                if flat.chars().count() > 22 {
                    out.push('…');
                }
                out
            })
            .unwrap_or_default(),
        o.geolocation.clone().unwrap_or_default(),
    ]
}

/// One row of the live-instances table.
fn instance_row(i: &VastInstance) -> Vec<String> {
    vec![
        i.id.0.to_string(),
        i.actual_status.clone().unwrap_or_default(),
        render::variant(&i.phase()),
        i.gpu_name.clone().unwrap_or_default(),
        render::dash(i.num_gpus),
        i.dph_total.map(|d| format!("{d:.4}")).unwrap_or_default(),
        i.uptime_secs()
            .map(|s| render::human_secs(s as i64))
            .unwrap_or_default(),
        i.geolocation.clone().unwrap_or_default(),
        instance_note(i),
    ]
}

/// The `NOTE` column: the `status_msg`, surfaced instead of hidden.
///
/// Two lemons in one campaign sat in `pulling` for minutes while `status_msg` carried
/// the containerd error the whole time — visible in the vast dashboard, invisible here
/// (GARDEN-RUNS.md, R1 attempts 1/3/4). The words and the fatal test come from the **one**
/// helper pair on `VastInstance`, so `ls`, `watch` and the boot watchdog agree. A fatal
/// line always shows; progress chatter shows only while the box is still booting; a
/// healthy or parked box with routine chatter renders nothing. Truncated, because the
/// operator needs the *smell*, not the stack.
fn instance_note(i: &VastInstance) -> String {
    let Some(note) = i.status_note() else {
        return String::new();
    };
    let booting = matches!(
        i.phase(),
        BootPhase::Reserved | BootPhase::Provisioning | BootPhase::Pulling
    );
    if !(i.status_looks_fatal() || booting) {
        return String::new();
    }
    let mut out: String = note.chars().take(60).collect();
    if note.chars().count() > 60 {
        out.push('…');
    }
    out
}

/// One row of the offline (ledger) table.
fn ledger_row(r: &LedgerRow) -> Vec<String> {
    vec![
        r.instance_id
            .map(|i| i.0.to_string())
            .unwrap_or_else(|| "-".to_string()),
        render::variant(&r.state),
        r.gpu.clone().unwrap_or_default(),
        render::dash(r.num_gpus),
        r.dph.map(|d| format!("{d:.4}")).unwrap_or_default(),
        render::human_secs(render::now_unix() - r.at_unix),
        r.note.clone().unwrap_or_default(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(id: u64, dph: f64, rentable: Option<bool>, rented: Option<bool>) -> Offer {
        Offer {
            id,
            ask_contract_id: None,
            machine_id: None,
            gpu_name: "RTX 3090".to_string(),
            num_gpus: 2,
            gpu_ram: 24_576,
            gpu_total_ram: 49_152,
            dph_total: dph,
            dph_base: None,
            storage_cost: None,
            inet_down_cost: None,
            inet_up_cost: None,
            cpu_ram: None,
            cpu_cores_effective: None,
            cpu_name: None,
            mobo_name: None,
            host_id: None,
            disk_space: None,
            cuda_max_good: None,
            driver_version: None,
            geolocation: Some("Czechia, CZ".to_string()),
            inet_down: None,
            inet_up: None,
            reliability2: Some(0.99),
            direct_port_count: None,
            static_ip: None,
            rented,
            rentable,
            dlperf: None,
            dlperf_per_dphtotal: None,
            duration: None,
            end_date: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn the_note_column_surfaces_errors_and_boot_progress_but_not_healthy_chatter() {
        let inst = |status: &str, msg: Option<&str>| -> VastInstance {
            serde_json::from_value(serde_json::json!({
                "id": 1, "actual_status": status, "status_msg": msg,
            }))
            .expect("de")
        };
        assert_eq!(instance_note(&inst("running", None)), "");
        // Routine chatter on a healthy box: silence.
        assert_eq!(
            instance_note(&inst("running", Some("0e7f068147ad: Download complete"))),
            ""
        );
        // The same chatter while still pulling: progress worth showing.
        assert_eq!(
            instance_note(&inst("loading", Some("0e7f068147ad: Download complete"))),
            "0e7f068147ad: Download complete"
        );
        // An error shows whatever the phase claims.
        let n = instance_note(&inst(
            "running",
            Some("Error response from daemon: failed to create task for container\nand more"),
        ));
        assert!(n.starts_with("Error response from daemon"), "{n}");
        assert!(!n.contains('\n'), "flattened: {n}");
        let long = format!("error {}", "x".repeat(200));
        assert!(instance_note(&inst("loading", Some(&long))).chars().count() <= 61);
    }

    #[test]
    fn the_dl_cost_column_prints_per_tb_and_calls_home_boxes_free() {
        let marks = HashMap::new();
        let mut o = offer(1, 0.30, Some(true), Some(false));
        o.inet_down_cost = Some(0.0091);
        assert_eq!(offer_row(&o, &marks)[10], "9.1");
        o.inet_down_cost = Some(0.000_13);
        assert_eq!(offer_row(&o, &marks)[10], "~0");
        o.inet_down_cost = None;
        assert_eq!(offer_row(&o, &marks)[10], "");
    }

    #[test]
    fn the_mark_machine_and_cpu_columns_carry_the_host_class() {
        let mut marks = HashMap::new();
        marks.insert(140_330, FavoriteVerdict::Star);
        marks.insert(999, FavoriteVerdict::Skull);
        let mut o = offer(1, 0.84, Some(true), Some(false));
        o.machine_id = Some(140_330);
        o.cpu_name = Some("Intel Xeon Gold 6133 CPU @ 2.50GHz".to_owned());
        let row = offer_row(&o, &marks);
        assert_eq!(row[0], "★");
        assert_eq!(row[2], "140330");
        assert!(row[12].starts_with("Intel Xeon Gold 6133"), "{}", row[12]);
        assert!(row[12].chars().count() <= 23, "truncated: {}", row[12]);

        o.machine_id = Some(999);
        assert_eq!(offer_row(&o, &marks)[0], "☠");
        o.machine_id = None;
        assert_eq!(offer_row(&o, &marks)[0], "");
    }

    #[test]
    fn cheapest_ignores_offers_that_cannot_actually_be_taken() {
        let offers = vec![
            offer(1, 0.20, Some(false), None),
            offer(2, 0.30, None, Some(true)),
            offer(3, 0.40, Some(true), Some(false)),
        ];
        assert_eq!(cheapest(&offers).expect("one").id, 3);
        assert!(cheapest(&[]).is_none());
    }

    #[test]
    fn the_quote_carries_the_three_numbers_a_human_must_see() {
        let q = quote_for(&offer(7, 0.60, Some(true), Some(false)), 0.75, Some(7.73));
        assert_eq!(q.offer_id, 7);
        assert!((q.dph_total - 0.60).abs() < 1e-9);
        assert!((q.est_total_usd - 0.60).abs() < 1e-9, "one hour at $0.60");
        assert_eq!(q.credit, Some(7.73));
        assert_eq!(q.max_usd_per_hour, 0.75);
        let hours = q.burn_down_hours.expect("burn-down");
        assert!((hours - (7.73 / 0.60) as f32).abs() < 0.01, "{hours}");
    }

    #[test]
    fn an_offer_above_the_ceiling_carries_a_warning_rather_than_being_silently_rented() {
        let q = quote_for(&offer(7, 3.34, Some(true), Some(false)), 0.75, Some(7.73));
        assert!(
            q.warnings.iter().any(|w| w.contains("ceiling")),
            "{:?}",
            q.warnings
        );
    }

    #[test]
    fn unreadable_credit_produces_a_warning_and_no_fabricated_burn_down() {
        let q = quote_for(&offer(7, 0.60, Some(true), Some(false)), 0.75, None);
        assert_eq!(q.credit, None);
        assert!(!q.warnings.is_empty(), "not asking is worth saying");
    }

    #[test]
    fn an_offers_query_from_a_profile_names_only_the_profile() {
        let mut args = VastOffersArgs {
            limit: 20,
            ..VastOffersArgs::default()
        };
        args.profile = Some("two-3090s".to_string());
        // A `Ctx` is not needed for the profile branch; the function ignores it.
        let body = offer_query_for_test(&args).expect("query");
        assert_eq!(body["profile"], "two-3090s");
        assert!(body.get("gpu_names").is_none(), "one query builder: {body}");
    }

    #[test]
    fn an_explicit_query_carries_the_flags_verbatim() {
        let args = VastOffersArgs {
            profile: None,
            gpus: vec!["RTX 3090".to_string()],
            num_gpus: Some(2),
            geo: Some("EU".to_string()),
            max_price: Some(0.75),
            machine: None,
            limit: 20,
            json: false,
        };
        let body = offer_query_for_test(&args).expect("query");
        assert_eq!(body["gpu_names"][0], "RTX 3090");
        assert_eq!(body["num_gpus_min"], 2);
        assert_eq!(body["num_gpus_max"], 2);
        assert_eq!(body["max_dph"], 0.75);
        assert_eq!(body["geo"], "eu");
    }

    #[test]
    fn a_country_name_in_geo_is_refused_before_anything_is_searched() {
        let args = VastOffersArgs {
            geo: Some("Czechia".to_string()),
            limit: 20,
            ..VastOffersArgs::default()
        };
        assert!(offer_query_for_test(&args).is_err());
    }

    /// [`offer_query`] takes a `&Ctx` it never reads; building one needs the process
    /// environment, which is global and racy under the test harness.
    fn offer_query_for_test(args: &VastOffersArgs) -> anyhow::Result<serde_json::Value> {
        if let Some(p) = &args.profile {
            let id = ProfileId::parse(p)?;
            return Ok(serde_json::json!({ "profile": id.as_str() }));
        }
        let geo = args
            .geo
            .as_deref()
            .map(parse_geo)
            .transpose()?
            .unwrap_or(GeoFilter::Any);
        Ok(serde_json::json!({
            "gpu_names": args.gpus,
            "num_gpus_min": args.num_gpus.unwrap_or(1),
            "num_gpus_max": args.num_gpus.unwrap_or(1),
            "max_dph": args.max_price,
            "geo": geo,
            "limit": args.limit,
        }))
    }
}
