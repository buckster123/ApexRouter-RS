//! OWNER: unit P-04 (providers/src/vast/{rent,boot,stall}.rs). Do not edit outside that
//! unit.
//!
//! Renting. **Reserve before billing.**
//!
//! `ledger.reserve()` appends a `Reserved` row and returns a `PendingLaunch`; the create
//! call happens; `pending.commit(instance_id)` appends `Confirmed`. Dropping the guard
//! without committing appends `OrphanSuspect` synchronously. A `SIGKILL` skips `Drop`
//! entirely — which is why the `Reserved` row, written *before* the call, is the real
//! protection.
//!
//! ## The one irreversible action in the product
//!
//! Everything else ApexRouter does can be undone by restarting it. Renting cannot: the money
//! is gone the instant vast returns a `new_contract`. So:
//!
//! * there is no path to `VastApi::create` that does not take a [`SpendApproval`], and
//!   `SpendApproval` has no constructor other than
//!   [`apexrouter_core::money::SpendApproval::confirm`], which enforces the daemon-side
//!   ceiling;
//! * [`rent`] additionally refuses a request whose own `max_usd_per_hour` exceeds the
//!   approval it was handed, so a surface cannot approve `$0.40` and then rent at `$3.34`;
//! * [`preview`] exists so every surface shows `dph_total`, an estimated total **and the
//!   current account credit** before the confirm, in the same words;
//! * [`destroy`] verifies before forgetting, and is reachable from one CLI verb, one HTTP
//!   route and one library call, because the stop button must never be hard to find.
//!
//! Nothing here is exercised against the live API by any test. The money path is proved
//! against a mock; only `GET /users/current/` — which is free — is ever called for real.

use super::api::VastApi;
use super::boot::{watch_boot_every, DEFAULT_POLL_MIN_MS, POLL_FLOOR_MS};
use crate::local::{DownMode, LaunchPlan, Provisioner};
use apexrouter_core::config::VastCfg;
use apexrouter_core::error::{Error, Result};
use apexrouter_core::ledger::Ledger;
use apexrouter_core::money::SpendApproval;
use apexrouter_protocol::{
    AlertLevel, ArgvPreview, Backend, BackendId, BackendKind, BackendLimits, BootPhase,
    ContainerLaunch, CostEstimate, CredentialSource, EndpointRef, EndpointSpec, Event, GeoFilter,
    Health, InstanceId, LedgerRow, LedgerState, LogSource, Money, Offer, OfferQuery, PriceModel,
    PriceSource, Protocol, Provenance, RentRequest, VastInstance, VastSpec,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::Instant;

/// The window a cost preview is quoted over, hours. One hour is the same unit
/// `SpendApproval::confirm` checks credit against, so the two numbers never disagree.
pub const PREVIEW_HOURS: f64 = 1.0;

/// Look up one market offer so rent can check its live rate against the approval.
///
/// Vast has no single-ask GET we rely on; we search with an `id` equality filter and, when
/// the transport ignores `extra` (the in-process fixture), fall back to a wide search and
/// filter client-side. An offer that is gone from the market refuses rather than billing
/// an unknown rate.
async fn resolve_offer(api: &dyn VastApi, offer_id: u64) -> Result<Offer> {
    let mut q = OfferQuery {
        gpu_names: Vec::new(),
        num_gpus_min: 0,
        num_gpus_max: 0,
        max_dph: None,
        min_reliability: None,
        min_inet_down: None,
        min_disk_gb: None,
        min_cuda: None,
        geo: GeoFilter::Any,
        verified: None,
        limit: 256,
        order: Vec::new(),
        extra: serde_json::Map::new(),
    };
    q.extra
        .insert("id".to_owned(), serde_json::json!({ "eq": offer_id }));
    let res = api.search(&q).await?;
    if let Some(o) = res.offers.into_iter().find(|o| o.id == offer_id) {
        return Ok(o);
    }
    // Client-side fixtures ignore `extra`; try once more without the id filter.
    q.extra.clear();
    let res = api.search(&q).await?;
    res.offers
        .into_iter()
        .find(|o| o.id == offer_id)
        .ok_or_else(|| {
            Error::NotFound(format!(
                "offer {offer_id} is not on the market; cannot verify its hourly rate against \
                 the approval"
            ))
        })
}

/// How long [`destroy`] keeps checking that the box really is gone before giving up and
/// telling a human. Verification is the point: a `Destroyed` row we cannot justify is worse
/// than no row at all.
pub const DESTROY_VERIFY_SECS: u64 = 120;

/// Warn when the account credit buys fewer than this many hours at the offered rate.
const LOW_CREDIT_HOURS: f64 = 3.0;

/// Inbound bandwidth (`inet_down_cost`, $/GB) above which the quote calls the host
/// metered. $0.002/GB is $2/TB — home boxes sit at ~$0.13/TB, datacenter meters at
/// $9-40/TB, and nothing in between is worth an operator's surprise.
const METERED_INET_PER_GB: f64 = 0.002;

/// Environment variable names whose values must never be rendered in a preview or a log.
const SECRET_ENV: &[&str] = &["TOKEN", "KEY", "SECRET", "PASSWORD"];

/// The `BackendId` a rented instance appears under, everywhere.
///
/// Matches `EndpointSpec::suggested_id` for a `VastSpec`, so an event broadcast before the
/// endpoint record exists still names the backend the record will name.
///
/// # Errors
/// Only if a future id charset stopped admitting `vast-<digits>`.
pub fn backend_id(id: InstanceId) -> Result<BackendId> {
    Ok(BackendId::parse(&format!("vast-{}", id.0))?)
}

/// What the operator must see before confirming a rental.
///
/// Every surface renders these fields and no surface computes them itself: `dph_total`, an
/// estimated total, and the **current account credit**. The last one is what turns
/// "$3.34/hr" into "2.3 hours and the account is empty".
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RentPreview {
    /// Which offer this quotes.
    pub offer_id: u64,
    /// GPU model, exactly as the market spells it.
    pub gpu_name: String,
    /// How many of them.
    pub num_gpus: u32,
    /// All-in dollars per hour — the headline number.
    pub dph_total: f64,
    /// The window `est_total_usd` covers.
    pub est_hours: f64,
    /// `dph_total × est_hours`.
    pub est_total_usd: f64,
    /// The same figure with its provenance attached, for surfaces that render a
    /// [`CostEstimate`].
    pub est_total: CostEstimate,
    /// Credit remaining, when the account could be read. `None` means "we could not ask" —
    /// itself worth rendering, rather than showing a confident `$0.00`.
    pub credit: Option<f64>,
    /// How long the remaining credit lasts at this rate.
    pub burn_down_hours: Option<f32>,
    /// The ceiling the request carries, for the confirm dialog to echo back.
    pub max_usd_per_hour: f64,
    /// Everything the operator must read before saying yes.
    pub warnings: Vec<String>,
}

/// Quote one offer against one request and the account's credit.
///
/// Pure and synchronous: the same numbers land in the CLI's confirm prompt, the 409 body of
/// `POST /v1/vast/instances`, the web UI's rent drawer and the Slint dialog. A non-finite or
/// non-positive `hours` falls back to [`PREVIEW_HOURS`] rather than producing a `NaN` total.
pub fn preview(offer: &Offer, req: &RentRequest, credit: Option<f64>, hours: f64) -> RentPreview {
    let hours = if hours.is_finite() && hours > 0.0 {
        hours
    } else {
        PREVIEW_HOURS
    };
    let est_total_usd = offer.dph_total * hours;
    let mut warnings: Vec<String> = Vec::new();

    if offer.dph_total > req.max_usd_per_hour {
        warnings.push(format!(
            "this offer is ${:.4}/hr, above the ${:.4}/hr ceiling on the request",
            offer.dph_total, req.max_usd_per_hour
        ));
    }

    // Bandwidth is the cost the hourly rate hides: a metered host at ~$38/TB once turned
    // $0.60 of box-hours into a $3.80 leg (GARDEN-RUNS.md, R2a). The offer has carried
    // `inet_down_cost` all along; a quote that does not say it is not a quote.
    if let Some(rate_per_gb) = offer.inet_down_cost {
        if rate_per_gb > METERED_INET_PER_GB {
            warnings.push(format!(
                "metered inbound bandwidth: ${:.2}/TB — a 100 GB model pull adds ~${:.2}",
                rate_per_gb * 1000.0,
                rate_per_gb * 100.0
            ));
        }
    }

    let burn_down_hours = match credit {
        Some(credit) if offer.dph_total > 0.0 => {
            let hours_left = credit / offer.dph_total;
            if credit < offer.dph_total {
                warnings.push(format!(
                    "credit is ${credit:.2}: it does not cover one hour at ${:.4}/hr",
                    offer.dph_total
                ));
            } else if hours_left < LOW_CREDIT_HOURS {
                warnings.push(format!(
                    "credit is ${credit:.2}: about {hours_left:.1} hours at ${:.4}/hr, then \
                     the instance is cut off",
                    offer.dph_total
                ));
            }
            Some(hours_left as f32)
        }
        Some(_) => None,
        None => {
            warnings.push(
                "the account credit could not be read, so the burn-down is unknown".to_owned(),
            );
            None
        }
    };

    if req.launch.expose_public {
        warnings.push(
            "expose_public is set: the container port will be plaintext HTTP on a shared \
             public IP, and requires a per-instance API key"
                .to_owned(),
        );
    }
    if !req.confirm {
        warnings.push("this is a preview: nothing has been rented".to_owned());
    }

    RentPreview {
        offer_id: offer.id,
        gpu_name: offer.gpu_name.clone(),
        num_gpus: offer.num_gpus,
        dph_total: offer.dph_total,
        est_hours: hours,
        est_total_usd,
        est_total: CostEstimate::Approximate {
            usd: Money::from_usd(est_total_usd),
            source: PriceSource::VastOffer,
            assumption: format!(
                "{hours:.1} h at the offer's dph_total of ${:.4}/hr; vast bills by the second \
                 and charges storage and bandwidth separately",
                offer.dph_total
            ),
        },
        credit,
        burn_down_hours,
        max_usd_per_hour: req.max_usd_per_hour,
        warnings,
    }
}

/// Rent one box. There is no path to a billing call that does not take a [`SpendApproval`].
///
/// The order is the whole point:
///
/// 1. refuse anything the approval does not cover, before a byte leaves the machine;
/// 2. **append the `Reserved` row** — if the process dies here, the ledger already says a
///    rental was about to happen;
/// 3. call `create`;
/// 4. append `Confirmed` naming the instance id.
///
/// Step 2 before step 3 is not a nicety. A `SIGKILL` between them leaves a `Reserved` row
/// that `apexrouter vast ls --orphans` shows and a human can act on; the other order leaves a
/// billing box that nothing on this machine has ever heard of.
///
/// `req.offer_id` must be set: choosing an offer is `super::offers::search_unified`'s job,
/// and it is the *same* search the operator browsed, which is what makes "rents an offer you
/// never saw" impossible.
///
/// # Errors
/// Refuses an unconfirmed request, a request above its own approval, or one with no offer.
/// A failed `create` propagates and leaves an `OrphanSuspect` row plus a `Critical` alert,
/// because a failed create is not proof that nothing was created.
pub async fn rent(
    api: &dyn VastApi,
    ledger: &Ledger,
    req: &RentRequest,
    approval: SpendApproval,
    tx: &broadcast::Sender<Event>,
) -> Result<InstanceId> {
    if !req.confirm {
        return Err(Error::Invalid {
            what: "rent request".to_owned(),
            why: "`confirm` is false; call `preview` for a cost preview instead".to_owned(),
        });
    }

    // The approval is a ceiling, not a suggestion. A surface that confirmed $0.40 cannot then
    // rent at $3.34 by putting a bigger number in the request.
    let requested = Money::from_usd(req.max_usd_per_hour);
    if requested > approval.max_usd_per_hour() {
        return Err(Error::Invalid {
            what: "rent request".to_owned(),
            why: format!(
                "the request asks for up to {requested}/hr but the approval covers only {}/hr",
                approval.max_usd_per_hour()
            ),
        });
    }

    let offer_id = req.offer_id.ok_or_else(|| Error::Invalid {
        what: "rent request".to_owned(),
        why: "no offer_id: resolve the profile with `search_unified` first, so the offer \
              rented is the offer the operator saw"
            .to_owned(),
    })?;

    // The approval and the request max are ceilings on a *number the client typed*. The
    // bill is the offer's live `dph_total`. Without this check, a caller can approve $0.40
    // and still create a $3.34/hr box by naming its offer_id — which is how SpendApproval
    // was hollow for rent while wake already compared live rate.
    let offer = resolve_offer(api, offer_id).await?;
    let offer_rate = Money::from_usd(offer.dph_total);
    if offer_rate > approval.max_usd_per_hour() {
        return Err(Error::Invalid {
            what: "rent request".to_owned(),
            why: format!(
                "offer {offer_id} bills {offer_rate}/hr but the approval covers only {}/hr",
                approval.max_usd_per_hour()
            ),
        });
    }
    if offer.dph_total > req.max_usd_per_hour {
        return Err(Error::Invalid {
            what: "rent request".to_owned(),
            why: format!(
                "offer {offer_id} bills ${:.4}/hr, above the request ceiling of ${:.4}/hr",
                offer.dph_total, req.max_usd_per_hour
            ),
        });
    }

    let launch = hardened(&req.launch);
    let label = label_for(req);

    let _ = tx.send(Event::LogLine {
        source: LogSource::Daemon,
        line: format!(
            "vast: reserving offer {offer_id} at ${:.4}/hr (approved up to {}/hr via {:?})",
            offer.dph_total,
            approval.max_usd_per_hour(),
            approval.source()
        ),
    });

    // ---- the ledger row goes down BEFORE the billing call -------------------------------
    let pending = ledger.reserve(req, &approval)?;

    let id = match api.create(offer_id, &launch, &label).await {
        Ok(id) => id,
        Err(e) => {
            // `pending` is dropped on the way out: an `OrphanSuspect` row lands
            // synchronously, because a failed create is not proof that nothing was created.
            let _ = tx.send(Event::Alert {
                level: AlertLevel::Critical,
                message: format!(
                    "the vast create call for offer {offer_id} failed ({e}); a box may still \
                     have been created and may be billing"
                ),
                action: Some("apexrouter vast ls --orphans".to_owned()),
                id: format!("vast.create-failed.{offer_id}"),
            });
            return Err(e);
        }
    };

    pending.commit(id)?;
    tracing::info!(instance = id.0, offer_id, label, "rented a vast instance");

    if let Ok(backend) = backend_id(id) {
        let _ = tx.send(Event::BootProgress {
            backend,
            phase: BootPhase::Provisioning,
            line: Some(format!("instance {} created from offer {offer_id}", id.0)),
        });
    }
    broadcast_fleet(api, tx).await;

    Ok(id)
}

/// Destroy an instance and **verify before forgetting**.
///
/// `DestroyRequested` is appended first, then the `DELETE` goes out, then the instance is
/// polled until it is gone or terminal, and only then does the `Destroyed` row land with the
/// accrued cost. An unverified destroy is an error, not a success: the ledger keeps counting
/// the box as active — which is what `apexrouter vast ls --orphans` exists for — and a
/// `Serious` alert tells a human to check the console.
///
/// # Errors
/// When the `DELETE` fails, or when the instance is still there after the verification
/// budget. In both cases the `DestroyRequested` row remains, so the intent is on the record.
pub async fn destroy(
    api: &dyn VastApi,
    ledger: &Ledger,
    id: InstanceId,
    tx: &broadcast::Sender<Event>,
) -> Result<()> {
    destroy_within(
        api,
        ledger,
        id,
        tx,
        DESTROY_VERIFY_SECS,
        DEFAULT_POLL_MIN_MS,
    )
    .await
}

/// [`destroy`] with the verification budget and poll floor spelled out.
///
/// # Errors
/// As [`destroy`].
pub async fn destroy_within(
    api: &dyn VastApi,
    ledger: &Ledger,
    id: InstanceId,
    tx: &broadcast::Sender<Event>,
    verify_secs: u64,
    poll_min_ms: u64,
) -> Result<()> {
    // Read it before it is gone, so the accrued cost has something to be computed from.
    let before = match api.instance(id).await {
        Ok(inst) => inst,
        Err(e) => {
            tracing::warn!(
                instance = id.0,
                error = %e,
                "could not read the instance before destroying it; destroying anyway"
            );
            None
        }
    };

    ledger.append(&row(
        id,
        LedgerState::DestroyRequested,
        before.as_ref(),
        None,
        CostEstimate::Unknown,
        format!("destroy requested for instance {}", id.0),
    ))?;

    api.destroy(id).await?;

    let poll = Duration::from_millis(poll_min_ms.max(POLL_FLOOR_MS));
    let deadline = Instant::now() + Duration::from_secs(verify_secs);
    loop {
        match api.instance(id).await {
            Ok(None) => break,
            Ok(Some(inst)) if inst.is_terminal() => break,
            Ok(Some(_)) => {}
            Err(e) => {
                tracing::warn!(instance = id.0, error = %e, "destroy verification poll failed");
            }
        }
        if Instant::now() >= deadline {
            let _ = tx.send(Event::Alert {
                level: AlertLevel::Serious,
                message: format!(
                    "vast instance {} did not disappear within {verify_secs}s of the destroy \
                     call; it is still counted as active",
                    id.0
                ),
                action: Some(format!("apexrouter vast destroy {} --yes", id.0)),
                id: format!("vast.destroy-unverified.{}", id.0),
            });
            return Err(Error::Other(format!(
                "instance {} was asked to destroy but could not be verified gone within \
                 {verify_secs}s; the ledger still counts it as active",
                id.0
            )));
        }
        tokio::time::sleep(poll).await;
    }

    let at = chrono::Utc::now().timestamp();
    ledger.append(&row(
        id,
        LedgerState::Destroyed,
        before.as_ref(),
        Some(at),
        accrued(before.as_ref()),
        format!("instance {} verified gone", id.0),
    ))?;

    if let Ok(backend) = backend_id(id) {
        let _ = tx.send(Event::BootProgress {
            backend,
            phase: BootPhase::Destroyed,
            line: Some("destroyed and verified gone".to_owned()),
        });
    }
    broadcast_fleet(api, tx).await;
    Ok(())
}

/// What a parked box keeps costing per week, when the row prices its own disk
/// (`storage_cost` $/GB/month × allocated GB, re-based to a week).
///
/// `None` when vast did not say — the callers then render the measured band from the
/// campaign ($0.12–0.19/GB/month) *labelled as an estimate*, never a confident number.
pub fn weekly_disk_usd(inst: &VastInstance) -> Option<f64> {
    let per_gb_month = inst.storage_cost.filter(|c| c.is_finite() && *c >= 0.0)?;
    let gb = inst.disk_space.filter(|g| g.is_finite() && *g > 0.0)?;
    Some(per_gb_month * gb * 12.0 / 365.25 * 7.0)
}

/// Park one box: release the GPUs, keep the disk. **Not** a destroy — the instance stays
/// on the ledger, keeps billing storage, and `wake` can bring it back with its models and
/// builds intact ($3–6/week holds 100–150 GB; a re-download from CN cost an afternoon).
///
/// The state change is verified, not assumed: vast accepting the PUT is not the same as
/// the box stopping. On a verification timeout the request stands, the error says so, and
/// the ledger records `Parked` only once the fleet actually reports it.
///
/// # Errors
/// An unknown instance, a refused state change, or a box still not stopped after
/// `verify_secs`.
pub async fn park(
    api: &dyn VastApi,
    ledger: &Ledger,
    id: InstanceId,
    tx: &broadcast::Sender<Event>,
    verify_secs: u64,
    poll_min_ms: u64,
) -> Result<VastInstance> {
    let inst = api
        .instance(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("vast instance {}", id.0)))?;
    if inst.phase() == BootPhase::Parked {
        return Ok(inst);
    }

    api.set_target_state(id, false).await?;

    let poll = Duration::from_millis(poll_min_ms.max(POLL_FLOOR_MS));
    let deadline = Instant::now() + Duration::from_secs(verify_secs);
    let parked = loop {
        match api.instance(id).await {
            Ok(Some(now)) if now.phase() == BootPhase::Parked => break now,
            Ok(None) => {
                return Err(Error::Other(format!(
                    "instance {} disappeared while being parked — check the console: \
                     parking must never destroy",
                    id.0
                )))
            }
            Ok(Some(_)) => {}
            Err(e) => {
                tracing::warn!(instance = id.0, error = %e, "park verification poll failed");
            }
        }
        if Instant::now() >= deadline {
            return Err(Error::Other(format!(
                "instance {} was asked to stop but still is not stopped after \
                 {verify_secs}s; the request stands — check `apexrouter vast ls`",
                id.0
            )));
        }
        tokio::time::sleep(poll).await;
    };

    let weekly = weekly_disk_usd(&parked);
    ledger.append(&row(
        id,
        LedgerState::Parked,
        Some(&parked),
        None,
        weekly.map_or(CostEstimate::Unknown, |w| CostEstimate::Approximate {
            usd: Money::from_usd(w),
            source: PriceSource::VastOffer,
            assumption: format!(
                "one week of held disk at the instance's storage price ({:.0} GB)",
                parked.disk_space.unwrap_or_default()
            ),
        }),
        format!(
            "instance {} parked: GPUs released, disk held{}",
            id.0,
            weekly
                .map(|w| format!(" at ~${w:.2}/week"))
                .unwrap_or_default()
        ),
    ))?;

    if let Ok(backend) = backend_id(id) {
        let _ = tx.send(Event::BootProgress {
            backend,
            phase: BootPhase::Parked,
            line: weekly.map(|w| format!("disk held at ~${w:.2}/week")),
        });
    }
    broadcast_fleet(api, tx).await;
    Ok(parked)
}

/// Wake a parked box. **Resumes the hourly bill**, which is why there is no path to it
/// without a [`SpendApproval`] — the instance's live `dph_total` is checked against the
/// approved ceiling exactly as `up` checks it.
///
/// A wake that does not reach `running` within `max_boot_secs` is **re-parked**, not
/// abandoned: the GPUs may have been taken by another renter, and leaving `target_state:
/// running` behind means the box would silently start billing at 3 a.m. when they free up.
///
/// # Errors
/// An unknown instance, a rate above the approval, a refused state change, or a box that
/// could not start (re-parked, and the error says so).
pub async fn wake(
    api: &dyn VastApi,
    ledger: &Ledger,
    id: InstanceId,
    approval: SpendApproval,
    tx: &broadcast::Sender<Event>,
    max_boot_secs: u64,
    poll_min_ms: u64,
) -> Result<VastInstance> {
    let inst = api
        .instance(id)
        .await?
        .ok_or_else(|| Error::NotFound(format!("vast instance {}", id.0)))?;
    if let Some(dph) = inst.dph_total {
        if Money::from_usd(dph) > approval.max_usd_per_hour() {
            return Err(Error::Invalid {
                what: "vast instance".to_owned(),
                why: format!(
                    "instance {} bills {}/hr but the approval covers only {}/hr",
                    id.0,
                    Money::from_usd(dph),
                    approval.max_usd_per_hour()
                ),
            });
        }
    }

    api.set_target_state(id, true).await?;

    let poll = Duration::from_millis(poll_min_ms.max(POLL_FLOOR_MS));
    let deadline = Instant::now() + Duration::from_secs(max_boot_secs);
    let mut last: Option<BootPhase> = None;
    let woken = loop {
        match api.instance(id).await {
            Ok(Some(now)) => {
                let phase = now.phase();
                if last.as_ref() != Some(&phase) {
                    if let Ok(backend) = backend_id(id) {
                        let _ = tx.send(Event::BootProgress {
                            backend,
                            phase: phase.clone(),
                            line: now.status_note(),
                        });
                    }
                    last = Some(phase.clone());
                }
                if phase == BootPhase::Healthy {
                    break now;
                }
                if now.is_terminal() {
                    return Err(Error::Other(format!(
                        "instance {} reported `{}` during the wake and will not recover",
                        id.0,
                        now.actual_status.as_deref().unwrap_or("terminal")
                    )));
                }
            }
            Ok(None) => {
                return Err(Error::NotFound(format!(
                    "vast instance {} disappeared during the wake",
                    id.0
                )))
            }
            Err(e) => {
                tracing::warn!(instance = id.0, error = %e, "wake poll failed; retrying");
            }
        }
        if Instant::now() >= deadline {
            // Re-park, or the box starts billing unattended whenever GPUs free up.
            let reparked = api.set_target_state(id, false).await;
            let _ = tx.send(Event::Alert {
                level: AlertLevel::Serious,
                message: match &reparked {
                    Ok(()) => format!(
                        "instance {} did not start within {max_boot_secs}s (GPUs likely \
                         taken) and was parked again",
                        id.0
                    ),
                    Err(e) => format!(
                        "instance {} did not start within {max_boot_secs}s AND could not \
                         be re-parked ({e}) — it may start billing when GPUs free up",
                        id.0
                    ),
                },
                action: Some("apexrouter vast ls".to_owned()),
                id: format!("vast.wake.{}", id.0),
            });
            return Err(Error::Other(format!(
                "instance {} did not reach running within {max_boot_secs}s; {}",
                id.0,
                if reparked.is_ok() {
                    "it was parked again"
                } else {
                    "and re-parking FAILED — check the console"
                }
            )));
        }
        tokio::time::sleep(poll).await;
    };

    ledger.append(&row(
        id,
        LedgerState::Running,
        Some(&woken),
        None,
        CostEstimate::Unknown,
        format!("instance {} woken and observed running", id.0),
    ))?;
    broadcast_fleet(api, tx).await;
    Ok(woken)
}

/// The vast.ai `Provisioner`.
///
/// It deliberately does **not** destroy anything in [`Provisioner::down`]. `down` means "stop
/// routing to it"; a rented box is money, and only an explicit `apexrouter vast destroy` (or
/// `DELETE /v1/vast/instances/{id}?confirm=true`) ends it. Instances are never auto-destroyed
/// on daemon shutdown at any setting — a crash must not delete a paid box, and a stop must
/// not either.
pub struct VastProvisioner {
    api: Arc<dyn VastApi>,
    ledger: Ledger,
    cfg: VastCfg,
    tx: broadcast::Sender<Event>,
}

impl VastProvisioner {
    /// Build one over a REST client, the ledger and the event bus.
    pub fn new(
        api: Arc<dyn VastApi>,
        ledger: Ledger,
        cfg: VastCfg,
        tx: broadcast::Sender<Event>,
    ) -> VastProvisioner {
        VastProvisioner {
            api,
            ledger,
            cfg,
            tx,
        }
    }

    /// The REST client, for the surfaces that browse offers and read the account.
    pub fn api(&self) -> &dyn VastApi {
        self.api.as_ref()
    }

    /// [`rent`], bound to this provisioner's ledger and event bus.
    ///
    /// # Errors
    /// As [`rent`].
    pub async fn rent(&self, req: &RentRequest, approval: SpendApproval) -> Result<InstanceId> {
        rent(self.api.as_ref(), &self.ledger, req, approval, &self.tx).await
    }

    /// [`destroy`], bound to this provisioner's ledger and event bus. **The stop button.**
    ///
    /// # Errors
    /// As [`destroy`].
    pub async fn destroy(&self, id: InstanceId) -> Result<()> {
        destroy(self.api.as_ref(), &self.ledger, id, &self.tx).await
    }

    /// Quote an offer against the live account credit.
    pub async fn preview(&self, offer: &Offer, req: &RentRequest) -> RentPreview {
        let credit = match self.api.account().await {
            Ok(a) => Some(a.credit),
            Err(e) => {
                tracing::warn!(error = %e, "could not read the vast account credit for a preview");
                None
            }
        };
        preview(offer, req, credit, PREVIEW_HOURS)
    }

    /// After a boot that did not end healthy, write down what actually happened to the box.
    async fn settle(&self, id: InstanceId, before: &VastInstance) {
        if !matches!(self.api.instance(id).await, Ok(None)) {
            return;
        }
        let at = chrono::Utc::now().timestamp();
        if let Err(e) = self.ledger.append(&row(
            id,
            LedgerState::Destroyed,
            Some(before),
            Some(at),
            accrued(Some(before)),
            format!("instance {} gone after a failed boot", id.0),
        )) {
            tracing::error!(instance = id.0, error = %e, "could not record the destroyed row");
        }
    }
}

#[async_trait]
impl Provisioner for VastProvisioner {
    fn kind(&self) -> BackendKind {
        BackendKind::VastLlama
    }

    /// What attaching to this instance will do, and what it will cost.
    async fn plan(&self, draft: &EndpointSpec) -> Result<LaunchPlan> {
        let spec = match draft {
            EndpointSpec::Vast(spec) => spec,
            other => {
                return Err(Error::Invalid {
                    what: "endpoint spec".to_owned(),
                    why: format!("{:?} is not a vast endpoint", other.kind()),
                })
            }
        };

        let inst = if spec.instance_id.0 == 0 {
            None
        } else {
            self.api.instance(spec.instance_id).await?
        };
        let dph = inst.as_ref().and_then(|i| i.dph_total);

        let mut warnings = vec![
            "a rented box bills for as long as it exists; stopping the endpoint does NOT \
             destroy it — `apexrouter vast destroy <id> --yes` does"
                .to_owned(),
        ];
        if spec.launch.expose_public {
            warnings.push(
                "expose_public is set: this container port is plaintext HTTP on a shared \
                 public IP and needs a per-instance API key"
                    .to_owned(),
            );
        } else if spec.launch.host != "127.0.0.1" {
            warnings.push(format!(
                "launch host `{}` is overridden to 127.0.0.1: the default posture is \
                 tunnel-only",
                spec.launch.host
            ));
        }
        if let (Ok(account), Some(dph)) = (self.api.account().await, dph) {
            if dph > 0.0 {
                let hours = account.credit / dph;
                if hours < LOW_CREDIT_HOURS {
                    warnings.push(format!(
                        "credit is ${:.2}: about {hours:.1} hours at ${dph:.4}/hr",
                        account.credit
                    ));
                }
            }
        }

        let cost = match dph {
            Some(dph) => CostEstimate::Approximate {
                usd: Money::from_usd(dph * PREVIEW_HOURS),
                source: PriceSource::VastOffer,
                assumption: format!(
                    "one hour at the instance's dph_total of ${dph:.4}/hr; storage and \
                     bandwidth are billed separately"
                ),
            },
            None => CostEstimate::Unknown,
        };

        Ok(LaunchPlan {
            spec: draft.clone(),
            argv: argv_preview(&spec.launch),
            fit: None,
            cost,
            warnings,
            port: spec
                .tunnel
                .as_ref()
                .map_or(spec.launch.port, |t| t.local_port),
        })
    }

    /// Bring an already-created instance up as a routable backend.
    ///
    /// It does **not** create anything: choosing and renting an offer is [`rent`], which
    /// takes a [`RentRequest`] naming the offer the operator saw. An approval is still
    /// required, because attaching to a box means agreeing to keep paying for it, and the
    /// instance's live `dph_total` is checked against the approved ceiling — which is how
    /// attaching to a machine more expensive than the approved one gets caught.
    async fn up(&self, plan: LaunchPlan, approval: Option<SpendApproval>) -> Result<Backend> {
        let spec = match &plan.spec {
            EndpointSpec::Vast(spec) => spec.clone(),
            other => {
                return Err(Error::Invalid {
                    what: "endpoint spec".to_owned(),
                    why: format!("{:?} is not a vast endpoint", other.kind()),
                })
            }
        };
        let approval = approval.ok_or_else(|| Error::Invalid {
            what: "spend approval".to_owned(),
            why: "a rented box bills by the hour; bringing one up requires a SpendApproval"
                .to_owned(),
        })?;
        if spec.instance_id.0 == 0 {
            return Err(Error::Invalid {
                what: "vast spec".to_owned(),
                why: "instance id 0 names no contract; rent an offer with `rent` first".to_owned(),
            });
        }

        let inst = self
            .api
            .instance(spec.instance_id)
            .await?
            .ok_or_else(|| Error::NotFound(format!("vast instance {}", spec.instance_id.0)))?;
        if let Some(dph) = inst.dph_total {
            if Money::from_usd(dph) > approval.max_usd_per_hour() {
                return Err(Error::Invalid {
                    what: "vast instance".to_owned(),
                    why: format!(
                        "instance {} bills {}/hr but the approval covers only {}/hr",
                        spec.instance_id.0,
                        Money::from_usd(dph),
                        approval.max_usd_per_hour()
                    ),
                });
            }
        }

        let phase = watch_boot_every(
            self.api.as_ref(),
            spec.instance_id,
            self.cfg.max_boot_secs,
            self.cfg.poll_min_ms,
            &self.tx,
        )
        .await?;
        if phase != BootPhase::Healthy {
            // The watchdog may have destroyed it; record what the fleet says rather than
            // guessing.
            self.settle(spec.instance_id, &inst).await;
            return Err(Error::Other(format!(
                "vast instance {} did not become healthy: {phase:?}",
                spec.instance_id.0
            )));
        }

        self.ledger.append(&row(
            spec.instance_id,
            LedgerState::Running,
            Some(&inst),
            None,
            CostEstimate::Unknown,
            format!("instance {} observed running", spec.instance_id.0),
        ))?;

        rented_backend(&inst, &spec)
    }

    /// Stop routing to a rented box. **Never destroys it.**
    ///
    /// Destruction is irreversible and costs the operator a machine; it is one explicit verb
    /// away and never a side effect of stopping, draining or forgetting an endpoint. A
    /// `Warning` alert names the instance that is still billing and the command that ends it.
    async fn down(&self, id: &BackendId, mode: DownMode) -> Result<()> {
        let instance = instance_of(id)?;
        let dph = match self.api.instance(instance).await {
            Ok(Some(inst)) => inst.dph_total,
            _ => None,
        };
        let rate = dph.map_or_else(|| "an unknown rate".to_owned(), |d| format!("${d:.4}/hr"));
        tracing::info!(
            backend = id.as_str(),
            instance = instance.0,
            ?mode,
            "taking a vast backend out of the routing table; the instance is NOT destroyed"
        );
        let _ = self.tx.send(Event::Alert {
            level: AlertLevel::Warning,
            message: format!(
                "vast instance {} is out of the routing table but still billing at {rate}",
                instance.0
            ),
            action: Some(format!("apexrouter vast destroy {} --yes", instance.0)),
            id: format!("vast.still-billing.{}", instance.0),
        });
        Ok(())
    }

    /// The last `tail` lines of the container log, through vast's two-phase log fetch.
    async fn logs(&self, id: &BackendId, tail: usize) -> Result<Vec<String>> {
        let instance = instance_of(id)?;
        let tail = u32::try_from(tail).unwrap_or(u32::MAX);
        self.api.logs(instance, tail).await
    }

    /// Re-derive rented backends from the live fleet.
    ///
    /// An unreachable API propagates its error and leaves every ledger row untouched: being
    /// offline is never evidence that a box was destroyed.
    async fn reconcile(&self) -> Result<Vec<Backend>> {
        let instances = self.api.instances().await?;
        let mut out = Vec::new();
        for inst in &instances {
            let id = match backend_id(inst.id) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(instance = inst.id.0, error = %e, "unusable instance id");
                    continue;
                }
            };
            let (host, port) = inst
                .external_port(8000)
                .unwrap_or_else(|| ("127.0.0.1".to_owned(), 8000));
            out.push(Backend {
                id: id.clone(),
                kind: BackendKind::VastLlama,
                protocol: Protocol::OpenAi,
                label: inst
                    .label
                    .clone()
                    .unwrap_or_else(|| format!("vast {}", inst.id.0)),
                base_url: format!("http://{host}:{port}"),
                credential: CredentialSource::None,
                tags: tags_for(inst),
                models: Vec::new(),
                limits: BackendLimits::default(),
                price: inst.dph_total.map(|dph| PriceModel::PerHour {
                    dph: Money::from_usd(dph),
                }),
                health: Health::Unknown,
                provenance: Provenance::Rented,
                endpoint: Some(EndpointRef {
                    id,
                    kind: BackendKind::VastLlama,
                }),
                enabled: true,
                devices: Vec::new(),
                last_error: None,
            });
        }
        Ok(out)
    }
}

/// The `Backend` row a rented instance serves as.
///
/// **The one constructor**, used by [`Provisioner::up`] on this type and by the daemon's
/// rent job when it finishes the chain — two hand-built `Backend`s for the same box would
/// eventually disagree about the base URL, and the request path would route to the wrong
/// one. Tunnel-first: with a [`TunnelSpec`] the backend is `http://127.0.0.1:<local>`,
/// otherwise the instance must map the launch port publicly.
///
/// # Errors
/// A launch port that is neither tunnelled nor mapped, or an instance id no `BackendId`
/// charset admits.
pub fn rented_backend(inst: &VastInstance, spec: &VastSpec) -> Result<Backend> {
    let id = backend_id(spec.instance_id)?;
    let kind = EndpointSpec::Vast(spec.clone()).kind();
    let base_url = match &spec.tunnel {
        Some(t) => format!("http://127.0.0.1:{}", t.local_port),
        None => {
            let (host, port) =
                inst.external_port(spec.launch.port)
                    .ok_or_else(|| Error::Invalid {
                        what: "vast instance".to_owned(),
                        why: format!(
                            "port {} is not mapped and no tunnel is configured",
                            spec.launch.port
                        ),
                    })?;
            format!("http://{host}:{port}")
        }
    };
    Ok(Backend {
        id: id.clone(),
        kind,
        protocol: Protocol::OpenAi,
        label: inst.gpu_name.clone().map_or_else(
            || format!("vast {}", spec.instance_id.0),
            |gpu| format!("{gpu} on vast {}", spec.instance_id.0),
        ),
        base_url,
        credential: if spec.launch.expose_public {
            CredentialSource::Instance
        } else {
            CredentialSource::None
        },
        tags: tags_for(inst),
        models: Vec::new(),
        limits: BackendLimits::default(),
        price: inst.dph_total.map(|dph| PriceModel::PerHour {
            dph: Money::from_usd(dph),
        }),
        health: Health::Unknown,
        provenance: Provenance::Rented,
        endpoint: Some(EndpointRef { id, kind }),
        enabled: true,
        devices: Vec::new(),
        last_error: None,
    })
}

/// Force the tunnel-only posture at create time (`ARCHITECTURE.md` §9.5).
///
/// `launch_vllm.sh`'s own default is `0.0.0.0`, so the value is overwritten rather than
/// defaulted — and it is overwritten again on every stall restart.
fn hardened(launch: &ContainerLaunch) -> ContainerLaunch {
    let mut launch = launch.clone();
    if !launch.expose_public {
        launch.host = "127.0.0.1".to_owned();
        launch.env.insert("HOST".to_owned(), "127.0.0.1".to_owned());
    }
    launch
}

/// The label vast stores on the contract, so a box is recognisable as ours in the console.
fn label_for(req: &RentRequest) -> String {
    match (req.bind_alias.as_ref(), req.profile.as_ref()) {
        (Some(alias), _) => format!("apexrouter:{alias}"),
        (None, Some(profile)) => format!("apexrouter:{profile}"),
        (None, None) => "apexrouter".to_owned(),
    }
}

/// The container contract, rendered for the confirm dialog. **No credential appears here.**
fn argv_preview(launch: &ContainerLaunch) -> ArgvPreview {
    ArgvPreview {
        program: launch.image.clone(),
        args: vec!["--onstart".to_owned(), launch.onstart.clone()],
        env: launch
            .env
            .iter()
            .map(|(k, v)| {
                let upper = k.to_ascii_uppercase();
                let value = if SECRET_ENV.iter().any(|s| upper.contains(s)) {
                    "<from credentials.toml>".to_owned()
                } else {
                    v.clone()
                };
                (k.clone(), value)
            })
            .collect(),
        cwd: "/".to_owned(),
        warnings: Vec::new(),
    }
}

/// `vast-<id>` back to the contract id it names.
fn instance_of(id: &BackendId) -> Result<InstanceId> {
    id.as_str()
        .strip_prefix("vast-")
        .and_then(|n| n.parse::<u64>().ok())
        .map(InstanceId)
        .ok_or_else(|| Error::Invalid {
            what: "backend id".to_owned(),
            why: format!("`{id}` does not name a vast instance"),
        })
}

/// Tags every surface filters on.
fn tags_for(inst: &VastInstance) -> Vec<String> {
    let mut tags = vec!["rented".to_owned(), "vast".to_owned()];
    if let Some(gpu) = &inst.gpu_name {
        tags.push(format!(
            "gpu:{}",
            gpu.to_ascii_lowercase().replace(' ', "-")
        ));
    }
    tags
}

/// One follow-up ledger row for an instance whose id is already known.
fn row(
    id: InstanceId,
    state: LedgerState,
    inst: Option<&VastInstance>,
    destroyed_at_unix: Option<i64>,
    est_cost: CostEstimate,
    note: String,
) -> LedgerRow {
    LedgerRow {
        seq: 0, // assigned by `Ledger::append`
        at_unix: chrono::Utc::now().timestamp(),
        instance_id: Some(id),
        state,
        offer_id: None,
        profile: None,
        gpu: inst.and_then(|i| i.gpu_name.clone()),
        num_gpus: inst.and_then(|i| i.num_gpus),
        dph: inst.and_then(|i| i.dph_total),
        approved_max_dph: None,
        approval_source: None,
        destroyed_at_unix,
        est_cost,
        note: Some(note),
    }
}

/// What the box actually cost, from its own `dph_total` and uptime.
fn accrued(inst: Option<&VastInstance>) -> CostEstimate {
    match (
        inst.and_then(|i| i.dph_total),
        inst.and_then(VastInstance::uptime_secs),
    ) {
        (Some(dph), Some(secs)) => {
            let hours = secs / 3600.0;
            CostEstimate::Approximate {
                usd: Money::from_usd(dph * hours),
                source: PriceSource::VastOffer,
                assumption: format!(
                    "${dph:.4}/hr for {hours:.2} h of uptime; vast publishes no per-instance \
                     invoice, so storage and bandwidth are not included"
                ),
            }
        }
        _ => CostEstimate::Unknown,
    }
}

/// Push the fleet and the credit to every surface. Best effort — a failure here must never
/// turn a successful rental into an error.
async fn broadcast_fleet(api: &dyn VastApi, tx: &broadcast::Sender<Event>) {
    let instances = match api.instances().await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "could not refresh the vast fleet");
            return;
        }
    };
    let credit = api.account().await.ok().map(|a| a.credit);
    let _ = tx.send(Event::VastFleetChanged { instances, credit });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::vast::api::{FixtureCall, FixtureVast};
    use apexrouter_core::money::ApprovalSource;
    use apexrouter_core::paths::Paths;
    use apexrouter_protocol::{
        Alias, ContainerRuntime, ImageType, OfferQuery, OfferSearchResult, ProfileId, TunnelSpec,
        VastAccount, VastSpec,
    };
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};

    // ---- the mocks ---------------------------------------------------------------------
    //
    // The money path is proved against P-02's [`FixtureVast`], which replays the recorded
    // 2026-07-30 market and has a `panicking_on_create` mode built for exactly this unit's
    // acceptance. `MockVast` below covers what a replaying fixture cannot express — a poll
    // that errors transiently, a `destroy` that is refused, an instance that survives one —
    // and records poll timestamps so the watchdog's pacing is assertable. NOTHING in this
    // module touches the network: every call is answered from memory.

    /// The recorded RTX 3090 pair: `$0.305/hr`, Czechia. See `FixtureVast::recorded`.
    const RECORDED_OFFER: u64 = 43_731_729;
    /// The recorded 2xH100: `$3.344/hr` — 2.3 hours of Andre's real `$7.729`.
    const RECORDED_H100: u64 = 43_731_730;

    /// A scripted [`VastApi`] for the cases a replaying fixture cannot express: a poll that
    /// errors transiently, a `destroy` that is refused, an instance that survives one. It
    /// also timestamps every poll, which is how the watchdog's pacing is asserted.
    ///
    /// **It cannot create anything.** `create` returns an error, so a test that reaches the
    /// billing path by accident fails loudly instead of quietly pretending.
    pub(crate) struct MockVast {
        statuses: Mutex<Vec<Option<String>>>,
        stuck: Option<Option<String>>,
        /// One `status_msg` per poll, the last repeating forever. Empty means `None` always.
        msgs: Mutex<std::collections::VecDeque<Option<String>>>,
        /// Every `set_target_state` call, in order (`true` = wake, `false` = park).
        set_state_calls: Mutex<Vec<bool>>,
        errors_before: usize,
        destroy_ok: bool,
        instance_calls: AtomicUsize,
        destroy_calls: AtomicUsize,
        polls: Mutex<Vec<Instant>>,
        gone_after_destroy: bool,
    }

    /// `Mutex` poisoning is not interesting in a test: take the value either way.
    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        match m.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    impl MockVast {
        pub(crate) fn new() -> MockVast {
            MockVast {
                statuses: Mutex::new(Vec::new()),
                stuck: None,
                msgs: Mutex::new(std::collections::VecDeque::new()),
                set_state_calls: Mutex::new(Vec::new()),
                errors_before: 0,
                destroy_ok: true,
                instance_calls: AtomicUsize::new(0),
                destroy_calls: AtomicUsize::new(0),
                polls: Mutex::new(Vec::new()),
                gone_after_destroy: true,
            }
        }

        /// One `status_msg` per `instance()` answer, the last repeating forever.
        pub(crate) fn with_msg_script(self, msgs: &[Option<&str>]) -> MockVast {
            *lock(&self.msgs) = msgs.iter().map(|m| m.map(str::to_owned)).collect();
            self
        }

        /// One `instance()` answer per entry; `None` means "gone from the fleet".
        pub(crate) fn with_statuses(self, statuses: &[Option<&str>]) -> MockVast {
            *lock(&self.statuses) = statuses
                .iter()
                .rev()
                .map(|s| s.map(str::to_owned))
                .collect();
            self
        }

        /// Always answer with this status, forever.
        pub(crate) fn stuck(mut self, status: Option<&str>) -> MockVast {
            self.stuck = Some(status.map(str::to_owned));
            self
        }

        /// The first `n` `instance()` calls fail, then the script runs.
        pub(crate) fn errors_then(mut self, n: usize, statuses: &[Option<&str>]) -> MockVast {
            self.errors_before = n;
            self.with_statuses(statuses)
        }

        pub(crate) fn destroy_fails(mut self) -> MockVast {
            self.destroy_ok = false;
            self
        }

        /// The instance survives the destroy call — an unverifiable destroy.
        pub(crate) fn survives_destroy(mut self) -> MockVast {
            self.gone_after_destroy = false;
            self
        }

        pub(crate) fn destroyed(&self) -> bool {
            self.destroy_calls.load(Ordering::SeqCst) > 0
        }

        pub(crate) fn instance_calls(&self) -> usize {
            self.instance_calls.load(Ordering::SeqCst)
        }

        pub(crate) fn poll_gaps(&self) -> Vec<Duration> {
            let polls = lock(&self.polls);
            polls.windows(2).map(|w| w[1] - w[0]).collect()
        }

        pub(crate) fn set_state_calls(&self) -> Vec<bool> {
            lock(&self.set_state_calls).clone()
        }
    }

    /// A rented box with everything the fleet view renders.
    pub(crate) fn instance(id: u64, status: Option<&str>) -> VastInstance {
        VastInstance {
            id: InstanceId(id),
            actual_status: status.map(str::to_owned),
            intended_status: None,
            status_msg: None,
            machine_id: Some(142_595),
            storage_cost: Some(0.15),
            ssh_host: Some("ssh5.vast.ai".to_owned()),
            ssh_port: Some(41_234),
            public_ipaddr: Some("203.0.113.7".to_owned()),
            ports: serde_json::json!({"8000/tcp": [{"HostIp": "0.0.0.0", "HostPort": "41235"}]}),
            direct_port_start: None,
            direct_port_end: None,
            gpu_name: Some("RTX 3090".to_owned()),
            num_gpus: Some(2),
            gpu_util: None,
            dph_total: Some(0.3012),
            geolocation: Some("Czechia, CZ".to_owned()),
            label: Some("apexrouter:big".to_owned()),
            start_date: Some(chrono::Utc::now().timestamp() as f64 - 7_200.0),
            disk_util: None,
            disk_space: Some(80.0),
            inet_down: Some(900.0),
            extra: serde_json::Map::new(),
        }
    }

    #[async_trait]
    impl VastApi for MockVast {
        async fn account(&self) -> Result<VastAccount> {
            Ok(VastAccount {
                id: 291_079,
                credit: 7.729,
                balance: Some(0.0),
                can_pay: Some(true),
                has_billing: Some(true),
            })
        }

        async fn search(&self, _q: &OfferQuery) -> Result<OfferSearchResult> {
            Ok(OfferSearchResult::default())
        }

        /// **Never** creates. The money path is `FixtureVast`'s; reaching this is a bug in
        /// the test, and it says so rather than returning a plausible id.
        async fn create(
            &self,
            _offer_id: u64,
            _launch: &ContainerLaunch,
            _label: &str,
        ) -> Result<InstanceId> {
            Err(Error::Other(
                "MockVast never creates: use FixtureVast for the money path".to_owned(),
            ))
        }

        async fn instances(&self) -> Result<Vec<VastInstance>> {
            Ok(vec![instance(28_675_431, Some("running"))])
        }

        async fn instance(&self, id: InstanceId) -> Result<Option<VastInstance>> {
            let n = self.instance_calls.fetch_add(1, Ordering::SeqCst);
            lock(&self.polls).push(Instant::now());
            if n < self.errors_before {
                return Err(Error::Other("connection reset".to_owned()));
            }
            if self.gone_after_destroy && self.destroy_calls.load(Ordering::SeqCst) > 0 {
                return Ok(None);
            }
            let msg = {
                let mut msgs = lock(&self.msgs);
                if msgs.len() > 1 {
                    msgs.pop_front().flatten()
                } else {
                    msgs.front().cloned().flatten()
                }
            };
            let with_msg = |mut inst: VastInstance| {
                inst.status_msg = msg.clone();
                inst
            };
            if let Some(stuck) = &self.stuck {
                return Ok(Some(with_msg(instance(id.0, stuck.as_deref()))));
            }
            match lock(&self.statuses).pop() {
                Some(Some(status)) => Ok(Some(with_msg(instance(id.0, Some(&status))))),
                Some(None) => Ok(None),
                None => Ok(Some(with_msg(instance(id.0, Some("running"))))),
            }
        }

        async fn set_target_state(&self, _id: InstanceId, running: bool) -> Result<()> {
            // The status script drives what `instance()` reports afterwards: a park test
            // scripts `stopped`, a wake scripts the boot ladder. Recorded so a test can
            // assert a failed wake re-parked.
            lock(&self.set_state_calls).push(running);
            Ok(())
        }

        async fn destroy(&self, _id: InstanceId) -> Result<()> {
            self.destroy_calls.fetch_add(1, Ordering::SeqCst);
            if self.destroy_ok {
                Ok(())
            } else {
                Err(Error::Other("vast returned 403".to_owned()))
            }
        }

        async fn logs(&self, _id: InstanceId, tail: u32) -> Result<Vec<String>> {
            Ok((0..tail.min(3)).map(|n| format!("line {n}")).collect())
        }

        async fn exec(&self, _id: InstanceId, cmd: &str) -> Result<String> {
            Ok(cmd.to_owned())
        }
    }

    // ---- fixtures ----------------------------------------------------------------------

    /// `Paths::resolve` reads the process environment, which is global. One lock, and the
    /// variable is restored before it is released.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// A ledger rooted in a temp dir. Never `~/.vastai-gguf`, never `$STATE`.
    fn ledger(dir: &Path) -> Ledger {
        let guard = lock(env_lock());
        let previous = std::env::var_os("APEXROUTER_HOME");
        std::env::set_var("APEXROUTER_HOME", dir);
        let paths = Paths::resolve();
        match previous {
            Some(v) => std::env::set_var("APEXROUTER_HOME", v),
            None => std::env::remove_var("APEXROUTER_HOME"),
        }
        drop(guard);
        Ledger::open(&paths.expect("resolve paths")).expect("open ledger")
    }

    pub(crate) fn launch() -> ContainerLaunch {
        let mut env = std::collections::BTreeMap::new();
        env.insert("MODEL_REPO".to_owned(), "unsloth/Qwen3-8B-GGUF".to_owned());
        env.insert("HF_TOKEN".to_owned(), "hf_secret_value".to_owned());
        env.insert("HOST".to_owned(), "0.0.0.0".to_owned());
        ContainerLaunch {
            runtime: ContainerRuntime::LlamaCpp,
            image: "buckster123/vastai-gguf:latest".to_owned(),
            image_type: ImageType::Prebuilt,
            disk_gb: 80,
            env,
            onstart: "bash /app/launch.sh > /var/log/launch.log 2>&1 &".to_owned(),
            host: "0.0.0.0".to_owned(),
            port: 8000,
            expose_public: false,
        }
    }

    pub(crate) fn request() -> RentRequest {
        RentRequest {
            profile: Some(ProfileId::parse("rtx3090").expect("id")),
            offer_id: Some(RECORDED_OFFER),
            launch: launch(),
            confirm: true,
            max_usd_per_hour: 0.40,
            auto_tunnel: true,
            bind_alias: Some(Alias::parse("big").expect("alias")),
        }
    }

    fn approval(usd: f64) -> SpendApproval {
        SpendApproval::confirm(
            Money::from_usd(usd),
            ApprovalSource::Cli,
            &VastCfg::default(),
            None,
        )
        .expect("under the default ceiling")
    }

    /// The recorded RTX 3090 pair, price and all. Same row `FixtureVast::recorded` replays.
    fn offer() -> Offer {
        Offer {
            id: RECORDED_OFFER,
            ask_contract_id: None,
            machine_id: None,
            gpu_name: "RTX 3090".to_owned(),
            num_gpus: 2,
            gpu_ram: 24_576,
            gpu_total_ram: 49_152,
            dph_total: 0.305,
            dph_base: None,
            storage_cost: None,
            inet_down_cost: None,
            inet_up_cost: None,
            cpu_ram: None,
            cpu_cores_effective: None,
            cpu_name: None,
            mobo_name: None,
            host_id: None,
            disk_space: Some(80.0),
            cuda_max_good: Some(12.4),
            driver_version: None,
            geolocation: Some("Czechia, CZ".to_owned()),
            inet_down: Some(900.0),
            inet_up: None,
            reliability2: Some(0.9871),
            direct_port_count: Some(8),
            static_ip: None,
            rented: Some(false),
            rentable: Some(true),
            dlperf: None,
            dlperf_per_dphtotal: None,
            duration: None,
            end_date: None,
            extra: serde_json::Map::new(),
        }
    }

    /// The recorded 2xH100. The offer that burns Andre's balance in 2.3 hours.
    fn h100_offer() -> Offer {
        Offer {
            id: RECORDED_H100,
            gpu_name: "H100 SXM".to_owned(),
            num_gpus: 2,
            gpu_ram: 81_559,
            gpu_total_ram: 163_118,
            dph_total: 3.344,
            geolocation: Some("Montana, US".to_owned()),
            ..offer()
        }
    }

    fn channel() -> broadcast::Sender<Event> {
        broadcast::channel(64).0
    }

    fn alerts(rx: &mut broadcast::Receiver<Event>) -> Vec<(AlertLevel, String, Option<String>)> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                Event::Alert {
                    level,
                    message,
                    action,
                    ..
                } => Some((level, message, action)),
                _ => None,
            })
            .collect()
    }

    // ---- the money path ----------------------------------------------------------------
    //
    // Everything below runs against P-02's [`FixtureVast`], replaying the recorded
    // 2026-07-30 market. Nothing is created, nothing is billed, nothing leaves the machine.

    #[tokio::test]
    async fn a_rental_reserves_before_it_bills_and_confirms_after() {
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = FixtureVast::recorded();
        let tx = channel();

        let id = rent(&api, &ledger, &request(), approval(0.40), &tx)
            .await
            .expect("rent");
        assert_eq!(id, InstanceId(28_675_431));

        let rows = ledger.rows().expect("rows");
        assert_eq!(rows.len(), 2, "{rows:#?}");
        assert_eq!(rows[0].state, LedgerState::Reserved);
        assert_eq!(rows[0].instance_id, None, "the id does not exist yet");
        assert!(rows[0].at_unix <= rows[1].at_unix);
        assert_eq!(rows[1].state, LedgerState::Confirmed);
        assert_eq!(rows[1].instance_id, Some(id));
        assert_eq!(ledger.active().expect("active").len(), 1);

        // The same claim from the fixture's side: exactly one create, for the offer asked.
        assert!(api.calls().contains(&FixtureCall::Create {
            offer_id: RECORDED_OFFER,
            label: "apexrouter:big".to_owned(),
        }));
        assert_eq!(api.created().len(), 1);
    }

    #[tokio::test]
    async fn rent_refuses_an_offer_whose_rate_exceeds_the_approval() {
        // Approval $0.40, H100 at $3.344/hr — the classic hollow-approval hole.
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = FixtureVast::recorded();
        let tx = channel();

        let mut req = request();
        req.offer_id = Some(RECORDED_H100);
        req.max_usd_per_hour = 0.40;

        let err = rent(&api, &ledger, &req, approval(0.40), &tx)
            .await
            .expect_err("a $3.34/hr offer must not clear a $0.40 approval");
        assert!(
            err.to_string().contains("3.344") || err.to_string().contains("3.34"),
            "error must name the offer rate: {err}"
        );
        assert!(
            ledger.rows().expect("rows").is_empty(),
            "nothing may be reserved when the rate check fails"
        );
        assert!(
            !api.calls()
                .iter()
                .any(|c| matches!(c, FixtureCall::Create { .. })),
            "create must not run"
        );
    }

    #[tokio::test]
    async fn a_create_that_panics_still_leaves_the_reserved_row() {
        // The acceptance case, against the fixture's `panicking_on_create` mode. A `SIGKILL`
        // skips `Drop`, so the row written BEFORE the billing call is the real protection; a
        // panic is the strongest thing a test can simulate that still lets it read the file
        // afterwards.
        let dir = tempfile::tempdir().expect("tmp");
        let ledger_for_task = ledger(dir.path());
        let tx = channel();

        let joined = tokio::spawn(async move {
            let api = FixtureVast::recorded().panicking_on_create();
            rent(&api, &ledger_for_task, &request(), approval(0.40), &tx).await
        })
        .await;
        assert!(joined.is_err(), "the create call must have panicked");

        let ledger = ledger(dir.path());
        let rows = ledger.rows().expect("rows");
        assert_eq!(
            rows.first().map(|r| r.state),
            Some(LedgerState::Reserved),
            "the reservation must survive a create that never returned: {rows:#?}"
        );
        // Unwinding also runs the guard, so the reservation is resolved as an orphan.
        assert_eq!(
            rows.last().map(|r| r.state),
            Some(LedgerState::OrphanSuspect)
        );
        assert_eq!(
            ledger.active().expect("active").len(),
            1,
            "an unresolved reservation stays visible"
        );
    }

    #[tokio::test]
    async fn a_create_that_errors_leaves_an_orphan_suspect_and_a_critical_alert() {
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = FixtureVast::recorded().failing_create("vast returned 500");
        let tx = channel();
        let mut rx = tx.subscribe();

        let err = rent(&api, &ledger, &request(), approval(0.40), &tx)
            .await
            .expect_err("the create failed");
        assert!(err.to_string().contains("500"), "{err}");

        let rows = ledger.rows().expect("rows");
        assert_eq!(rows[0].state, LedgerState::Reserved);
        assert_eq!(rows[1].state, LedgerState::OrphanSuspect);

        let raised = alerts(&mut rx);
        assert_eq!(raised.len(), 1, "{raised:?}");
        assert_eq!(raised[0].0, AlertLevel::Critical);
        assert_eq!(raised[0].2.as_deref(), Some("apexrouter vast ls --orphans"));
    }

    #[tokio::test]
    async fn a_request_above_its_own_approval_never_reaches_the_create_call() {
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = FixtureVast::recorded();
        let tx = channel();

        let mut req = request();
        req.max_usd_per_hour = 3.34;
        let err = rent(&api, &ledger, &req, approval(0.40), &tx)
            .await
            .expect_err("the approval covers $0.40");
        assert!(err.to_string().contains("approval covers"), "{err}");

        assert!(api.created().is_empty(), "nothing may be created");
        assert!(api.calls().is_empty(), "not one call left the machine");
        assert!(
            ledger.rows().expect("rows").is_empty(),
            "and nothing is reserved either"
        );
    }

    #[tokio::test]
    async fn an_unconfirmed_request_is_a_preview_not_a_rental() {
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = FixtureVast::recorded();
        let tx = channel();

        let mut req = request();
        req.confirm = false;
        let err = rent(&api, &ledger, &req, approval(0.40), &tx)
            .await
            .expect_err("confirm is false");
        assert!(err.to_string().contains("confirm"), "{err}");
        assert!(api.created().is_empty());
        assert!(api.calls().is_empty());
        assert!(ledger.rows().expect("rows").is_empty());
    }

    #[tokio::test]
    async fn renting_without_an_offer_id_refuses_rather_than_searching_on_its_own() {
        // The whole point of the single search path: the offer rented is the offer seen.
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = FixtureVast::recorded();
        let tx = channel();

        let mut req = request();
        req.offer_id = None;
        let err = rent(&api, &ledger, &req, approval(0.40), &tx)
            .await
            .expect_err("no offer");
        assert!(err.to_string().contains("search_unified"), "{err}");
        assert!(
            !api.calls().iter().any(|c| matches!(c, FixtureCall::Search)),
            "rent must never run a search of its own"
        );
        assert!(ledger.rows().expect("rows").is_empty());
    }

    #[tokio::test]
    async fn the_create_call_forces_the_tunnel_only_posture() {
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = FixtureVast::recorded();
        let tx = channel();

        rent(&api, &ledger, &request(), approval(0.40), &tx)
            .await
            .expect("rent");

        let created = api.created();
        let create = created.first().expect("one create call");
        assert_eq!(create.offer_id, RECORDED_OFFER);
        assert_eq!(
            create.launch.host, "127.0.0.1",
            "0.0.0.0 must never survive"
        );
        assert_eq!(
            create.launch.env.get("HOST").map(String::as_str),
            Some("127.0.0.1")
        );
        assert_eq!(create.label, "apexrouter:big");
    }

    #[test]
    fn an_exposed_instance_keeps_the_host_it_asked_for() {
        let mut l = launch();
        l.expose_public = true;
        l.host = "0.0.0.0".to_owned();
        assert_eq!(hardened(&l).host, "0.0.0.0");
        // …and the default posture is still forced when it is not opted out of.
        assert_eq!(hardened(&launch()).host, "127.0.0.1");
    }

    #[test]
    fn the_label_names_the_alias_then_the_profile() {
        assert_eq!(label_for(&request()), "apexrouter:big");
        let mut req = request();
        req.bind_alias = None;
        assert_eq!(label_for(&req), "apexrouter:rtx3090");
        req.profile = None;
        assert_eq!(label_for(&req), "apexrouter");
    }

    // ---- the preview -------------------------------------------------------------------

    #[test]
    fn a_preview_shows_the_rate_the_total_and_the_credit() {
        let p = preview(&offer(), &request(), Some(7.729), 1.0);
        assert!((p.dph_total - 0.305).abs() < 1e-9);
        assert!((p.est_total_usd - 0.305).abs() < 1e-9);
        assert_eq!(p.credit, Some(7.729));
        assert!((p.burn_down_hours.expect("burn down") - 25.34).abs() < 0.01);
        assert!((p.max_usd_per_hour - 0.40).abs() < 1e-9);
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);

        // It survives the wire, because the 409 body carries it.
        let json = serde_json::to_string(&p).expect("ser");
        assert_eq!(serde_json::from_str::<RentPreview>(&json).expect("de"), p);
    }

    #[test]
    fn a_metered_host_is_named_in_the_quote_and_a_home_box_is_not() {
        // The Taiwan rate that turned $0.60 of box-hours into a $3.80 leg.
        let mut o = offer();
        o.inet_down_cost = Some(0.0091);
        let p = preview(&o, &request(), Some(10.0), 1.0);
        assert!(
            p.warnings
                .iter()
                .any(|w| w.contains("metered inbound") && w.contains("$9.10/TB")),
            "{:?}",
            p.warnings
        );

        // The Poland home-box rate: ~$0.13/TB is nobody's surprise.
        o.inet_down_cost = Some(0.00013);
        let p = preview(&o, &request(), Some(10.0), 1.0);
        assert!(
            !p.warnings.iter().any(|w| w.contains("metered")),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn a_two_h100_box_against_the_real_balance_warns_before_it_is_confirmed() {
        // The recorded 2xH100 at $3.344/hr against the real $7.729 of credit is 2.3 hours.
        // That is the number the operator has to see, in words, before the button does
        // anything.
        let mut req = request();
        req.offer_id = Some(RECORDED_H100);
        req.max_usd_per_hour = 4.00;

        let p = preview(&h100_offer(), &req, Some(7.729), 1.0);
        assert!((p.burn_down_hours.expect("burn down") - 2.311).abs() < 0.01);
        assert!((p.est_total_usd - 3.344).abs() < 1e-9);
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
        assert!(p.warnings[0].contains("2.3 hours"), "{:?}", p.warnings);
        assert!(p.warnings[0].contains("$7.73"), "{:?}", p.warnings);
    }

    #[test]
    fn a_preview_flags_an_offer_above_the_ceiling_and_credit_that_cannot_cover_an_hour() {
        let mut o = offer();
        o.dph_total = 3.34;
        let p = preview(&o, &request(), Some(1.10), 1.0);
        assert!(
            p.warnings.iter().any(|w| w.contains("above the")),
            "{:?}",
            p.warnings
        );
        assert!(
            p.warnings
                .iter()
                .any(|w| w.contains("does not cover one hour")),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn an_unreadable_credit_is_said_out_loud_rather_than_shown_as_zero() {
        let p = preview(&offer(), &request(), None, 1.0);
        assert_eq!(p.credit, None);
        assert_eq!(p.burn_down_hours, None);
        assert!(
            p.warnings.iter().any(|w| w.contains("could not be read")),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn a_preview_of_an_unconfirmed_request_says_nothing_has_been_rented() {
        let mut req = request();
        req.confirm = false;
        req.launch.expose_public = true;
        let p = preview(&offer(), &req, Some(7.729), 4.0);
        assert!((p.est_total_usd - 1.22).abs() < 1e-9, "{}", p.est_total_usd);
        assert!(p
            .warnings
            .iter()
            .any(|w| w.contains("nothing has been rented")));
        assert!(p.warnings.iter().any(|w| w.contains("expose_public")));
        // A nonsense window falls back to the quoted hour rather than producing a NaN.
        assert!((preview(&offer(), &req, None, f64::NAN).est_hours - 1.0).abs() < 1e-9);
        assert!((preview(&offer(), &req, None, -3.0).est_hours - 1.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn the_provisioner_preview_reads_the_live_credit() {
        let dir = tempfile::tempdir().expect("tmp");
        let prov = provisioner(FixtureVast::recorded(), dir.path());
        let p = prov.preview(&offer(), &request()).await;
        assert_eq!(p.credit, Some(7.729), "the live credit, not a constant");
    }

    // ---- destroy -----------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn destroy_verifies_before_it_forgets() {
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = MockVast::new();
        let tx = channel();

        destroy_within(&api, &ledger, InstanceId(28_675_431), &tx, 60, 1_000)
            .await
            .expect("destroy");

        let rows = ledger.rows().expect("rows");
        assert_eq!(rows.len(), 2, "{rows:#?}");
        assert_eq!(rows[0].state, LedgerState::DestroyRequested);
        assert_eq!(rows[1].state, LedgerState::Destroyed);
        assert!(rows[1].destroyed_at_unix.is_some());
        match &rows[1].est_cost {
            CostEstimate::Approximate { usd, .. } => {
                // Two hours of uptime at $0.3012/hr.
                assert!((usd.as_usd() - 0.6024).abs() < 0.01, "{usd}");
            }
            other => panic!("expected an accrued cost, got {other:?}"),
        }
        assert!(ledger.active().expect("active").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_destroy_that_cannot_be_verified_is_not_recorded_as_done() {
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = MockVast::new().survives_destroy();
        let tx = channel();
        let mut rx = tx.subscribe();

        let err = destroy_within(&api, &ledger, InstanceId(28_675_431), &tx, 10, 1_000)
            .await
            .expect_err("it never went away");
        assert!(err.to_string().contains("could not be verified"), "{err}");

        let rows = ledger.rows().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, LedgerState::DestroyRequested);
        assert_eq!(
            ledger.active().expect("active").len(),
            1,
            "an unverified destroy keeps the box on the books"
        );
        assert_eq!(
            alerts(&mut rx).first().map(|a| a.0),
            Some(AlertLevel::Serious)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_destroy_call_that_fails_leaves_the_intent_on_the_record() {
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = MockVast::new().destroy_fails();
        let tx = channel();

        destroy_within(&api, &ledger, InstanceId(28_675_431), &tx, 10, 1_000)
            .await
            .expect_err("403");
        let rows = ledger.rows().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, LedgerState::DestroyRequested);
    }

    // ---- the provisioner ---------------------------------------------------------------

    fn provisioner(api: impl VastApi + 'static, dir: &Path) -> VastProvisioner {
        VastProvisioner::new(Arc::new(api), ledger(dir), VastCfg::default(), channel())
    }

    fn vast_spec(instance_id: u64, tunnel: bool) -> EndpointSpec {
        EndpointSpec::Vast(VastSpec {
            instance_id: InstanceId(instance_id),
            runtime: ContainerRuntime::LlamaCpp,
            launch: launch(),
            tunnel: tunnel.then(|| TunnelSpec {
                instance_id: InstanceId(instance_id),
                local_port: 8_800,
                remote_port: 8_000,
                ssh_host: "ssh5.vast.ai".to_owned(),
                ssh_port: 41_234,
            }),
        })
    }

    #[tokio::test]
    async fn a_plan_says_out_loud_that_stopping_is_not_destroying() {
        let dir = tempfile::tempdir().expect("tmp");
        let prov = provisioner(MockVast::new(), dir.path());
        let plan = prov.plan(&vast_spec(28_675_431, true)).await.expect("plan");
        assert_eq!(plan.port, 8_800);
        assert!(
            plan.warnings.iter().any(|w| w.contains("does NOT destroy")),
            "{:?}",
            plan.warnings
        );
        match plan.cost {
            CostEstimate::Approximate { usd, .. } => {
                assert!((usd.as_usd() - 0.3012).abs() < 1e-4, "{usd}");
            }
            other => panic!("expected a per-hour estimate, got {other:?}"),
        }
        // The confirm dialog never renders a credential.
        let rendered = format!("{:?}", plan.argv);
        assert!(!rendered.contains("hf_secret_value"), "{rendered}");
        assert!(rendered.contains("<from credentials.toml>"), "{rendered}");
        assert_eq!(prov.kind(), BackendKind::VastLlama);
    }

    #[tokio::test(start_paused = true)]
    async fn bringing_a_rented_box_up_requires_an_approval_that_covers_its_rate() {
        let dir = tempfile::tempdir().expect("tmp");
        let prov = provisioner(MockVast::new(), dir.path());
        let plan = prov.plan(&vast_spec(28_675_431, true)).await.expect("plan");

        let err = prov
            .up(plan.clone(), None)
            .await
            .expect_err("no approval at all");
        assert!(err.to_string().contains("SpendApproval"), "{err}");

        // $0.10/hr does not cover a $0.3012/hr box.
        let err = prov
            .up(plan.clone(), Some(approval(0.10)))
            .await
            .expect_err("under-approved");
        assert!(err.to_string().contains("approval covers"), "{err}");

        let backend = prov.up(plan, Some(approval(0.40))).await.expect("up");
        assert_eq!(backend.base_url, "http://127.0.0.1:8800");
        assert_eq!(backend.provenance, Provenance::Rented);
        assert_eq!(backend.id.as_str(), "vast-28675431");
        assert!(backend.tags.contains(&"rented".to_owned()));
        assert_eq!(backend.credential, CredentialSource::None);
        assert!(!backend.base_url.ends_with("/v1"));
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_tunnel_the_backend_points_at_the_mapped_public_port() {
        let dir = tempfile::tempdir().expect("tmp");
        let prov = provisioner(MockVast::new(), dir.path());
        let plan = prov
            .plan(&vast_spec(28_675_431, false))
            .await
            .expect("plan");
        let backend = prov.up(plan, Some(approval(0.40))).await.expect("up");
        assert_eq!(backend.base_url, "http://203.0.113.7:41235");
    }

    #[tokio::test]
    async fn taking_a_backend_down_never_destroys_the_instance() {
        let dir = tempfile::tempdir().expect("tmp");
        let api = Arc::new(MockVast::new());
        let tx = channel();
        let mut rx = tx.subscribe();
        let prov = VastProvisioner::new(api.clone(), ledger(dir.path()), VastCfg::default(), tx);

        let id = BackendId::parse("vast-28675431").expect("id");
        for mode in [DownMode::Drain, DownMode::Now, DownMode::Forget] {
            prov.down(&id, mode).await.expect("down");
        }
        assert!(
            !api.destroyed(),
            "a crash must not delete a paid box, and neither must a stop"
        );

        let raised = alerts(&mut rx);
        assert_eq!(raised.len(), 3, "{raised:?}");
        assert!(raised[0].1.contains("still billing"), "{:?}", raised[0]);
        assert_eq!(
            raised[0].2.as_deref(),
            Some("apexrouter vast destroy 28675431 --yes")
        );
    }

    #[tokio::test]
    async fn reconcile_rebuilds_the_fleet_and_logs_read_the_instance() {
        let dir = tempfile::tempdir().expect("tmp");
        let prov = provisioner(MockVast::new(), dir.path());
        let backends = prov.reconcile().await.expect("reconcile");
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].id.as_str(), "vast-28675431");
        assert_eq!(backends[0].kind, BackendKind::VastLlama);

        let lines = prov
            .logs(&BackendId::parse("vast-28675431").expect("id"), 3)
            .await
            .expect("logs");
        assert_eq!(lines.len(), 3);

        // A backend id that names no instance is a clean error, not a panic.
        let err = prov
            .logs(&BackendId::parse("local-carnice").expect("id"), 3)
            .await
            .expect_err("not a vast id");
        assert!(
            err.to_string().contains("does not name a vast instance"),
            "{err}"
        );
    }

    #[test]
    fn the_backend_id_matches_the_one_the_endpoint_record_will_use() {
        let id = backend_id(InstanceId(28_675_431)).expect("id");
        assert_eq!(id.as_str(), "vast-28675431");
        assert_eq!(id.as_str(), vast_spec(28_675_431, false).suggested_id());
        assert_eq!(
            instance_of(&id).expect("round trip"),
            InstanceId(28_675_431)
        );
    }

    #[test]
    fn accrued_cost_needs_both_a_rate_and_an_uptime() {
        assert_eq!(accrued(None), CostEstimate::Unknown);
        let mut inst = instance(1, Some("running"));
        inst.start_date = None;
        assert_eq!(accrued(Some(&inst)), CostEstimate::Unknown);
        inst.start_date = Some(chrono::Utc::now().timestamp() as f64 - 3_600.0);
        inst.dph_total = None;
        assert_eq!(accrued(Some(&inst)), CostEstimate::Unknown);
    }

    /// The hermeticity guard for this unit.
    ///
    /// Every test above answers from [`MockVast`], which owns no socket. This asserts the
    /// two things that could quietly change that: a real client, or an absolute URL that
    /// something might one day dial.
    #[test]
    fn nothing_in_this_unit_can_reach_a_paid_endpoint() {
        let src = include_str!("rent.rs");
        let tests = src.split("mod tests").nth(1).expect("a test module");
        for forbidden in ["VastApiHttp", "https://", "reqwest"] {
            assert!(
                !tests.contains(forbidden),
                "the tests must not mention {forbidden}"
            );
        }
        // The only hosts named anywhere are loopback and TEST-NET-3 (RFC 5737), and they are
        // string assertions, never dialled.
        assert!(tests.contains("127.0.0.1"));
        assert!(tests.contains("203.0.113.7"));
    }

    // ---- park and wake -----------------------------------------------------------------

    /// A fleet with one running, priced, disk-priced box, for the park/wake path.
    fn parked_fixture(id: u64) -> FixtureVast {
        FixtureVast::new()
            .with_instances_json(&format!(
                r#"{{"instances": [{{"id": {id}, "actual_status": "running",
                     "dph_total": 0.8361, "disk_space": 150.0, "storage_cost": 0.15,
                     "gpu_name": "RTX 4090", "num_gpus": 2}}]}}"#
            ))
            .expect("fixture fleet")
    }

    #[tokio::test]
    async fn parking_verifies_the_stop_and_stays_on_the_ledger() {
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = parked_fixture(46_509_449);
        let tx = channel();
        // Seed an active rental so "parked but active" is observable.
        rent(&api, &ledger, &request(), approval(0.90), &tx)
            .await
            .ok();

        let parked = park(
            &api,
            &ledger,
            InstanceId(46_509_449),
            &tx,
            30,
            1, // poll floor clamps this; the fixture answers instantly
        )
        .await
        .expect("park");
        assert_eq!(parked.phase(), BootPhase::Parked);
        assert!(api
            .calls()
            .contains(&FixtureCall::SetTargetState(InstanceId(46_509_449), false)));

        let rows = ledger.rows().expect("rows");
        let last = rows.last().expect("a parked row");
        assert_eq!(last.state, LedgerState::Parked);
        assert!(
            last.note.as_deref().unwrap_or_default().contains("/week"),
            "the weekly figure is on the record: {:?}",
            last.note
        );
        // Parking is not forgetting: the box still counts as active.
        assert!(ledger
            .active()
            .expect("active")
            .iter()
            .any(|r| r.instance_id == Some(InstanceId(46_509_449))));
    }

    #[tokio::test]
    async fn parking_an_already_parked_box_is_idempotent() {
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = FixtureVast::new()
            .with_instances_json(r#"{"instances": [{"id": 7, "actual_status": "stopped"}]}"#)
            .expect("fleet");
        let tx = channel();

        park(&api, &ledger, InstanceId(7), &tx, 30, 1)
            .await
            .expect("idempotent");
        assert!(
            !api.calls()
                .iter()
                .any(|c| matches!(c, FixtureCall::SetTargetState(..))),
            "already parked: no state change is sent"
        );
        assert!(ledger.rows().expect("rows").is_empty(), "and no new row");
    }

    #[tokio::test]
    async fn waking_requires_the_approval_to_cover_the_live_rate() {
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = parked_fixture(7);
        let tx = channel();

        let err = wake(
            &api,
            &ledger,
            InstanceId(7),
            approval(0.40), // the box bills $0.8361/hr
            &tx,
            30,
            1,
        )
        .await
        .expect_err("above the approval");
        assert!(err.to_string().contains("approval covers"), "{err}");
        assert!(
            !api.calls()
                .iter()
                .any(|c| matches!(c, FixtureCall::SetTargetState(..))),
            "nothing was started"
        );
    }

    #[tokio::test]
    async fn a_wake_reaches_running_and_lands_on_the_ledger() {
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = parked_fixture(7);
        let tx = channel();

        let woken = wake(&api, &ledger, InstanceId(7), approval(0.90), &tx, 30, 1)
            .await
            .expect("wake");
        assert_eq!(woken.phase(), BootPhase::Healthy);
        assert!(api
            .calls()
            .contains(&FixtureCall::SetTargetState(InstanceId(7), true)));
        let rows = ledger.rows().expect("rows");
        assert_eq!(rows.last().map(|r| r.state), Some(LedgerState::Running));
    }

    #[tokio::test(start_paused = true)]
    async fn a_wake_that_cannot_start_is_parked_again_not_left_armed() {
        // The GPUs went to another renter: the box must not be left with
        // `target_state: running`, or it starts billing unattended when they free up.
        let dir = tempfile::tempdir().expect("tmp");
        let ledger = ledger(dir.path());
        let api = MockVast::new().stuck(Some("scheduling"));
        let tx = channel();

        let err = wake(&api, &ledger, InstanceId(7), approval(0.90), &tx, 20, 5_000)
            .await
            .expect_err("never reached running");
        assert!(err.to_string().contains("parked again"), "{err}");
        assert_eq!(
            api.set_state_calls(),
            vec![true, false],
            "started, then re-parked"
        );
        assert!(
            ledger.rows().expect("rows").is_empty(),
            "no Running row for a wake that never ran"
        );
    }
}
