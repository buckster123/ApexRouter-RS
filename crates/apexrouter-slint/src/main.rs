//! OWNER: unit U-02 (crates/apexrouter-slint/src/**, except build.rs). Do not edit outside
//! that unit.
//!
//! `apexrouter-ui` — the native app. **An edge client of the same HTTP API; there is no
//! second business-logic path.** It links `apexrouter-protocol` and `apexrouter-client`
//! only, and CI asserts with `cargo tree` that it does not link `apexrouter-core`.
//!
//! **Thread model: the `tokio` main-attribute macro is never used** — CI greps this crate
//! for it, and it would take the main thread away from the event loop. `main` builds a
//! multi-thread runtime by hand, keeps it alive for the app's lifetime, and ends with
//! `ui.run()?` — Slint owns the main thread. Each callback captures `ui.as_weak()` plus
//! `rt.handle().clone()`; properties are read on the UI thread, work is `handle.spawn()`ed,
//! and results come back via `weak.upgrade_in_event_loop(move |ui| …)`.
//!
//! Every write the web UI can perform, this can perform: routes, backends, launch, rent,
//! destroy, recipes, profiles, providers, downloads. The only deferrals are cosmetic and
//! are listed in `docs/SLINT.md` — drag-to-reorder (↑/↓ here), the stacked SVG charts (a
//! bar row here) and the HF search browser (repo + quant fields here).

mod api;

slint::include_modules!();

use apexrouter_client::NodeClient;
use apexrouter_protocol::{
    Alias, CheckResult, ContainerLaunch, ContainerRuntime, CredentialSource, DeviceBudget,
    EndpointSpec, FitPlan, GeoFilter, HfFile, HfFileGroup, ImageType, KvType, LocalLlamaSpec,
    LocalVllmSpec, ManagedSpec, ModelRoute, Money, NglPlan, NodeSpec, Offer, OfferQuery,
    OfferSearchResult, ProfileId, Protocol, Provenance2, ProviderId, Recipe, RecipeId, RecipeKind,
    RentRequest, RetryPolicy, RouteFilter, RouteTarget, SamplingMode, SearchProfile, SmokeProbe,
    SplitMode, SplitPlan, TriState, UpstreamModel, UsageSummary, ValidationReport, VastInstance,
};
use api::{
    apply_backend_detail, apply_snapshot, budget_line, check_rows, control_token, control_url,
    cost_text, fit_view, fmt_money, group_hf_files, hf_rows, model_rows, now_unix, opt,
    parse_selector, probe_rows, q, registry_rows, render_logs, selector_text, split_list,
    strategy_from_index, strategy_index, target_rows, toast, usage_rows, Bridge, Store,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Entry point. Builds the runtime by hand, keeps it alive, and gives Slint the main
/// thread. The `tokio` main-attribute macro would take the main thread away from the
/// event loop, which is why it appears nowhere in this crate.
fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    let ui = AppWindow::new()?;
    let client = NodeClient::new(control_url(), control_token());
    let bridge = Bridge::new(client, rt.handle().clone(), ui.as_weak());

    let st = ui.global::<State>();
    st.set_control_url(control_url().into());
    st.set_base_url(format!("http://{}/v1", apexrouter_protocol::DEFAULT_PROXY_BIND).into());

    // The route editor's target list is the one model the UI mutates in place, because
    // reordering is the operation and a wholesale replace would lose the scroll position.
    let targets: Rc<VecModel<TargetRow>> = Rc::new(VecModel::default());
    st.set_re_targets(ModelRc::from(targets.clone()));

    wire_shell(&ui, &bridge);
    wire_routes(&ui, &bridge, &targets);
    wire_backends(&ui, &bridge);
    wire_launch(&ui, &bridge);
    wire_fleet(&ui, &bridge);
    wire_catalog(&ui, &bridge);
    wire_providers(&ui, &bridge);
    wire_doctor(&ui, &bridge);

    // First paint from REST, then the WS takes over. Same order as the web UI.
    bridge.refresh();
    bridge.refresh_local_models();
    bridge.spawn_ws();

    // Relative timestamps and the boot elapsed counter, kept honest once a second.
    let ticker = slint::Timer::default();
    {
        let weak = bridge.ui();
        let store = bridge.store();
        ticker.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(1),
            move || {
                if let Some(ui) = weak.upgrade() {
                    api::tick(&ui, &store);
                }
            },
        );
    }

    ui.run()?;
    // The runtime outlives every callback because it is dropped only here.
    drop(rt);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Small request shapes reused by several callbacks
// ─────────────────────────────────────────────────────────────────────────────

/// `POST <path>` with an empty body; the response is discarded.
fn post_action(bridge: &Bridge, what: &'static str, path: String, ok: String) {
    bridge.act(what, move |c| async move {
        let _: serde_json::Value = c.post(&path, &serde_json::json!({})).await?;
        anyhow::Ok(ok)
    });
}

/// `DELETE <path>`.
fn delete_action(bridge: &Bridge, what: &'static str, path: String, ok: String) {
    bridge.act(what, move |c| async move {
        c.delete(&path).await?;
        anyhow::Ok(ok)
    });
}

/// The KV type the Launch tab's combo is on.
fn kv_from_index(i: i32) -> KvType {
    match i {
        1 => KvType::Bf16,
        2 => KvType::Q8_0,
        3 => KvType::Q5_1,
        4 => KvType::Q5_0,
        5 => KvType::Q4_1,
        6 => KvType::Q4_0,
        7 => KvType::Iq4Nl,
        8 => KvType::F32,
        _ => KvType::F16,
    }
}

/// The split mode the Launch tab's combo is on.
fn split_from_index(i: i32) -> SplitMode {
    match i {
        0 => SplitMode::None,
        2 => SplitMode::Row,
        3 => SplitMode::Tensor,
        _ => SplitMode::Layer,
    }
}

/// The wire spelling of a split mode, for the `/v1/fit` query string. `{:?}` would send
/// `Layer`, and the serde representation is snake_case.
fn split_mode_flag(m: SplitMode) -> &'static str {
    match m {
        SplitMode::None => "none",
        SplitMode::Layer => "layer",
        SplitMode::Row => "row",
        SplitMode::Tensor => "tensor",
    }
}

/// The sampling preset the Launch tab's combo is on.
fn mode_from_index(i: i32) -> SamplingMode {
    match i {
        1 => SamplingMode::Coding,
        2 => SamplingMode::Nonthinking,
        3 => SamplingMode::Raw,
        _ => SamplingMode::Thinking,
    }
}

/// Flash attention is a tri-state, not a bool: `auto` is not the same as `off`.
fn tristate_from_index(i: i32) -> TriState {
    match i {
        1 => TriState::On,
        2 => TriState::Off,
        _ => TriState::Auto,
    }
}

/// The geo filter the profile editor's combo is on.
fn geo_from_index(i: i32) -> GeoFilter {
    match i {
        1 => GeoFilter::EuNordic,
        2 => GeoFilter::Eu,
        3 => GeoFilter::Us,
        _ => GeoFilter::Any,
    }
}

/// The inverse of [`geo_from_index`]; a `Codes(...)` filter has no combo entry and reads
/// as `any` rather than silently becoming one.
fn geo_to_index(g: &GeoFilter) -> i32 {
    match g {
        GeoFilter::EuNordic => 1,
        GeoFilter::Eu => 2,
        GeoFilter::Us => 3,
        _ => 0,
    }
}

/// The image type the profile editor's combo is on.
fn image_type_from_index(i: i32) -> ImageType {
    match i {
        1 => ImageType::Builder,
        2 => ImageType::Vllm,
        3 => ImageType::Studio,
        _ => ImageType::Prebuilt,
    }
}

/// The inverse of [`image_type_from_index`].
fn image_type_to_index(t: ImageType) -> i32 {
    match t {
        ImageType::Prebuilt => 0,
        ImageType::Builder => 1,
        ImageType::Vllm => 2,
        ImageType::Studio => 3,
    }
}

/// A string field parsed as a `u32`, or `None` when it is blank or not a number.
fn opt_u32(s: &str) -> Option<u32> {
    opt(s).and_then(|v| v.parse::<u32>().ok())
}

/// A string field parsed as an `f64`.
fn opt_f64(s: &str) -> Option<f64> {
    opt(s).and_then(|v| v.parse::<f64>().ok())
}

/// The alias the user typed, or the daemon's current default when the field is blank.
fn alias_or_default(ui: &AppWindow, typed: &str) -> Option<Alias> {
    let st = ui.global::<State>();
    let text = if typed.trim().is_empty() {
        st.get_default_alias().to_string()
    } else {
        typed.trim().to_string()
    };
    Alias::parse(&text).ok()
}

// ─────────────────────────────────────────────────────────────────────────────
// Shell, router bar, rig strip
// ─────────────────────────────────────────────────────────────────────────────

/// Toast dismissal, the global refresh, the default-alias dropdown, config reload, the
/// rig rescan and the rig-strip click-through.
fn wire_shell(ui: &AppWindow, bridge: &Bridge) {
    let st = ui.global::<State>();

    {
        let weak = ui.as_weak();
        st.on_dismiss_toast(move || {
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<State>();
                st.set_toast("".into());
                st.set_toast_level(0);
            }
        });
    }

    {
        let b = bridge.clone();
        st.on_refresh_all(move || {
            b.refresh();
            b.refresh_local_models();
        });
    }

    {
        let b = bridge.clone();
        st.on_set_default_alias(move |alias| {
            let alias = alias.to_string();
            if alias.is_empty() {
                return;
            }
            let ok = format!("default now points at `{alias}`");
            b.act("set default alias", move |c| async move {
                let _: serde_json::Value = c
                    .post("/v1/routes/default", &serde_json::json!({ "alias": alias }))
                    .await?;
                anyhow::Ok(ok)
            });
        });
    }

    {
        let b = bridge.clone();
        st.on_reload_config(move || {
            post_action(
                &b,
                "reload",
                "/v1/reload".to_string(),
                "config and routes reparsed".to_string(),
            );
        });
    }

    {
        let b = bridge.clone();
        st.on_rescan_rig(move || {
            b.act("rescan", |c| async move {
                let _: serde_json::Value = c
                    .post("/v1/rig/rescan?builds=1&models=1", &serde_json::json!({}))
                    .await?;
                anyhow::Ok("rig rescanned".to_string())
            });
            b.refresh_local_models();
        });
    }

    {
        let weak = ui.as_weak();
        let store = bridge.store();
        st.on_filter_by_device(move |device| {
            if let Some(ui) = weak.upgrade() {
                if let Ok(mut s) = store.lock() {
                    s.device_filter = device.to_string();
                }
                apply_snapshot(&ui, &store);
                ui.global::<State>().set_screen(2);
            }
        });
    }

    {
        let b = bridge.clone();
        st.on_cancel_request(move |id| {
            let id = id.to_string();
            post_action(
                &b,
                "cancel request",
                format!("/v1/requests/{}/cancel", q(&id)),
                format!("cancelled {id}"),
            );
        });
    }

    {
        let b = bridge.clone();
        st.on_cancel_job(move |id| {
            let id = id.to_string();
            post_action(
                &b,
                "cancel job",
                format!("/v1/jobs/{}/cancel", q(&id)),
                format!("cancelled job {id}"),
            );
        });
    }

    {
        let b = bridge.clone();
        let weak = ui.as_weak();
        st.on_alert_act(move |id, action| {
            let (id, action) = (id.to_string(), action.to_string());
            match action.as_str() {
                "grant" | "deny" => post_action(
                    &b,
                    "approval",
                    format!("/v1/approvals/{}/{}", q(&id), action),
                    format!("approval {action}ed"),
                ),
                "destroy" | "reconcile" => {
                    if let Some(ui) = weak.upgrade() {
                        ui.global::<State>().set_screen(4);
                        toast(
                            &ui,
                            "the instance is on the Fleet screen — destroy is always visible there",
                            2,
                        );
                    }
                }
                other => {
                    if let Some(ui) = weak.upgrade() {
                        toast(&ui, &format!("no UI verb for `{other}` yet"), 2);
                    }
                }
            }
        });
    }

    {
        let weak = ui.as_weak();
        st.on_open_web_ui(move || {
            if let Some(ui) = weak.upgrade() {
                let url = ui.global::<State>().get_control_url();
                toast(&ui, &format!("the web UI is served at {url}/"), 5);
            }
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Routes
// ─────────────────────────────────────────────────────────────────────────────

/// Read the editor's fields back into a `ModelRoute`. The single place the draft becomes
/// a protocol type, so a validation failure has one message and one origin.
fn draft_route(ui: &AppWindow, targets: &VecModel<TargetRow>) -> anyhow::Result<ModelRoute> {
    let st = ui.global::<State>();
    let alias = Alias::parse(st.get_re_alias().trim())?;
    let mut out = Vec::new();
    for row in targets.iter() {
        out.push(RouteTarget {
            backend: parse_selector(row.backend.as_str())?,
            model: opt(row.model.as_str()),
            weight: row.weight.max(1) as u32,
        });
    }
    Ok(ModelRoute {
        alias,
        targets: out,
        strategy: strategy_from_index(st.get_re_strategy()),
        filter: RouteFilter {
            require_tags: split_list(st.get_re_require_tags().as_str()),
            exclude_tags: split_list(st.get_re_exclude_tags().as_str()),
            max_cost_per_mtok: opt_f64(st.get_re_max_cost().as_str()).map(Money::from_usd),
            min_ctx: opt_u32(st.get_re_min_ctx().as_str()),
            require_vision: st.get_re_require_vision(),
            require_tools: st.get_re_require_tools(),
        },
        retry: RetryPolicy {
            attempts: st.get_re_attempts().clamp(1, 255) as u8,
            failover: st.get_re_failover(),
            honor_retry_after: st.get_re_honor_retry(),
        },
        is_default: st.get_re_is_default(),
        description: opt(st.get_re_description().as_str()),
    })
}

/// Load one alias out of the last snapshot and into the editor.
fn load_route(
    ui: &AppWindow,
    store: &Arc<Mutex<Store>>,
    targets: &VecModel<TargetRow>,
    alias: &str,
) {
    let st = ui.global::<State>();
    let guard = match store.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let route = match guard.routes().iter().find(|r| r.alias.as_str() == alias) {
        Some(r) => r.clone(),
        None => return,
    };
    let backends = guard.backends().to_vec();
    drop(guard);

    st.set_route_sel(alias.into());
    st.set_route_editing(true);
    st.set_re_alias(route.alias.to_string().into());
    st.set_re_strategy(strategy_index(route.strategy));
    st.set_re_failover(route.retry.failover);
    st.set_re_honor_retry(route.retry.honor_retry_after);
    st.set_re_attempts(route.retry.attempts as i32);
    st.set_re_is_default(route.is_default);
    st.set_re_description(route.description.clone().unwrap_or_default().into());
    st.set_re_require_tags(route.filter.require_tags.join(", ").into());
    st.set_re_exclude_tags(route.filter.exclude_tags.join(", ").into());
    st.set_re_min_ctx(
        route
            .filter
            .min_ctx
            .map(|c| c.to_string())
            .unwrap_or_default()
            .into(),
    );
    st.set_re_max_cost(
        route
            .filter
            .max_cost_per_mtok
            .map(|m| format!("{:.6}", m.as_usd()))
            .unwrap_or_default()
            .into(),
    );
    st.set_re_require_vision(route.filter.require_vision);
    st.set_re_require_tools(route.filter.require_tools);
    st.set_route_test_result("".into());
    st.set_route_issues(ModelRc::new(VecModel::<SharedString>::default()));

    targets.set_vec(target_rows(&route, &backends));
}

/// Alias list, editor, target reordering, save/delete/test/validate.
fn wire_routes(ui: &AppWindow, bridge: &Bridge, targets: &Rc<VecModel<TargetRow>>) {
    let st = ui.global::<State>();

    {
        let weak = ui.as_weak();
        let store = bridge.store();
        let targets = targets.clone();
        st.on_route_select(move |alias| {
            if let Some(ui) = weak.upgrade() {
                load_route(&ui, &store, &targets, alias.as_str());
            }
        });
    }

    {
        let weak = ui.as_weak();
        let targets = targets.clone();
        st.on_route_new(move || {
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<State>();
                st.set_route_sel("".into());
                st.set_route_editing(true);
                st.set_re_alias("".into());
                st.set_re_strategy(0);
                st.set_re_failover(true);
                st.set_re_honor_retry(true);
                st.set_re_attempts(2);
                st.set_re_is_default(false);
                st.set_re_description("".into());
                st.set_re_require_tags("".into());
                st.set_re_exclude_tags("".into());
                st.set_re_min_ctx("".into());
                st.set_re_max_cost("".into());
                st.set_re_require_vision(false);
                st.set_re_require_tools(false);
                st.set_route_test_result("".into());
                targets.set_vec(Vec::<TargetRow>::new());
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let targets = targets.clone();
        st.on_route_save(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            match draft_route(&ui, &targets) {
                Ok(route) => {
                    let alias = route.alias.to_string();
                    let ok = format!("route `{alias}` saved — hot, no restart");
                    b.act("save route", move |c| async move {
                        let _: serde_json::Value =
                            c.put(&format!("/v1/routes/{}", q(&alias)), &route).await?;
                        anyhow::Ok(ok)
                    });
                }
                Err(e) => toast(&ui, &format!("cannot save: {e}"), 3),
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_route_delete(move || {
            if let Some(ui) = weak.upgrade() {
                let alias = ui.global::<State>().get_re_alias().to_string();
                if alias.is_empty() {
                    return;
                }
                delete_action(
                    &b,
                    "delete route",
                    format!("/v1/routes/{}", q(&alias)),
                    format!("route `{alias}` deleted"),
                );
                ui.global::<State>().set_route_editing(false);
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_route_test(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let alias = ui.global::<State>().get_re_alias().to_string();
            if alias.is_empty() {
                toast(&ui, "name the alias first", 2);
                return;
            }
            b.fetch(
                "route test",
                move |c| async move {
                    let probe: SmokeProbe = c
                        .post(
                            &format!("/v1/routes/{}/test", q(&alias)),
                            &serde_json::json!({}),
                        )
                        .await?;
                    anyhow::Ok(probe)
                },
                |ui, _store, probe| {
                    let st = ui.global::<State>();
                    st.set_route_test_level(if probe.ok { 1 } else { 4 });
                    st.set_route_test_result(
                        format!(
                            "{} · {} ms · ttft {} · {} tok/s · {}",
                            probe.name,
                            probe.ms,
                            probe
                                .ttft_ms
                                .map(|m| format!("{m} ms"))
                                .unwrap_or_else(|| "—".into()),
                            probe
                                .tok_per_s
                                .map(|t| format!("{t:.2}"))
                                .unwrap_or_else(|| "—".into()),
                            probe.detail
                        )
                        .into(),
                    );
                },
            );
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let store = bridge.store();
        let targets = targets.clone();
        st.on_route_validate(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            // Validate the table as it *would* be after this save, not as it is on disk.
            let mut table: Vec<ModelRoute> = match store.lock() {
                Ok(g) => g.routes().to_vec(),
                Err(_) => Vec::new(),
            };
            if let Ok(draft) = draft_route(&ui, &targets) {
                match table.iter_mut().find(|r| r.alias == draft.alias) {
                    Some(slot) => *slot = draft,
                    None => table.push(draft),
                }
            }
            b.fetch(
                "validate",
                move |c| async move {
                    let report: ValidationReport = c.post("/v1/routes/validate", &table).await?;
                    anyhow::Ok(report)
                },
                |ui, _store, report| {
                    let lines: Vec<SharedString> = report
                        .issues
                        .iter()
                        .map(|i| {
                            SharedString::from(format!(
                                "{:?} {}: {}{}",
                                i.severity,
                                i.field,
                                i.message,
                                i.fix
                                    .as_ref()
                                    .map(|f| format!("  → {f}"))
                                    .unwrap_or_default()
                            ))
                        })
                        .collect();
                    let st = ui.global::<State>();
                    st.set_route_issues(ModelRc::new(VecModel::from(lines)));
                    toast(
                        ui,
                        if report.ok {
                            "table validates"
                        } else {
                            "table has issues — see the banner"
                        },
                        if report.ok { 1 } else { 2 },
                    );
                },
            );
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_route_make_default(move || {
            if let Some(ui) = weak.upgrade() {
                let alias = ui.global::<State>().get_re_alias().to_string();
                let ok = format!("default now points at `{alias}`");
                b.act("set default alias", move |c| async move {
                    let _: serde_json::Value = c
                        .post("/v1/routes/default", &serde_json::json!({ "alias": alias }))
                        .await?;
                    anyhow::Ok(ok)
                });
            }
        });
    }

    {
        let weak = ui.as_weak();
        let targets = targets.clone();
        st.on_target_add(move || {
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<State>();
                let sel = st.get_re_new_backend().to_string();
                if sel.trim().is_empty() {
                    toast(&ui, "type a selector: id:… / tag:… / glob:…", 2);
                    return;
                }
                match parse_selector(&sel) {
                    Ok(parsed) => {
                        targets.push(TargetRow {
                            backend: selector_text(&parsed).into(),
                            model: st.get_re_new_model(),
                            weight: st.get_re_new_weight().max(1),
                            resolves: "saved on the next Save".into(),
                            level: 0,
                        });
                        st.set_re_new_backend("".into());
                        st.set_re_new_model("".into());
                        st.set_re_new_weight(1);
                    }
                    Err(e) => toast(&ui, &format!("bad selector: {e}"), 3),
                }
            }
        });
    }

    {
        let targets = targets.clone();
        st.on_target_move(move |index, delta| {
            let len = targets.row_count() as i32;
            let to = index + delta;
            if index < 0 || index >= len || to < 0 || to >= len {
                return;
            }
            let mut rows: Vec<TargetRow> = targets.iter().collect();
            rows.swap(index as usize, to as usize);
            targets.set_vec(rows);
        });
    }

    {
        let targets = targets.clone();
        st.on_target_remove(move |index| {
            if index >= 0 && (index as usize) < targets.row_count() {
                targets.remove(index as usize);
            }
        });
    }

    {
        let targets = targets.clone();
        st.on_target_edit(move |index, backend, model, weight| {
            if index < 0 || index as usize >= targets.row_count() {
                return;
            }
            let i = index as usize;
            if let Some(mut row) = targets.row_data(i) {
                row.backend = backend;
                row.model = model;
                row.weight = weight.max(1);
                targets.set_row_data(i, row);
            }
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Backends
// ─────────────────────────────────────────────────────────────────────────────

/// Selection, the six per-backend verbs, alias binding, node registration and the log
/// tail.
fn wire_backends(ui: &AppWindow, bridge: &Bridge) {
    let st = ui.global::<State>();

    {
        let weak = ui.as_weak();
        let store = bridge.store();
        st.on_backend_select(move |id| {
            if let Some(ui) = weak.upgrade() {
                ui.global::<State>().set_backend_sel(id.clone());
                if let Ok(guard) = store.lock() {
                    if let Some(b) = guard
                        .backends()
                        .iter()
                        .find(|b| b.id.as_str() == id.as_str())
                    {
                        apply_backend_detail(&ui, b);
                    }
                }
            }
        });
    }

    {
        let b = bridge.clone();
        st.on_backend_probe(move |id| {
            let id = id.to_string();
            post_action(
                &b,
                "probe",
                format!("/v1/backends/{}/probe", q(&id)),
                format!("probed {id}"),
            );
        });
    }

    {
        let b = bridge.clone();
        st.on_backend_set_enabled(move |id, enabled| {
            let id = id.to_string();
            let verb = if enabled { "enable" } else { "disable" };
            post_action(
                &b,
                "enable/disable",
                format!("/v1/backends/{}/{verb}", q(&id)),
                format!("{id} {verb}d"),
            );
        });
    }

    {
        let b = bridge.clone();
        st.on_backend_drain(move |id| {
            let id = id.to_string();
            post_action(
                &b,
                "drain",
                format!("/v1/backends/{}/drain", q(&id)),
                format!("{id} is draining — in-flight requests finish first"),
            );
        });
    }

    {
        let b = bridge.clone();
        st.on_backend_stop(move |id| {
            let id = id.to_string();
            post_action(
                &b,
                "stop",
                format!("/v1/endpoints/{}/stop", q(&id)),
                format!("{id} stopped"),
            );
        });
    }

    {
        let b = bridge.clone();
        st.on_backend_restart(move |id| {
            let id = id.to_string();
            post_action(
                &b,
                "restart",
                format!("/v1/endpoints/{}/restart", q(&id)),
                format!("{id} restarting"),
            );
        });
    }

    {
        let b = bridge.clone();
        st.on_backend_destroy(move |id| {
            let id = id.to_string();
            delete_action(
                &b,
                "destroy",
                format!("/v1/endpoints/{}", q(&id)),
                format!("{id} destroyed"),
            );
        });
    }

    {
        let b = bridge.clone();
        st.on_backend_forget(move |id| {
            let id = id.to_string();
            delete_action(
                &b,
                "forget",
                format!("/v1/backends/{}", q(&id)),
                format!("{id} forgotten"),
            );
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_backend_bind(move |alias, id| {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let (alias, id) = (alias.to_string(), id.to_string());
            if alias.is_empty() || id.is_empty() {
                toast(&ui, "pick an alias first", 2);
                return;
            }
            let ok = format!("`{alias}` now points at {id}");
            b.act("bind", move |c| async move {
                let _: serde_json::Value = c
                    .post(
                        &format!("/v1/routes/{}/swap", q(&alias)),
                        &serde_json::json!({ "to": id, "mode": "hot" }),
                    )
                    .await?;
                anyhow::Ok(ok)
            });
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_backend_register(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();
            let url = st.get_node_url().to_string();
            if url.trim().is_empty() {
                toast(&ui, "a node needs a base URL", 2);
                return;
            }
            let label = match opt(st.get_node_label().as_str()) {
                Some(l) => l,
                None => url.clone(),
            };
            let spec = NodeSpec {
                base_url: url.clone(),
                credential: CredentialSource::None,
                label,
                declared_models: split_list(st.get_node_models().as_str()),
                protocol: Protocol::OpenAi,
            };
            let ok = format!("registered {url}");
            b.act("register node", move |c| async move {
                let _: serde_json::Value = c.post("/v1/backends", &spec).await?;
                anyhow::Ok(ok)
            });
        });
    }

    {
        let b = bridge.clone();
        st.on_log_open(move |id| {
            let id = id.to_string();
            if id.is_empty() {
                return;
            }
            b.follow_logs(
                format!("/v1/backends/{}/logs?tail=200&follow=1", q(&id)),
                id,
            );
        });
    }

    {
        let weak = ui.as_weak();
        let store = bridge.store();
        st.on_log_clear(move || {
            if let Some(ui) = weak.upgrade() {
                if let Ok(mut s) = store.lock() {
                    s.log_buffer.clear();
                }
                render_logs(&ui, &store);
            }
        });
    }

    {
        let weak = ui.as_weak();
        let store = bridge.store();
        st.on_log_filter_changed(move |_| {
            if let Some(ui) = weak.upgrade() {
                render_logs(&ui, &store);
            }
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Launch
// ─────────────────────────────────────────────────────────────────────────────

/// Build the llama.cpp spec from what is on screen. Returns `Err` with a sentence the
/// operator can act on rather than a silent no-op.
fn llama_spec(ui: &AppWindow, store: &Arc<Mutex<Store>>) -> anyhow::Result<LocalLlamaSpec> {
    let st = ui.global::<State>();
    let guard = store
        .lock()
        .map_err(|_| anyhow::anyhow!("store is poisoned"))?;

    let index = st.get_local_model_index();
    anyhow::ensure!(index >= 0, "pick a model first");
    let model = guard
        .local_models
        .get(index as usize)
        .ok_or_else(|| anyhow::anyhow!("that model is no longer in the list — rescan"))?;
    let model_path = model
        .primary_path()
        .ok_or_else(|| anyhow::anyhow!("that model has no file on disk"))?
        .to_string();

    let builds = st.get_build_names();
    let build_index = st.get_build_index();
    let build_id = builds
        .row_data(build_index.max(0) as usize)
        .ok_or_else(|| anyhow::anyhow!("pick a llama.cpp build first"))?;
    let build = apexrouter_protocol::BuildId::parse(build_id.as_str())?;

    let devices = guard.checked_devices.clone();
    let alias = alias_or_default(ui, st.get_ll_alias().as_str())
        .ok_or_else(|| anyhow::anyhow!("that alias is not a valid id"))?;

    Ok(LocalLlamaSpec {
        build,
        model_path,
        mmproj: opt(st.get_ll_mmproj().as_str()),
        alias_flag: alias.to_string(),
        host: st.get_ll_host().to_string(),
        port: opt_u32(st.get_ll_port().as_str()).and_then(|p| u16::try_from(p).ok()),
        ctx: Some(st.get_ll_ctx().max(0) as u32),
        parallel: Some(st.get_ll_parallel().max(1) as u32),
        kv_type: Some(kv_from_index(st.get_ll_kv())),
        ngl: NglPlan::Auto,
        split: SplitPlan {
            devices,
            mode: split_from_index(st.get_ll_split_mode()),
            main_gpu: opt_u32(st.get_ll_main_gpu().as_str()),
            tensor_split: opt(st.get_ll_tensor_split().as_str()).map(|s| {
                s.split(',')
                    .filter_map(|p| p.trim().parse::<f32>().ok())
                    .collect()
            }),
        },
        mode: mode_from_index(st.get_ll_mode()),
        flash_attn: Some(tristate_from_index(st.get_ll_flash())),
        api_key: None,
        extra_args: st
            .get_ll_extra()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect(),
    })
}

/// Build the vLLM spec from what is on screen.
fn vllm_spec(ui: &AppWindow, store: &Arc<Mutex<Store>>) -> anyhow::Result<LocalVllmSpec> {
    let st = ui.global::<State>();
    let model_id = opt(st.get_vl_model().as_str())
        .ok_or_else(|| anyhow::anyhow!("a vLLM launch needs a model id"))?;
    let devices = store
        .lock()
        .map(|g| g.checked_devices.clone())
        .unwrap_or_default();
    Ok(LocalVllmSpec {
        bin: st.get_vl_bin().to_string(),
        model_id,
        tp: Some(st.get_vl_tp().max(1) as u32),
        ctx: Some(st.get_vl_ctx().max(0) as u32),
        quantization: opt(st.get_vl_quant().as_str()),
        kv_cache_dtype: opt(st.get_vl_kv_dtype().as_str()),
        enforce_eager: st.get_vl_eager(),
        reasoning_parser: None,
        gpu_util: opt_f64(st.get_vl_gpu_util().as_str()).map(|v| v as f32),
        max_num_seqs: match st.get_vl_max_seqs() {
            n if n > 0 => Some(n as u32),
            _ => None,
        },
        trust_remote: st.get_vl_trust(),
        chunked_prefill: st.get_vl_chunked(),
        host: st.get_vl_host().to_string(),
        port: opt_u32(st.get_vl_port().as_str()).and_then(|p| u16::try_from(p).ok()),
        devices,
        extra_args: st
            .get_vl_extra()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect(),
    })
}

/// `POST /v1/endpoints?no_wait=true&alias=…`. `no_wait` on purpose: the drawer becomes
/// the boot view and the WS drives it, so the window never blocks on a model load.
fn start_endpoint(bridge: &Bridge, ui: &AppWindow, spec: EndpointSpec, alias: Option<Alias>) {
    let store = bridge.store();
    if let Ok(mut s) = store.lock() {
        s.boot_started = Some(now_unix());
        s.log_buffer.clear();
    }
    let st = ui.global::<State>();
    st.set_boot_active(true);
    st.set_boot_phase("requested".into());
    st.set_boot_level(5);
    st.set_boot_pct(0.02);
    st.set_launch_open(true);

    let path = match &alias {
        Some(a) => format!("/v1/endpoints?no_wait=true&alias={}", q(a.as_str())),
        None => "/v1/endpoints?no_wait=true".to_string(),
    };
    bridge.act("launch", move |c| async move {
        let value: serde_json::Value = c.post(&path, &spec).await?;
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("(no id)")
            .to_string();
        anyhow::Ok(format!("launch accepted: {id}"))
    });
}

/// Save a spec as a recipe: `POST /v1/recipes`.
fn save_recipe(bridge: &Bridge, id: RecipeId, label: String, kind: RecipeKind) {
    let ok = format!("recipe `{id}` saved");
    let recipe = Recipe {
        id,
        label,
        description: None,
        kind,
        provenance: Provenance2 {
            discovered_at_unix: now_unix(),
            size_bytes: None,
            source: "slint-ui".to_string(),
            fit: None,
        },
        created_at_unix: now_unix(),
        updated_at_unix: now_unix(),
    };
    bridge.act("save recipe", move |c| async move {
        let _: serde_json::Value = c.post("/v1/recipes", &recipe).await?;
        anyhow::Ok(ok)
    });
}

/// The three tabs, the device checkboxes, the live fit and the money path.
fn wire_launch(ui: &AppWindow, bridge: &Bridge) {
    let st = ui.global::<State>();

    {
        let b = bridge.clone();
        st.on_load_local_models(move || b.refresh_local_models());
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let store = bridge.store();
        st.on_local_model_select(move |index| {
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<State>();
                st.set_local_model_index(index);
                // Seed the alias from the model name, the way a human would.
                if st.get_ll_alias().is_empty() {
                    if let Ok(guard) = store.lock() {
                        if let Some(m) = guard.local_models.get(index.max(0) as usize) {
                            let slug: String = m
                                .name
                                .to_lowercase()
                                .chars()
                                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                                .collect();
                            if let Ok(a) = Alias::parse(slug.trim_matches('-')) {
                                st.set_ll_alias(a.to_string().into());
                            }
                        }
                    }
                }
                st.invoke_fit_refresh();
                let _ = &b;
            }
        });
    }

    {
        let weak = ui.as_weak();
        let store = bridge.store();
        st.on_device_toggle(move |index, checked| {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();
            let devices = st.get_devices();
            let row = match devices.row_data(index.max(0) as usize) {
                Some(r) => r,
                None => return,
            };
            let token = row.device.to_string();
            if let Ok(mut s) = store.lock() {
                s.checked_devices.retain(|d| d != &token);
                if checked {
                    s.checked_devices.push(token);
                }
            }
            apply_snapshot(&ui, &store);
            st.invoke_fit_refresh();
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let store = bridge.store();
        st.on_fit_refresh(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();
            let index = st.get_local_model_index();
            let (model_path, devices, budget) = {
                let guard = match store.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                let path = guard
                    .local_models
                    .get(index.max(0) as usize)
                    .and_then(|m| m.primary_path().map(|p| p.to_string()));
                let budgets: Vec<DeviceBudget> = guard
                    .snapshot
                    .as_ref()
                    .map(|s| {
                        s.rig
                            .gpus
                            .iter()
                            .filter(|g| guard.checked_devices.iter().any(|d| d == &g.device))
                            .map(|g| DeviceBudget {
                                device: g.device.clone(),
                                free_mb: g.vram_free_mb.min(g.vram_total_mb),
                                reserved_mb: g.reserved_mb,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (path, guard.checked_devices.join(","), budgets)
            };
            st.set_fit_budget(budget_line(&budget, 512).into());
            let model_path = match model_path {
                Some(p) => p,
                None => {
                    st.set_fit_verdict("pick a model".into());
                    st.set_fit_level(0);
                    return;
                }
            };
            let builds = st.get_build_names();
            let build = builds
                .row_data(st.get_build_index().max(0) as usize)
                .map(|s| s.to_string())
                .unwrap_or_default();
            let path = format!(
                "/v1/fit?model={}&ctx={}&parallel={}&kv={}&devices={}&build={}&split_mode={}{}{}",
                q(&model_path),
                st.get_ll_ctx().max(0),
                st.get_ll_parallel().max(1),
                kv_from_index(st.get_ll_kv()).as_flag(),
                q(&devices),
                q(&build),
                split_mode_flag(split_from_index(st.get_ll_split_mode())),
                match opt(st.get_ll_main_gpu().as_str()) {
                    Some(g) => format!("&main_gpu={}", q(&g)),
                    None => String::new(),
                },
                match opt(st.get_ll_tensor_split().as_str()) {
                    Some(t) => format!("&tensor_split={}", q(&t)),
                    None => String::new(),
                }
            );
            b.fetch(
                "fit",
                move |c| async move {
                    let plan: FitPlan = c.get(&path).await?;
                    anyhow::Ok(plan)
                },
                |ui, _store, plan| {
                    let st = ui.global::<State>();
                    let total = (plan.weights_mb + plan.kv_mb + plan.compute_mb)
                        .saturating_add(plan.headroom_mb.max(0) as u64);
                    let (verdict, level, w, k, cmp, caption, why) = fit_view(&plan, total);
                    st.set_fit_verdict(verdict.into());
                    st.set_fit_level(level);
                    st.set_fit_weights_frac(w);
                    st.set_fit_kv_frac(k);
                    st.set_fit_compute_frac(cmp);
                    st.set_fit_caption(caption.into());
                    st.set_fit_why(ModelRc::new(VecModel::from(
                        why.into_iter()
                            .map(SharedString::from)
                            .collect::<Vec<SharedString>>(),
                    )));
                },
            );
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let store = bridge.store();
        st.on_launch_local(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            match llama_spec(&ui, &store) {
                Ok(spec) => {
                    let alias = Alias::parse(&spec.alias_flag).ok();
                    start_endpoint(&b, &ui, EndpointSpec::LocalLlama(spec), alias);
                }
                Err(e) => toast(&ui, &format!("cannot launch: {e}"), 3),
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let store = bridge.store();
        st.on_launch_local_recipe(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            match llama_spec(&ui, &store) {
                Ok(spec) => {
                    let suggested = EndpointSpec::LocalLlama(spec.clone()).suggested_id();
                    match RecipeId::parse(&suggested) {
                        Ok(id) => {
                            save_recipe(&b, id, spec.alias_flag.clone(), RecipeKind::Local(spec))
                        }
                        Err(e) => toast(&ui, &format!("cannot name the recipe: {e}"), 3),
                    }
                }
                Err(e) => toast(&ui, &format!("cannot save: {e}"), 3),
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let store = bridge.store();
        st.on_launch_vllm(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            match vllm_spec(&ui, &store) {
                Ok(spec) => {
                    let alias = alias_or_default(&ui, ui.global::<State>().get_vl_alias().as_str());
                    start_endpoint(&b, &ui, EndpointSpec::LocalVllm(spec), alias);
                }
                Err(e) => toast(&ui, &format!("cannot launch: {e}"), 3),
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let store = bridge.store();
        st.on_launch_vllm_recipe(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            match vllm_spec(&ui, &store) {
                Ok(spec) => {
                    let suggested = EndpointSpec::LocalVllm(spec.clone()).suggested_id();
                    match RecipeId::parse(&suggested) {
                        Ok(id) => {
                            save_recipe(&b, id, spec.model_id.clone(), RecipeKind::LocalVllm(spec))
                        }
                        Err(e) => toast(&ui, &format!("cannot name the recipe: {e}"), 3),
                    }
                }
                Err(e) => toast(&ui, &format!("cannot save: {e}"), 3),
            }
        });
    }

    // ── The market ────────────────────────────────────────────────────────
    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let store = bridge.store();
        st.on_offers_search(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();
            let index = st.get_rent_profile_index();
            let profile = match store.lock() {
                Ok(g) => g
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.profiles.get(index.max(0) as usize).cloned()),
                Err(_) => None,
            };
            let query = match profile {
                Some(p) => OfferQuery {
                    gpu_names: p.gpu_names.clone(),
                    num_gpus_min: p.num_gpus_min,
                    num_gpus_max: p.num_gpus_max,
                    max_dph: p.max_dph.map(|m| m.as_usd()),
                    min_reliability: Some(f64::from(p.min_reliability)),
                    min_inet_down: Some(f64::from(p.min_inet_down)),
                    min_disk_gb: Some(p.min_disk_gb),
                    min_cuda: p.min_cuda.map(f64::from),
                    geo: p.geo.clone(),
                    verified: None,
                    limit: 60,
                    order: vec![("dph_total".into(), "asc".into())],
                    extra: p.extra.clone(),
                },
                None => {
                    toast(&ui, "pick a search profile — Catalog authors them", 2);
                    return;
                }
            };
            b.fetch(
                "offer search",
                move |c| async move {
                    let res: OfferSearchResult = c.post("/v1/vast/offers/search", &query).await?;
                    anyhow::Ok(res)
                },
                |ui, store, res| {
                    let st = ui.global::<State>();
                    st.set_offers(ModelRc::new(VecModel::from(api::offer_rows(&res.offers))));
                    st.set_gpu_vocabulary(ModelRc::new(VecModel::from(
                        res.gpu_name_vocabulary
                            .iter()
                            .map(SharedString::from)
                            .collect::<Vec<SharedString>>(),
                    )));
                    st.set_rent_search_note(
                        if res.relaxations.is_empty() {
                            format!("{} offers", res.offers.len())
                        } else {
                            format!(
                                "{} offers after relaxing: {}",
                                res.offers.len(),
                                res.relaxations.join("; ")
                            )
                        }
                        .into(),
                    );
                    st.set_offer_index(-1);
                    if let Ok(mut s) = store.lock() {
                        s.offers = res.offers;
                    }
                },
            );
        });
    }

    {
        let weak = ui.as_weak();
        st.on_offer_select(move |index| {
            if let Some(ui) = weak.upgrade() {
                ui.global::<State>().set_offer_index(index);
                ui.global::<State>().invoke_rent_cost_refresh();
            }
        });
    }

    {
        let weak = ui.as_weak();
        let store = bridge.store();
        st.on_rent_profile_select(move |index| {
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<State>();
                st.set_rent_profile_index(index);
                if let Ok(g) = store.lock() {
                    if let Some(p) = g
                        .snapshot
                        .as_ref()
                        .and_then(|s| s.profiles.get(index.max(0) as usize))
                    {
                        st.set_rent_disk_gb(p.min_disk_gb.min(i32::MAX as u32) as i32);
                        if let Some(m) = p.max_dph {
                            st.set_rent_max_dph(format!("{:.4}", m.as_usd()).into());
                        }
                        st.set_rent_runtime(match p.image_type {
                            ImageType::Vllm => 1,
                            _ => 0,
                        });
                    }
                }
                st.invoke_rent_cost_refresh();
            }
        });
    }

    {
        let weak = ui.as_weak();
        let store = bridge.store();
        st.on_rent_cost_refresh(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();
            let index = st.get_offer_index();
            let offer: Option<Offer> = store
                .lock()
                .ok()
                .and_then(|g| g.offers.get(index.max(0) as usize).cloned());
            let credit = store
                .lock()
                .ok()
                .and_then(|g| g.snapshot.as_ref().and_then(|s| s.totals.vast_credit));
            let hours = opt_f64(st.get_rent_hours().as_str()).unwrap_or(1.0);
            match offer {
                Some(o) => {
                    let dph = o.dph_total;
                    let total = dph * hours.max(0.0);
                    st.set_rent_dph_line(format!("{}/hr", fmt_money(Money::from_usd(dph))).into());
                    st.set_rent_total_line(
                        format!("{} for {hours:.1} h", fmt_money(Money::from_usd(total))).into(),
                    );
                    st.set_rent_credit_line(
                        credit
                            .map(|c| fmt_money(Money::from_usd(c)))
                            .unwrap_or_else(|| "unknown".to_string())
                            .into(),
                    );
                    st.set_rent_burndown_line(
                        match (credit, dph) {
                            (Some(c), d) if d > 0.0 => format!("{:.1} h at this rate", c / d),
                            _ => "—".to_string(),
                        }
                        .into(),
                    );
                    let approved = opt_f64(st.get_rent_max_dph().as_str());
                    let over_budget = approved.map(|a| dph > a).unwrap_or(true);
                    let over_credit = credit.map(|c| total > c).unwrap_or(false);
                    st.set_rent_cost_level(if over_credit {
                        4
                    } else if over_budget {
                        3
                    } else {
                        1
                    });
                    st.set_rent_fit_line(
                        format!(
                            "{} pooled VRAM across {} GPU(s)",
                            api::fmt_mb(o.pooled_vram_mb()),
                            o.num_gpus
                        )
                        .into(),
                    );
                }
                None => {
                    st.set_rent_dph_line("—".into());
                    st.set_rent_total_line("—".into());
                    st.set_rent_credit_line(
                        credit
                            .map(|c| fmt_money(Money::from_usd(c)))
                            .unwrap_or_else(|| "unknown".to_string())
                            .into(),
                    );
                    st.set_rent_burndown_line("—".into());
                    st.set_rent_cost_level(0);
                    st.set_rent_fit_line("".into());
                }
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let store = bridge.store();
        st.on_rent_go(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();

            // Money gate. The daemon enforces this too — a request without `confirm` is a
            // 409 carrying the preview — but the button must never be the thing that
            // decides to spend.
            if !st.get_rent_confirm() {
                toast(
                    &ui,
                    "tick the confirm box first — this spends real money",
                    3,
                );
                return;
            }
            let approved = match opt_f64(st.get_rent_max_dph().as_str()) {
                Some(v) if v > 0.0 => v,
                _ => {
                    toast(&ui, "set the max $/hr you approve", 3);
                    return;
                }
            };
            let (offer, profile) = {
                let guard = match store.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };
                let offer = guard
                    .offers
                    .get(st.get_offer_index().max(0) as usize)
                    .cloned();
                let profile = guard
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.profiles.get(st.get_rent_profile_index().max(0) as usize))
                    .map(|p| p.id.clone());
                (offer, profile)
            };
            let offer = match offer {
                Some(o) => o,
                None => {
                    toast(&ui, "pick an offer first", 3);
                    return;
                }
            };
            if offer.dph_total > approved {
                toast(
                    &ui,
                    &format!(
                        "that offer is {} — above the {} you approved",
                        fmt_money(Money::from_usd(offer.dph_total)),
                        fmt_money(Money::from_usd(approved))
                    ),
                    4,
                );
                return;
            }

            let runtime = match st.get_rent_runtime() {
                1 => ContainerRuntime::Vllm,
                _ => ContainerRuntime::LlamaCpp,
            };
            let mut env: BTreeMap<String, String> = BTreeMap::new();
            if let Some(repo) = opt(st.get_rent_hf_repo().as_str()) {
                env.insert("MODEL_REPO".into(), repo);
            }
            if let Some(quant) = opt(st.get_rent_hf_quant().as_str()) {
                env.insert("MODEL_QUANT".into(), quant);
            }
            let launch = ContainerLaunch {
                runtime,
                image: st.get_rent_image().to_string(),
                image_type: match runtime {
                    ContainerRuntime::Vllm => ImageType::Vllm,
                    ContainerRuntime::LlamaCpp => ImageType::Prebuilt,
                    // Studio is multi-service; the Slint launch drawer stays single-service.
                },
                disk_gb: st.get_rent_disk_gb().max(1) as u32,
                env,
                onstart: st.get_rent_onstart().to_string(),
                host: "127.0.0.1".to_string(),
                port: u16::try_from(st.get_rent_port().max(1)).unwrap_or(8000),
                expose_public: st.get_rent_expose(),
            };
            let request = RentRequest {
                profile,
                offer_id: Some(offer.id),
                launch,
                confirm: true,
                max_usd_per_hour: approved,
                auto_tunnel: st.get_rent_tunnel(),
                bind_alias: alias_or_default(&ui, st.get_rent_alias().as_str()),
            };
            let ok = format!(
                "rent requested at {}/hr",
                fmt_money(Money::from_usd(offer.dph_total))
            );
            st.set_rent_confirm(false);
            st.set_boot_active(true);
            st.set_boot_phase("reserved".into());
            st.set_boot_level(5);
            st.set_launch_open(true);
            b.act("rent", move |c| async move {
                let _: serde_json::Value =
                    c.post("/v1/vast/instances?no_wait=true", &request).await?;
                anyhow::Ok(ok)
            });
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let store = bridge.store();
        st.on_boot_destroy(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let id = match store.lock() {
                Ok(g) => g.boot_backend.clone(),
                Err(_) => String::new(),
            };
            if id.is_empty() {
                toast(&ui, "nothing to destroy yet", 2);
                return;
            }
            delete_action(
                &b,
                "destroy",
                format!("/v1/endpoints/{}", q(&id)),
                format!("{id} destroyed"),
            );
        });
    }

    {
        let weak = ui.as_weak();
        let store = bridge.store();
        st.on_boot_dismiss(move || {
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<State>();
                st.set_boot_active(false);
                st.set_launch_open(false);
                if let Ok(mut s) = store.lock() {
                    s.boot_started = None;
                }
            }
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fleet
// ─────────────────────────────────────────────────────────────────────────────

/// Destroy, tunnel, restart-download, diagnose, reconcile, bind.
fn wire_fleet(ui: &AppWindow, bridge: &Bridge) {
    let st = ui.global::<State>();

    {
        let b = bridge.clone();
        st.on_refresh_fleet(move || b.refresh());
    }

    {
        let b = bridge.clone();
        st.on_instance_destroy(move |id| {
            let id = id.to_string();
            delete_action(
                &b,
                "destroy instance",
                format!("/v1/vast/instances/{}?confirm=true", q(&id)),
                format!("instance {id} destroyed — billing stops"),
            );
        });
    }

    {
        let b = bridge.clone();
        st.on_instance_tunnel(move |id, up| {
            let id = id.to_string();
            let path = format!("/v1/vast/instances/{}/tunnel", q(&id));
            if up {
                post_action(&b, "tunnel", path, format!("tunnel to {id} opening"));
            } else {
                delete_action(&b, "tunnel", path, format!("tunnel to {id} closed"));
            }
        });
    }

    {
        let b = bridge.clone();
        st.on_instance_restart_download(move |id| {
            let id = id.to_string();
            post_action(
                &b,
                "restart download",
                format!("/v1/vast/instances/{}/restart-download", q(&id)),
                format!("download restarted on {id}"),
            );
        });
    }

    {
        let b = bridge.clone();
        st.on_instance_diagnose(move |id| {
            let id = id.to_string();
            let label = id.clone();
            b.fetch(
                "diagnose",
                move |c| async move {
                    let results: Vec<CheckResult> = c
                        .get(&format!("/v1/vast/instances/{}/diagnose", q(&id)))
                        .await?;
                    anyhow::Ok(results)
                },
                move |ui, _store, results| {
                    let st = ui.global::<State>();
                    st.set_diagnose_of(label.into());
                    st.set_diagnose_rows(ModelRc::new(VecModel::from(check_rows(&results))));
                },
            );
        });
    }

    {
        let b = bridge.clone();
        let b2 = bridge.clone();
        st.on_instance_reconcile(move |id| {
            let id = id.to_string();
            // There is no reconcile *verb* in §6.2, and inventing one would be a second
            // business-logic path. Reconciliation is what `GET /v1/vast/instances` already
            // does: re-read the live fleet, let the daemon compare it against the ledger,
            // and re-render whatever it decided.
            b.fetch(
                "reconcile",
                |c| async move {
                    let live: Vec<VastInstance> = c.get("/v1/vast/instances").await?;
                    anyhow::Ok(live)
                },
                move |ui, _store, live| {
                    let seen = live.iter().any(|i| i.id.to_string() == id);
                    toast(
                        ui,
                        &if seen {
                            format!("{id} is still live at vast.ai — the ledger now agrees")
                        } else {
                            format!("{id} is gone at vast.ai — it is billing nothing")
                        },
                        if seen { 2 } else { 1 },
                    );
                },
            );
            b2.refresh();
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_instance_bind(move |id, alias| {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let (id, alias) = (id.to_string(), alias.to_string());
            if alias.is_empty() {
                toast(&ui, "pick an alias first", 2);
                return;
            }
            let ok = format!("`{alias}` now points at instance {id}");
            b.act("bind instance", move |c| async move {
                let _: serde_json::Value = c
                    .post(
                        &format!("/v1/routes/{}/swap", q(&alias)),
                        &serde_json::json!({ "to": format!("vast-{id}"), "mode": "hot" }),
                    )
                    .await?;
                anyhow::Ok(ok)
            });
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Catalog
// ─────────────────────────────────────────────────────────────────────────────

/// Recipes, profiles and Hugging Face downloads.
fn wire_catalog(ui: &AppWindow, bridge: &Bridge) {
    let st = ui.global::<State>();

    {
        let weak = ui.as_weak();
        let store = bridge.store();
        st.on_recipe_select(move |id| {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let guard = match store.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let recipe = guard
                .snapshot
                .as_ref()
                .and_then(|s| s.recipes.iter().find(|r| r.id.as_str() == id.as_str()))
                .cloned();
            drop(guard);
            if let Some(r) = recipe {
                let st = ui.global::<State>();
                st.set_recipe_sel(id);
                st.set_rc_id(r.id.to_string().into());
                st.set_rc_label(r.label.clone().into());
                st.set_rc_description(r.description.clone().unwrap_or_default().into());
                st.set_rc_body(
                    serde_json::to_string_pretty(&r.kind)
                        .unwrap_or_else(|e| format!("could not render this recipe: {e}"))
                        .into(),
                );
                st.set_rc_issues(ModelRc::new(VecModel::<SharedString>::default()));
            }
        });
    }

    {
        let weak = ui.as_weak();
        st.on_recipe_new(move || {
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<State>();
                st.set_recipe_sel("".into());
                st.set_rc_id("".into());
                st.set_rc_label("".into());
                st.set_rc_description("".into());
                st.set_rc_body(
                    serde_json::to_string_pretty(&serde_json::json!({
                        "kind": "local",
                        "build": "",
                        "model_path": "",
                        "alias_flag": "auto",
                        "host": "127.0.0.1",
                        "ngl": { "ngl": "auto" },
                        "split": { "devices": [], "mode": "layer" },
                        "mode": "thinking"
                    }))
                    .unwrap_or_default()
                    .into(),
                );
            }
        });
    }

    {
        let weak = ui.as_weak();
        st.on_recipe_duplicate(move || {
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<State>();
                let id = st.get_rc_id().to_string();
                st.set_rc_id(format!("{id}-copy").into());
                st.set_rc_label(format!("{} (copy)", st.get_rc_label()).into());
                st.set_recipe_sel("".into());
                toast(&ui, "duplicated — press save to write it", 5);
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_recipe_save(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();
            let id = match RecipeId::parse(st.get_rc_id().trim()) {
                Ok(i) => i,
                Err(e) => {
                    toast(&ui, &format!("bad recipe id: {e}"), 3);
                    return;
                }
            };
            let kind: RecipeKind = match serde_json::from_str(st.get_rc_body().as_str()) {
                Ok(k) => k,
                Err(e) => {
                    toast(&ui, &format!("the recipe body is not a valid spec: {e}"), 3);
                    return;
                }
            };
            let existed = !st.get_recipe_sel().is_empty();
            let recipe = Recipe {
                id: id.clone(),
                label: st.get_rc_label().to_string(),
                description: opt(st.get_rc_description().as_str()),
                kind,
                provenance: Provenance2 {
                    discovered_at_unix: now_unix(),
                    size_bytes: None,
                    source: "slint-ui".to_string(),
                    fit: None,
                },
                created_at_unix: now_unix(),
                updated_at_unix: now_unix(),
            };
            let ok = format!("recipe `{id}` saved");
            let path = format!("/v1/recipes/{}", q(id.as_str()));
            b.act("save recipe", move |c| async move {
                let _: serde_json::Value = if existed {
                    c.put(&path, &recipe).await?
                } else {
                    c.post("/v1/recipes", &recipe).await?
                };
                anyhow::Ok(ok)
            });
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_recipe_delete(move || {
            if let Some(ui) = weak.upgrade() {
                let id = ui.global::<State>().get_rc_id().to_string();
                delete_action(
                    &b,
                    "delete recipe",
                    format!("/v1/recipes/{}", q(&id)),
                    format!("recipe `{id}` deleted"),
                );
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_recipe_validate(move || {
            if let Some(ui) = weak.upgrade() {
                let id = ui.global::<State>().get_rc_id().to_string();
                b.fetch(
                    "validate recipe",
                    move |c| async move {
                        let report: ValidationReport = c
                            .post(
                                &format!("/v1/recipes/{}/validate", q(&id)),
                                &serde_json::json!({}),
                            )
                            .await?;
                        anyhow::Ok(report)
                    },
                    |ui, _store, report| {
                        let lines: Vec<SharedString> = report
                            .issues
                            .iter()
                            .map(|i| {
                                SharedString::from(format!(
                                    "{:?} {}: {}",
                                    i.severity, i.field, i.message
                                ))
                            })
                            .collect();
                        ui.global::<State>()
                            .set_rc_issues(ModelRc::new(VecModel::from(lines)));
                        toast(
                            ui,
                            if report.ok {
                                "recipe validates"
                            } else {
                                "recipe has issues"
                            },
                            if report.ok { 1 } else { 2 },
                        );
                    },
                );
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_recipe_run(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();
            let id = st.get_rc_id().to_string();
            if id.is_empty() {
                toast(&ui, "select a recipe first", 2);
                return;
            }
            let alias = alias_or_default(&ui, st.get_rc_alias().as_str());
            let path = match &alias {
                Some(a) => format!(
                    "/v1/recipes/{}/instantiate?no_wait=true&alias={}",
                    q(&id),
                    q(a.as_str())
                ),
                None => format!("/v1/recipes/{}/instantiate?no_wait=true", q(&id)),
            };
            st.set_boot_active(true);
            st.set_boot_phase("requested".into());
            st.set_boot_level(5);
            st.set_launch_open(true);
            post_action(
                &b,
                "run recipe",
                path,
                format!("recipe `{id}` instantiated"),
            );
        });
    }

    // ── Profiles ──────────────────────────────────────────────────────────
    {
        let weak = ui.as_weak();
        let store = bridge.store();
        st.on_profile_select(move |id| {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let profile = store.lock().ok().and_then(|g| {
                g.snapshot
                    .as_ref()
                    .and_then(|s| s.profiles.iter().find(|p| p.id.as_str() == id.as_str()))
                    .cloned()
            });
            if let Some(p) = profile {
                let st = ui.global::<State>();
                st.set_profile_sel(id);
                st.set_pf_id(p.id.to_string().into());
                st.set_pf_label(p.label.clone().into());
                st.set_pf_gpu_names(p.gpu_names.join(", ").into());
                st.set_pf_min_gpus(p.num_gpus_min.min(i32::MAX as u32) as i32);
                st.set_pf_max_gpus(p.num_gpus_max.min(i32::MAX as u32) as i32);
                st.set_pf_max_dph(
                    p.max_dph
                        .map(|m| format!("{:.4}", m.as_usd()))
                        .unwrap_or_default()
                        .into(),
                );
                st.set_pf_min_reliability(format!("{:.3}", p.min_reliability).into());
                st.set_pf_min_inet(p.min_inet_down.min(i32::MAX as u32) as i32);
                st.set_pf_min_disk(p.min_disk_gb.min(i32::MAX as u32) as i32);
                st.set_pf_min_cuda(
                    p.min_cuda
                        .map(|c| format!("{c:.1}"))
                        .unwrap_or_default()
                        .into(),
                );
                st.set_pf_geo(geo_to_index(&p.geo));
                st.set_pf_image_type(image_type_to_index(p.image_type));
            }
        });
    }

    {
        let weak = ui.as_weak();
        st.on_profile_new(move || {
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<State>();
                st.set_profile_sel("".into());
                st.set_pf_id("".into());
                st.set_pf_label("".into());
                st.set_pf_gpu_names("".into());
                st.set_pf_min_gpus(1);
                st.set_pf_max_gpus(1);
                st.set_pf_max_dph("".into());
                st.set_pf_min_reliability("0.98".into());
                st.set_pf_min_inet(200);
                st.set_pf_min_disk(60);
                st.set_pf_min_cuda("".into());
                st.set_pf_geo(0);
                st.set_pf_image_type(0);
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_profile_save(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();
            let id = match ProfileId::parse(st.get_pf_id().trim()) {
                Ok(i) => i,
                Err(e) => {
                    toast(&ui, &format!("bad profile id: {e}"), 3);
                    return;
                }
            };
            let existed = !st.get_profile_sel().is_empty();
            let profile = SearchProfile {
                id: id.clone(),
                label: st.get_pf_label().to_string(),
                gpu_names: split_list(st.get_pf_gpu_names().as_str()),
                num_gpus_min: st.get_pf_min_gpus().max(1) as u32,
                num_gpus_max: st.get_pf_max_gpus().max(1) as u32,
                max_dph: opt_f64(st.get_pf_max_dph().as_str()).map(Money::from_usd),
                min_reliability: opt_f64(st.get_pf_min_reliability().as_str()).unwrap_or(0.0)
                    as f32,
                min_inet_down: st.get_pf_min_inet().max(0) as u32,
                min_disk_gb: st.get_pf_min_disk().max(0) as u32,
                min_cuda: opt_f64(st.get_pf_min_cuda().as_str()).map(|v| v as f32),
                geo: geo_from_index(st.get_pf_geo()),
                image_type: image_type_from_index(st.get_pf_image_type()),
                extra: serde_json::Map::new(),
            };
            let ok = format!("profile `{id}` saved");
            let path = format!("/v1/profiles/{}", q(id.as_str()));
            b.act("save profile", move |c| async move {
                let _: serde_json::Value = if existed {
                    c.put(&path, &profile).await?
                } else {
                    c.post("/v1/profiles", &profile).await?
                };
                anyhow::Ok(ok)
            });
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_profile_delete(move || {
            if let Some(ui) = weak.upgrade() {
                let id = ui.global::<State>().get_pf_id().to_string();
                delete_action(
                    &b,
                    "delete profile",
                    format!("/v1/profiles/{}", q(&id)),
                    format!("profile `{id}` deleted"),
                );
            }
        });
    }

    // ── Hugging Face ──────────────────────────────────────────────────────
    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_load_hf_files(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let repo = ui.global::<State>().get_hf_repo().to_string();
            if repo.trim().is_empty() {
                toast(&ui, "type a repo, e.g. unsloth/Qwen3-30B-A3B-GGUF", 2);
                return;
            }
            b.fetch(
                "hf files",
                move |c| async move {
                    let value: serde_json::Value =
                        c.get(&format!("/v1/hf/models/{}/files", q(&repo))).await?;
                    // The route is documented as `Vec<HfFile>`; P-07's own signature groups
                    // them. Accept either rather than pick a side.
                    let groups = match serde_json::from_value::<Vec<HfFileGroup>>(value.clone()) {
                        Ok(g) => g,
                        Err(_) => group_hf_files(&serde_json::from_value::<Vec<HfFile>>(value)?),
                    };
                    anyhow::Ok(groups)
                },
                |ui, store, groups| {
                    ui.global::<State>()
                        .set_hf_files(ModelRc::new(VecModel::from(hf_rows(&groups))));
                    if let Ok(mut s) = store.lock() {
                        s.hf_groups = groups;
                    }
                },
            );
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_hf_download(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();
            let repo = st.get_hf_repo().to_string();
            let quant = st.get_hf_quant().to_string();
            if repo.trim().is_empty() || quant.trim().is_empty() {
                toast(&ui, "a download needs a repo and a quant", 2);
                return;
            }
            let mut body = serde_json::json!({ "repo": repo, "quant": quant });
            if let Some(dest) = opt(st.get_hf_dest().as_str()) {
                body["dest"] = serde_json::Value::String(dest);
            }
            let ok = format!("downloading {quant} from {repo}");
            b.act("hf download", move |c| async move {
                let _: serde_json::Value = c.post("/v1/hf/downloads?no_wait=true", &body).await?;
                anyhow::Ok(ok)
            });
        });
    }

    {
        let b = bridge.clone();
        st.on_hf_cancel(move |job| {
            let job = job.to_string();
            delete_action(
                &b,
                "cancel download",
                format!("/v1/hf/downloads/{}", q(&job)),
                format!("download {job} cancelled"),
            );
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Providers
// ─────────────────────────────────────────────────────────────────────────────

/// Credential entry (write-only), test, catalogue, activate.
fn wire_providers(ui: &AppWindow, bridge: &Bridge) {
    let st = ui.global::<State>();

    {
        let weak = ui.as_weak();
        let store = bridge.store();
        st.on_provider_select(move |id| {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let provider = store.lock().ok().and_then(|g| {
                g.snapshot
                    .as_ref()
                    .and_then(|s| s.providers.iter().find(|p| p.id.as_str() == id.as_str()))
                    .cloned()
            });
            if let Some(p) = provider {
                let st = ui.global::<State>();
                st.set_provider_sel(id);
                st.set_pv_base_url(p.base_url.clone().into());
                st.set_pv_source(api::credential_source(&p.credential).into());
                // The key field always starts empty: the value is never read back (§9.2).
                st.set_pv_key("".into());
                st.set_pv_key_env(match &p.credential {
                    CredentialSource::Env { var } => var.clone().into(),
                    _ => SharedString::new(),
                });
                st.set_pv_key_file(match &p.credential {
                    CredentialSource::File { path } => path.clone().into(),
                    _ => SharedString::new(),
                });
                st.set_provider_models(ModelRc::new(VecModel::<ModelRow>::default()));
                st.set_provider_checks(ModelRc::new(VecModel::<CheckRow>::default()));
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_provider_save(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();
            let id = st.get_provider_sel().to_string();
            if id.is_empty() {
                return;
            }
            let mut body = serde_json::Map::new();
            if let Some(url) = opt(st.get_pv_base_url().as_str()) {
                body.insert("base_url".into(), serde_json::Value::String(url));
            }
            // Exactly one credential form is sent, and a blank key field means "keep the
            // one you have" rather than "clear it".
            if let Some(k) = opt(st.get_pv_key().as_str()) {
                body.insert("api_key".into(), serde_json::Value::String(k));
            } else if let Some(v) = opt(st.get_pv_key_env().as_str()) {
                body.insert("api_key_env".into(), serde_json::Value::String(v));
            } else if let Some(f) = opt(st.get_pv_key_file().as_str()) {
                body.insert("api_key_file".into(), serde_json::Value::String(f));
            }
            st.set_pv_key("".into());
            let ok = format!("provider `{id}` saved — a key goes to credentials.toml at 0600");
            let path = format!("/v1/providers/{}", q(&id));
            b.act("save provider", move |c| async move {
                let _: serde_json::Value = c.put(&path, &serde_json::Value::Object(body)).await?;
                anyhow::Ok(ok)
            });
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_provider_test(move || {
            if let Some(ui) = weak.upgrade() {
                let id = ui.global::<State>().get_provider_sel().to_string();
                if id.is_empty() {
                    return;
                }
                b.fetch(
                    "provider test",
                    move |c| async move {
                        let results: Vec<CheckResult> = c
                            .post(
                                &format!("/v1/providers/{}/test", q(&id)),
                                &serde_json::json!({}),
                            )
                            .await?;
                        anyhow::Ok(results)
                    },
                    |ui, _store, results| {
                        ui.global::<State>()
                            .set_provider_checks(ModelRc::new(VecModel::from(check_rows(
                                &results,
                            ))));
                    },
                );
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_load_provider_models(move || {
            if let Some(ui) = weak.upgrade() {
                let id = ui.global::<State>().get_provider_sel().to_string();
                if id.is_empty() {
                    return;
                }
                b.fetch(
                    "provider catalogue",
                    move |c| async move {
                        let models: Vec<UpstreamModel> =
                            c.get(&format!("/v1/providers/{}/models", q(&id))).await?;
                        anyhow::Ok(models)
                    },
                    |ui, store, models| {
                        let filter = ui.global::<State>().get_pv_model_filter().to_string();
                        ui.global::<State>()
                            .set_provider_models(ModelRc::new(VecModel::from(model_rows(
                                &models, &filter,
                            ))));
                        if let Ok(mut s) = store.lock() {
                            s.provider_models = models;
                        }
                    },
                );
            }
        });
    }

    {
        let weak = ui.as_weak();
        let store = bridge.store();
        st.on_provider_filter_changed(move |filter| {
            if let Some(ui) = weak.upgrade() {
                if let Ok(s) = store.lock() {
                    ui.global::<State>()
                        .set_provider_models(ModelRc::new(VecModel::from(model_rows(
                            &s.provider_models,
                            filter.as_str(),
                        ))));
                }
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let store = bridge.store();
        st.on_provider_activate(move |model_id| {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            match managed_spec(&ui, &store, model_id.as_str()) {
                Ok(spec) => {
                    let alias = alias_or_default(&ui, "");
                    start_endpoint(&b, &ui, EndpointSpec::Managed(spec), alias);
                }
                Err(e) => toast(&ui, &format!("cannot activate: {e}"), 3),
            }
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        let store = bridge.store();
        st.on_provider_recipe(move |model_id| {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            match managed_spec(&ui, &store, model_id.as_str()) {
                Ok(spec) => {
                    let suggested = EndpointSpec::Managed(spec.clone()).suggested_id();
                    match RecipeId::parse(&suggested) {
                        Ok(id) => {
                            save_recipe(&b, id, model_id.to_string(), RecipeKind::Managed(spec))
                        }
                        Err(e) => toast(&ui, &format!("cannot name the recipe: {e}"), 3),
                    }
                }
                Err(e) => toast(&ui, &format!("cannot save: {e}"), 3),
            }
        });
    }
}

/// Build a managed-provider spec for one upstream model id.
fn managed_spec(
    ui: &AppWindow,
    store: &Arc<Mutex<Store>>,
    model_id: &str,
) -> anyhow::Result<ManagedSpec> {
    let id = ui.global::<State>().get_provider_sel().to_string();
    let provider = ProviderId::parse(&id)?;
    let guard = store
        .lock()
        .map_err(|_| anyhow::anyhow!("store is poisoned"))?;
    let status = guard
        .snapshot
        .as_ref()
        .and_then(|s| s.providers.iter().find(|p| p.id == provider))
        .ok_or_else(|| anyhow::anyhow!("that provider is no longer configured"))?;
    Ok(ManagedSpec {
        provider,
        base_url: status.base_url.clone(),
        credential: status.credential.clone(),
        model_id: Some(model_id.to_string()),
        protocol: Protocol::OpenAi,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Usage / Doctor
// ─────────────────────────────────────────────────────────────────────────────

/// Usage windows and groupings, the check registry, per-check runs, and smoke.
fn wire_doctor(ui: &AppWindow, bridge: &Bridge) {
    let st = ui.global::<State>();

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_load_usage(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();
            let window = st
                .get_usage_windows()
                .row_data(st.get_usage_window().max(0) as usize)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "24h".to_string());
            let by = st
                .get_usage_groupings()
                .row_data(st.get_usage_by().max(0) as usize)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "provider".to_string());
            b.fetch(
                "usage",
                move |c| async move {
                    let summary: UsageSummary = c
                        .get(&format!("/v1/usage?since={}&by={}", q(&window), q(&by)))
                        .await?;
                    anyhow::Ok(summary)
                },
                |ui, _store, summary| {
                    let st = ui.global::<State>();
                    let (total, metered) = cost_text(&summary.total_cost);
                    st.set_usage_total(total.into());
                    st.set_usage_metered(metered);
                    st.set_usage_tokens(
                        format!(
                            "{} prompt / {} completion",
                            summary.total_prompt, summary.total_completion
                        )
                        .into(),
                    );
                    st.set_usage_requests(summary.rows.to_string().into());
                    st.set_usage_rows(ModelRc::new(VecModel::from(usage_rows(&summary))));
                },
            );
        });
    }

    {
        let b = bridge.clone();
        st.on_load_checks(move || {
            b.fetch(
                "checks",
                |c| async move {
                    let value: serde_json::Value = c.get("/v1/checks").await?;
                    anyhow::Ok(value)
                },
                |ui, _store, value| {
                    ui.global::<State>()
                        .set_checks(ModelRc::new(VecModel::from(registry_rows(&value))));
                },
            );
        });
    }

    {
        let b = bridge.clone();
        st.on_run_check(move |id| {
            let id = id.to_string();
            // `/v1/diagnose` streams one event per check; `follow_checks` folds each result
            // into the table as it lands rather than batching at the end.
            b.follow_checks(format!("/v1/diagnose?only={}", q(&id)));
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_run_diagnose(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<State>().set_doctor_running(true);
            }
            b.follow_checks("/v1/diagnose".to_string());
        });
    }

    {
        let weak = ui.as_weak();
        let b = bridge.clone();
        st.on_run_smoke(move || {
            let ui = match weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let st = ui.global::<State>();
            let alias = match alias_or_default(&ui, st.get_smoke_alias().as_str()) {
                Some(a) => a.to_string(),
                None => {
                    toast(&ui, "name an alias to smoke-test", 2);
                    return;
                }
            };
            st.set_doctor_running(true);
            b.fetch(
                "smoke",
                move |c| async move {
                    let value: serde_json::Value = c
                        .post("/v1/smoke", &serde_json::json!({ "alias": alias }))
                        .await?;
                    anyhow::Ok(value)
                },
                |ui, _store, value| {
                    let st = ui.global::<State>();
                    st.set_doctor_running(false);
                    let rows = probe_rows(&value);
                    if rows.is_empty() {
                        toast(ui, "the daemon answered, but with no recognisable probe", 2);
                    }
                    st.set_probes(ModelRc::new(VecModel::from(rows)));
                },
            );
        });
    }
}
