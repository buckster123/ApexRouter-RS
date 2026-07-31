//! OWNER: unit P-04 (providers/src/vast/{rent,boot,stall}.rs). Do not edit outside that
//! unit.
//!
//! The boot watchdog. Polls no faster than `[providers.vast] poll_min_ms` (vast publishes no
//! rate limits), treats `exited | offline | unknown` as **terminal** — they never recover —
//! and auto-destroys a wedged instance at `max_boot_secs`.
//!
//! ## Why a watchdog at all
//!
//! A container that never reaches `running` still bills. `$3.34/hr` wedged overnight is the
//! whole account. So the deadline is not advisory: when it expires the instance is destroyed,
//! and if the destroy call itself fails that is a **`Critical`** alert carrying the exact
//! command a human must run, because at that point only a human can stop the meter.
//!
//! ## What "terminal" means
//!
//! `VastInstance::is_terminal` is `exited | offline | unknown`. Those never recover, so the
//! loop returns immediately rather than paying for another `max_boot_secs` of polling. Every
//! *other* unrecognised status is `Provisioning`, never `Failed`: a status vast added last
//! Tuesday is not evidence that a box we are paying for is dead.
//!
//! ## Pacing
//!
//! The gap between two instance polls is never shorter than `poll_min_ms`, measured from the
//! start of one poll to the start of the next, so a slow API response stretches the interval
//! rather than compressing it. The deadline is evaluated *after* the pacing sleep, so the
//! watchdog can fire up to one poll interval late — late is a bounded cost, and polling
//! faster than the floor to be punctual is how you get rate-limited by an API that publishes
//! no limits.

use super::api::VastApi;
use super::rent::backend_id;
use apexrouter_core::error::Result;
use apexrouter_protocol::{AlertLevel, BootPhase, Event, InstanceId, LogSource};
use std::time::Duration;
use tokio::sync::broadcast;
// tokio's clock, not std's: `start_paused` tests advance this one, and in production the two
// are the same monotonic reading.
use tokio::time::Instant;

/// The polling floor used by [`watch_boot`], mirroring `VastCfg::default().poll_min_ms`.
///
/// [`watch_boot`]'s published signature carries no config, so it uses the shipped default; a
/// daemon holding a [`apexrouter_core::config::VastCfg`] should call [`watch_boot_every`]
/// with the operator's value instead. A unit test asserts the two numbers have not drifted.
pub const DEFAULT_POLL_MIN_MS: u64 = 5_000;

/// However low `poll_min_ms` is configured, never poll faster than this. Vast publishes no
/// rate-limit headers, so the only safe policy is a floor we choose ourselves.
pub const POLL_FLOOR_MS: u64 = 250;

/// Drive one instance's boot state machine to a terminal phase, broadcasting each transition.
///
/// Returns the phase the machine ended in: [`BootPhase::Healthy`] when it came up,
/// [`BootPhase::Destroyed`] when the instance vanished from the fleet, or
/// [`BootPhase::Failed`] when vast reported a terminal status or the watchdog expired. A
/// wedged instance is **destroyed** at `max_secs`, because a box that never boots still bills.
///
/// Polling errors are not fatal: an unreachable API is a reason to retry until the deadline,
/// never a reason to conclude anything about a machine that is currently charging money.
pub async fn watch_boot(
    api: &dyn VastApi,
    id: InstanceId,
    max_secs: u64,
    tx: &broadcast::Sender<Event>,
) -> Result<BootPhase> {
    watch_boot_every(api, id, max_secs, DEFAULT_POLL_MIN_MS, tx).await
}

/// [`watch_boot`] with the operator's configured `[providers.vast] poll_min_ms`.
///
/// This is the form the daemon uses; [`watch_boot`] is the published signature and delegates
/// here with [`DEFAULT_POLL_MIN_MS`].
pub async fn watch_boot_every(
    api: &dyn VastApi,
    id: InstanceId,
    max_secs: u64,
    poll_min_ms: u64,
    tx: &broadcast::Sender<Event>,
) -> Result<BootPhase> {
    let poll = Duration::from_millis(poll_min_ms.max(POLL_FLOOR_MS));
    let deadline = Instant::now() + Duration::from_secs(max_secs);
    let mut last: Option<BootPhase> = None;
    let mut consecutive_errors: u32 = 0;

    loop {
        let polled_at = Instant::now();

        match api.instance(id).await {
            Ok(Some(inst)) => {
                consecutive_errors = 0;
                let phase = inst.phase();
                if last.as_ref() != Some(&phase) {
                    emit_phase(tx, id, &phase, inst.status_msg.clone());
                    last = Some(phase.clone());
                }

                // `exited | offline | unknown` never recover. Stop paying to watch.
                if inst.is_terminal() {
                    let _ = tx.send(Event::Alert {
                        level: AlertLevel::Serious,
                        message: format!(
                            "vast instance {} reported `{}` and will not recover",
                            id.0,
                            inst.actual_status.as_deref().unwrap_or("terminal")
                        ),
                        action: Some(format!("apexrouter vast destroy {} --yes", id.0)),
                        id: format!("vast.terminal.{}", id.0),
                    });
                    return Ok(phase);
                }
                if matches!(phase, BootPhase::Healthy | BootPhase::Destroyed) {
                    return Ok(phase);
                }
            }
            // Gone from the fleet: somebody else destroyed it, or it never existed.
            Ok(None) => {
                emit_phase(tx, id, &BootPhase::Destroyed, None);
                return Ok(BootPhase::Destroyed);
            }
            Err(e) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                tracing::warn!(
                    instance = id.0,
                    attempts = consecutive_errors,
                    error = %e,
                    "vast poll failed; the instance is still billing, so we keep watching"
                );
                let _ = tx.send(Event::LogLine {
                    source: LogSource::Instance { id },
                    line: format!("vast API unreachable ({e}); retrying"),
                });
            }
        }

        // Pace from the start of the poll, so a slow response stretches the gap instead of
        // compressing the next one.
        let elapsed = polled_at.elapsed();
        if elapsed < poll {
            tokio::time::sleep(poll - elapsed).await;
        }

        if Instant::now() >= deadline {
            return expire(api, id, max_secs, tx).await;
        }
    }
}

/// The deadline expired. Destroy the instance and say so loudly.
///
/// A destroy that fails is the one case where software cannot stop the meter, so it is a
/// `Critical` alert carrying the literal command a human has to run.
async fn expire(
    api: &dyn VastApi,
    id: InstanceId,
    max_secs: u64,
    tx: &broadcast::Sender<Event>,
) -> Result<BootPhase> {
    tracing::warn!(
        instance = id.0,
        max_secs,
        "boot watchdog expired; destroying the instance so it stops billing"
    );

    let reason = match api.destroy(id).await {
        Ok(()) => {
            let _ = tx.send(Event::Alert {
                level: AlertLevel::Serious,
                message: format!(
                    "vast instance {} never became healthy within {max_secs}s and was destroyed",
                    id.0
                ),
                action: None,
                id: format!("vast.watchdog.{}", id.0),
            });
            format!("boot watchdog: not healthy after {max_secs}s; the instance was destroyed")
        }
        Err(e) => {
            let _ = tx.send(Event::Alert {
                level: AlertLevel::Critical,
                message: format!(
                    "vast instance {} is wedged and the destroy call FAILED ({e}) — \
                     it is still billing",
                    id.0
                ),
                action: Some(format!("apexrouter vast destroy {} --yes", id.0)),
                id: format!("vast.watchdog.{}", id.0),
            });
            format!(
                "boot watchdog: not healthy after {max_secs}s and the destroy call failed \
                 ({e}); the instance may still be billing"
            )
        }
    };

    let phase = BootPhase::Failed { reason };
    emit_phase(tx, id, &phase, None);
    Ok(phase)
}

/// Broadcast one boot transition. Best effort: nobody listening is not an error.
fn emit_phase(
    tx: &broadcast::Sender<Event>,
    id: InstanceId,
    phase: &BootPhase,
    line: Option<String>,
) {
    if let Ok(backend) = backend_id(id) {
        let _ = tx.send(Event::BootProgress {
            backend,
            phase: phase.clone(),
            line,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vast::rent::tests::MockVast;
    use apexrouter_core::config::VastCfg;

    fn channel() -> (broadcast::Sender<Event>, broadcast::Receiver<Event>) {
        broadcast::channel(64)
    }

    fn phases(rx: &mut broadcast::Receiver<Event>) -> Vec<BootPhase> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Event::BootProgress { phase, .. } = ev {
                out.push(phase);
            }
        }
        out
    }

    #[test]
    fn the_default_poll_floor_matches_the_shipped_config() {
        assert_eq!(DEFAULT_POLL_MIN_MS, VastCfg::default().poll_min_ms);
    }

    #[tokio::test(start_paused = true)]
    async fn a_healthy_boot_reports_every_transition_once() {
        let api = MockVast::new().with_statuses(&[
            Some("created"),
            Some("created"),
            Some("loading"),
            Some("running"),
        ]);
        let (tx, mut rx) = channel();

        let phase = watch_boot_every(&api, InstanceId(7), 600, 5_000, &tx)
            .await
            .expect("watch");
        assert_eq!(phase, BootPhase::Healthy);
        assert_eq!(
            phases(&mut rx),
            vec![
                BootPhase::Provisioning,
                BootPhase::Pulling,
                BootPhase::Healthy
            ],
            "a repeated status must not re-broadcast"
        );
        assert!(!api.destroyed(), "a healthy boot must never be destroyed");
    }

    #[tokio::test(start_paused = true)]
    async fn it_never_polls_faster_than_the_configured_floor() {
        let api = MockVast::new().with_statuses(&[
            Some("created"),
            Some("created"),
            Some("created"),
            Some("running"),
        ]);
        let (tx, _rx) = channel();

        watch_boot_every(&api, InstanceId(7), 3_600, 5_000, &tx)
            .await
            .expect("watch");

        let gaps = api.poll_gaps();
        assert_eq!(gaps.len(), 3, "four polls means three gaps");
        for gap in gaps {
            assert!(
                gap >= Duration::from_millis(5_000),
                "polled after only {gap:?}, faster than poll_min_ms"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_terminal_status_stops_the_loop_at_once() {
        for terminal in ["exited", "offline", "unknown"] {
            let api = MockVast::new().with_statuses(&[Some("created"), Some(terminal)]);
            let (tx, mut rx) = channel();

            let phase = watch_boot_every(&api, InstanceId(7), 86_400, 1_000, &tx)
                .await
                .expect("watch");
            assert!(
                matches!(phase, BootPhase::Failed { .. }),
                "{terminal} -> {phase:?}"
            );
            assert_eq!(api.instance_calls(), 2, "{terminal} kept polling");
            assert!(
                !api.destroyed(),
                "{terminal} is already dead; destroying it is not this loop's call"
            );

            let alerts = std::iter::from_fn(|| rx.try_recv().ok())
                .filter(|e| matches!(e, Event::Alert { .. }))
                .count();
            assert_eq!(alerts, 1, "{terminal} must raise exactly one alert");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_instance_that_vanished_reads_as_destroyed() {
        let api = MockVast::new().with_statuses(&[None]);
        let (tx, mut rx) = channel();

        let phase = watch_boot_every(&api, InstanceId(7), 600, 1_000, &tx)
            .await
            .expect("watch");
        assert_eq!(phase, BootPhase::Destroyed);
        assert_eq!(phases(&mut rx), vec![BootPhase::Destroyed]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_wedged_instance_is_destroyed_at_the_deadline() {
        // Never leaves `created`.
        let api = MockVast::new().stuck(Some("created"));
        let (tx, mut rx) = channel();

        let phase = watch_boot_every(&api, InstanceId(7), 30, 5_000, &tx)
            .await
            .expect("watch");

        match &phase {
            BootPhase::Failed { reason } => {
                assert!(reason.contains("watchdog"), "{reason}");
                assert!(reason.contains("destroyed"), "{reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(api.destroyed(), "the watchdog must stop the meter");

        let levels: Vec<AlertLevel> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                Event::Alert { level, .. } => Some(level),
                _ => None,
            })
            .collect();
        assert_eq!(levels, vec![AlertLevel::Serious]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_destroy_at_the_deadline_is_a_critical_alert_with_the_command() {
        let api = MockVast::new().stuck(Some("created")).destroy_fails();
        let (tx, mut rx) = channel();

        let phase = watch_boot_every(&api, InstanceId(41), 20, 5_000, &tx)
            .await
            .expect("watch");
        match &phase {
            BootPhase::Failed { reason } => {
                assert!(reason.contains("still be billing"), "{reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        let alert = std::iter::from_fn(|| rx.try_recv().ok())
            .find_map(|e| match e {
                Event::Alert {
                    level,
                    action,
                    message,
                    ..
                } => Some((level, action, message)),
                _ => None,
            })
            .expect("an alert");
        assert_eq!(alert.0, AlertLevel::Critical);
        assert_eq!(
            alert.1.as_deref(),
            Some("apexrouter vast destroy 41 --yes"),
            "a human must be told the exact command"
        );
        assert!(alert.2.contains("still billing"), "{}", alert.2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_polling_error_is_retried_rather_than_treated_as_a_verdict() {
        let api = MockVast::new().errors_then(2, &[Some("running")]);
        let (tx, _rx) = channel();

        let phase = watch_boot_every(&api, InstanceId(7), 600, 1_000, &tx)
            .await
            .expect("transient errors are not fatal");
        assert_eq!(phase, BootPhase::Healthy);
        assert_eq!(api.instance_calls(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unrecognised_status_is_never_read_as_failure() {
        let api = MockVast::new().stuck(Some("a-status-vast-added-tuesday"));
        let (tx, mut rx) = channel();
        // 0 s of budget: one poll still happens, then the watchdog fires.
        let phase = watch_boot_every(&api, InstanceId(7), 0, 1_000, &tx)
            .await
            .expect("watch");
        assert!(matches!(phase, BootPhase::Failed { .. }));
        assert_eq!(api.instance_calls(), 1, "at least one poll always happens");
        assert_eq!(
            phases(&mut rx).first(),
            Some(&BootPhase::Provisioning),
            "an unknown status is Provisioning, never Failed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn watch_boot_uses_the_shipped_poll_default() {
        let api = MockVast::new().with_statuses(&[Some("created"), Some("running")]);
        let (tx, _rx) = channel();
        watch_boot(&api, InstanceId(7), 600, &tx).await.expect("ok");
        let gaps = api.poll_gaps();
        assert_eq!(gaps.len(), 1);
        assert!(
            gaps[0] >= Duration::from_millis(DEFAULT_POLL_MIN_MS),
            "{gaps:?}"
        );
    }
}
