//! OWNER: unit C-08 (core/ledger.rs, core/money.rs). Do not edit outside that unit.
//!
//! The append-only money ledger. **"Active" is a query, not a file.**
//!
//! Reserve **before** billing: [`Ledger::reserve`] appends a `Reserved` row and returns a
//! [`PendingLaunch`] guard; the create call happens; `commit()` appends `Confirmed`.
//! `impl Drop for PendingLaunch` appends `OrphanSuspect` **synchronously**, which is why
//! [`Ledger::append`] has a blocking path — `Drop` cannot `await`. A `SIGKILL` skips `Drop`
//! entirely, which is why the `Reserved` row, written *before* the call, is the real
//! protection.
//!
//! ## The file
//!
//! `$STATE/ledger.jsonl`: one JSON [`LedgerRow`] per line, `O_APPEND`, one `write()` per row,
//! so a concurrent reader never sees a torn line and two writers never interleave one. The
//! sequence number is assigned under an exclusive `flock` held across "read the tail, then
//! write the row", which is what keeps `seq` unique even when the daemon and an offline
//! writer append at the same moment. Nothing is ever rewritten and nothing is ever deleted:
//! a row is a fact about the past.
//!
//! ## Following a reservation through the log
//!
//! A `Reserved` row has no instance id — the id does not exist until the create call returns.
//! So the rows that resolve a reservation ([`PendingLaunch::commit`] and the `Drop` handler)
//! start their `note` with `reserve_seq=<n>`, naming the row they supersede. That
//! back-reference is what lets [`Ledger::active`] tell "reserved and still unresolved" from
//! "reserved, then confirmed as instance 42" without ever mutating the earlier row.

use crate::error::{Error, Result};
use crate::money::{ApprovalSource, SpendApproval};
use crate::paths::Paths;
use apexrouter_protocol::{
    CostEstimate, InstanceId, LedgerRow, LedgerState, PriceSource, ProfileId, RentRequest,
};
use std::collections::{BTreeMap, HashSet};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// The prefix that ties a follow-up row back to the `Reserved` row it resolves.
const RESERVE_REF: &str = "reserve_seq=";

/// `$STATE/ledger.jsonl`, opened `O_APPEND`.
#[derive(Clone, Debug)]
pub struct Ledger {
    /* C-08 */
    path: PathBuf,
}

impl Ledger {
    /// Open (creating if needed) the ledger for this state directory.
    pub fn open(paths: &Paths) -> Result<Ledger> {
        Ledger::at(paths.ledger())
    }

    /// The real constructor, path-only so it is exercisable without a whole [`Paths`].
    fn at(path: PathBuf) -> Result<Ledger> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                path: dir.display().to_string(),
                source,
            })?;
        }
        // Create it now, so a fresh install finds out the state dir is unwritable here rather
        // than at the instant money is about to move.
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| Error::Io {
                path: path.display().to_string(),
                source,
            })?;
        Ok(Ledger { path })
    }

    /// **SYNCHRONOUS** — `Drop` cannot `await`. `O_APPEND`, one `write()` per row, so a
    /// concurrent reader never sees a torn line. Returns the assigned sequence number.
    ///
    /// The caller's `row.seq` is ignored and overwritten: the sequence belongs to the file,
    /// not to whoever built the row, and is assigned under the same exclusive lock as the
    /// write.
    pub fn append(&self, row: &LedgerRow) -> Result<u64> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| self.io(source))?;

        // Held until `file` is dropped at the end of this function, on every path.
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
            .map_err(|e| self.io(std::io::Error::from(e)))?;

        let seq = self
            .rows()?
            .iter()
            .map(|r| r.seq)
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        let mut row = row.clone();
        row.seq = seq;
        let mut line = serde_json::to_string(&row)?;
        line.push('\n');

        // One buffer, one `write_all`: a short line never straddles two syscalls.
        file.write_all(line.as_bytes())
            .map_err(|source| self.io(source))?;
        file.flush().map_err(|source| self.io(source))?;

        Ok(seq)
    }

    /// Every row, in file order. A row that will not parse is skipped and reported, never
    /// fatal.
    pub fn rows(&self) -> Result<Vec<LedgerRow>> {
        let file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(self.io(source)),
        };

        let mut out = Vec::new();
        for (n, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|source| self.io(source))?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<LedgerRow>(&line) {
                Ok(row) => out.push(row),
                Err(e) => tracing::warn!(
                    path = %self.path.display(),
                    line = n + 1,
                    error = %e,
                    "skipping unparseable ledger row"
                ),
            }
        }
        Ok(out)
    }

    /// Rows describing something still billing: a QUERY over the log, not a single-slot file.
    ///
    /// The rule, stated so an operator can check it against the file by eye:
    ///
    /// * For every instance id, only its **latest** row counts. It is active unless that row
    ///   is `Destroyed` or carries a `destroyed_at_unix` — architecture §4.8's "every ledger
    ///   row without `destroyed_at` is queried". A merely *requested* destroy is not proof of
    ///   anything, so it stays here until destruction is verified.
    /// * A `Reserved` or `OrphanSuspect` row that never got an instance id counts too, unless
    ///   a later row named it with `reserve_seq=`. We do not know whether the create call
    ///   went through, and an unknown box that might be billing is exactly what must stay
    ///   visible.
    ///
    /// The result is in file order.
    pub fn active(&self) -> Result<Vec<LedgerRow>> {
        let rows = self.rows()?;

        let mut resolved: HashSet<u64> = HashSet::new();
        let mut latest: BTreeMap<u64, LedgerRow> = BTreeMap::new();
        let mut unlinked: Vec<LedgerRow> = Vec::new();

        for row in rows {
            if let Some(seq) = reserve_ref(row.note.as_deref()) {
                resolved.insert(seq);
            }
            match row.instance_id {
                Some(id) => {
                    latest.insert(id.0, row);
                }
                None => unlinked.push(row),
            }
        }

        let mut out: Vec<LedgerRow> = latest
            .into_values()
            .filter(|r| r.state != LedgerState::Destroyed && r.destroyed_at_unix.is_none())
            .chain(unlinked.into_iter().filter(|r| {
                matches!(r.state, LedgerState::Reserved | LedgerState::OrphanSuspect)
                    && r.destroyed_at_unix.is_none()
                    && !resolved.contains(&r.seq)
            }))
            .collect();
        out.sort_by_key(|r| r.seq);
        Ok(out)
    }

    /// Append `Reserved` and hand back the guard. Nothing has been billed yet.
    pub fn reserve(&self, req: &RentRequest, approval: &SpendApproval) -> Result<PendingLaunch> {
        let approved_max_dph = approval.max_usd_per_hour().as_usd();
        let approval_source = source_str(approval.source()).to_owned();

        let row = LedgerRow {
            seq: 0, // assigned by `append`
            at_unix: now_unix(),
            instance_id: None,
            state: LedgerState::Reserved,
            offer_id: req.offer_id,
            profile: req.profile.clone(),
            gpu: None,
            num_gpus: None,
            // The offer's real rate is not known until the create call returns; the approved
            // ceiling is the only honest number we hold at this instant.
            dph: None,
            approved_max_dph: Some(approved_max_dph),
            approval_source: Some(approval_source.clone()),
            destroyed_at_unix: None,
            est_cost: CostEstimate::Approximate {
                usd: approval.max_usd_per_hour(),
                source: PriceSource::Derived,
                assumption: "one hour at the approved ceiling; the offer's real dph is unknown \
                             until the create call returns"
                    .to_owned(),
            },
            note: Some(describe(req)),
        };

        let seq = self.append(&row)?;

        Ok(PendingLaunch {
            ledger: self.clone(),
            seq,
            committed: false,
            instance_id: None,
            offer_id: req.offer_id,
            profile: req.profile.clone(),
            approved_max_dph,
            approval_source,
        })
    }

    /// The file this ledger writes to.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// An I/O error that names the ledger file, because "permission denied" alone is not
    /// actionable at 3am.
    fn io(&self, source: std::io::Error) -> Error {
        Error::Io {
            path: self.path.display().to_string(),
            source,
        }
    }
}

/// A reservation in flight. Dropping it without [`PendingLaunch::commit`] records an
/// `OrphanSuspect` row, which raises a `Critical` alert with a Destroy action.
#[derive(Debug)]
pub struct PendingLaunch {
    /* C-08: ledger, seq, committed */
    ledger: Ledger,
    seq: u64,
    committed: bool,
    /// Set the instant `commit` learns the id, *before* the append, so an append that fails
    /// still leaves an `OrphanSuspect` row naming the box that is now billing.
    instance_id: Option<InstanceId>,
    offer_id: Option<u64>,
    profile: Option<ProfileId>,
    approved_max_dph: f64,
    approval_source: String,
}

impl PendingLaunch {
    /// The create call returned an instance id. Appends `Confirmed`.
    pub fn commit(mut self, id: InstanceId) -> Result<()> {
        self.instance_id = Some(id);
        let row = self.follow_up(
            LedgerState::Confirmed,
            format!("{RESERVE_REF}{} confirmed as instance {id}", self.seq),
        );
        // `committed` is set only after the row is durable: if the append fails, `Drop` still
        // records an orphan — one carrying the instance id, since we now know it.
        self.ledger.append(&row)?;
        self.committed = true;
        Ok(())
    }

    /// A row that resolves this reservation, carrying the `reserve_seq=` back-reference.
    fn follow_up(&self, state: LedgerState, note: String) -> LedgerRow {
        LedgerRow {
            seq: 0, // assigned by `append`
            at_unix: now_unix(),
            instance_id: self.instance_id,
            state,
            offer_id: self.offer_id,
            profile: self.profile.clone(),
            gpu: None,
            num_gpus: None,
            dph: None,
            approved_max_dph: Some(self.approved_max_dph),
            approval_source: Some(self.approval_source.clone()),
            destroyed_at_unix: None,
            est_cost: CostEstimate::Unknown,
            note: Some(note),
        }
    }
}

impl Drop for PendingLaunch {
    fn drop(&mut self) {
        // C-08: if !committed, append an OrphanSuspect row SYNCHRONOUSLY.
        if self.committed {
            return;
        }
        let note = match self.instance_id {
            Some(id) => format!(
                "{RESERVE_REF}{} dropped after the create call returned instance {id}; it may \
                 be billing",
                self.seq
            ),
            None => format!(
                "{RESERVE_REF}{} dropped without a commit; the create call may still have gone \
                 through",
                self.seq
            ),
        };
        let row = self.follow_up(LedgerState::OrphanSuspect, note);
        // Nothing above us can handle a failure here — we are unwinding, or returning through
        // a `?` — but a silent one would hide a paid box, so it is logged loudly.
        if let Err(e) = self.ledger.append(&row) {
            tracing::error!(
                path = %self.ledger.path().display(),
                seq = self.seq,
                error = %e,
                "could not record an orphaned vast reservation; check the fleet by hand"
            );
        }
    }
}

/// The `reserve_seq=<n>` back-reference a follow-up row carries, if this note has one.
fn reserve_ref(note: Option<&str>) -> Option<u64> {
    let rest = note?.strip_prefix(RESERVE_REF)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The string form recorded in `LedgerRow::approval_source`, snake case to match the wire
/// vocabulary the rest of the protocol uses.
fn source_str(source: ApprovalSource) -> &'static str {
    match source {
        ApprovalSource::Cli => "cli",
        ApprovalSource::WebUi => "web_ui",
        ApprovalSource::SlintUi => "slint_ui",
        ApprovalSource::Mcp { .. } => "mcp",
        ApprovalSource::Api => "api",
    }
}

/// A one-line human description of what was reserved, for the `note` field.
///
/// It deliberately never starts with [`RESERVE_REF`]: a `Reserved` row must not look like a
/// row that resolves some other reservation.
fn describe(req: &RentRequest) -> String {
    let what = match (req.offer_id, req.profile.as_ref()) {
        (Some(offer), Some(profile)) => format!("offer {offer} from profile {profile}"),
        (Some(offer), None) => format!("offer {offer}"),
        (None, Some(profile)) => format!("cheapest offer from profile {profile}"),
        (None, None) => "an unspecified offer".to_owned(),
    };
    format!("reserving {what}, image {}", req.launch.image)
}

/// Unix seconds.
fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VastCfg;
    use apexrouter_protocol::{ContainerLaunch, ContainerRuntime, ImageType, Money};
    use std::path::Path;

    fn ledger(dir: &Path) -> Ledger {
        Ledger::at(dir.join("ledger.jsonl")).expect("open ledger")
    }

    fn approval() -> SpendApproval {
        SpendApproval::confirm(
            Money::from_usd(0.40),
            ApprovalSource::Cli,
            &VastCfg::default(),
            None,
        )
        .expect("under the default ceiling")
    }

    fn rent_request() -> RentRequest {
        RentRequest {
            profile: Some(ProfileId::parse("rtx3090").expect("id")),
            offer_id: Some(12_345),
            launch: ContainerLaunch {
                runtime: ContainerRuntime::LlamaCpp,
                image: "buckster123/vastai-gguf:latest".into(),
                image_type: ImageType::Prebuilt,
                disk_gb: 60,
                env: BTreeMap::new(),
                onstart: "bash /app/launch.sh > /var/log/launch.log 2>&1 &".into(),
                host: "127.0.0.1".into(),
                port: 8000,
                expose_public: false,
            },
            confirm: true,
            max_usd_per_hour: 0.40,
            auto_tunnel: true,
            bind_alias: None,
        }
    }

    fn row(state: LedgerState, instance: Option<u64>) -> LedgerRow {
        LedgerRow {
            seq: 0,
            at_unix: 1_785_412_331,
            instance_id: instance.map(InstanceId),
            state,
            offer_id: Some(12_345),
            profile: None,
            gpu: Some("RTX 3090".into()),
            num_gpus: Some(2),
            dph: Some(0.3012),
            approved_max_dph: Some(0.40),
            approval_source: Some("cli".into()),
            destroyed_at_unix: None,
            est_cost: CostEstimate::Unknown,
            note: None,
        }
    }

    #[test]
    fn append_assigns_a_monotonic_sequence_and_rows_round_trip() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = ledger(dir.path());
        assert!(l.rows().expect("rows").is_empty());

        // The caller's seq is ignored: the file owns the sequence.
        let mut r = row(LedgerState::Reserved, None);
        r.seq = 999;
        assert_eq!(l.append(&r).expect("append"), 1);
        assert_eq!(
            l.append(&row(LedgerState::Confirmed, Some(42)))
                .expect("append"),
            2
        );
        assert_eq!(
            l.append(&row(LedgerState::Running, Some(42)))
                .expect("append"),
            3
        );

        let rows = l.rows().expect("rows");
        assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(rows[0].state, LedgerState::Reserved);
        assert_eq!(rows[2].instance_id, Some(InstanceId(42)));
        assert_eq!(rows[2].gpu.as_deref(), Some("RTX 3090"));

        // Append-only: one line per row, nothing rewritten.
        let raw = std::fs::read_to_string(l.path()).expect("read");
        assert_eq!(raw.lines().count(), 3);
        assert!(raw.ends_with('\n'));
    }

    #[test]
    fn a_reopened_ledger_continues_the_sequence() {
        let dir = tempfile::tempdir().expect("tmp");
        assert_eq!(
            ledger(dir.path())
                .append(&row(LedgerState::Reserved, None))
                .expect("append"),
            1
        );
        assert_eq!(
            ledger(dir.path())
                .append(&row(LedgerState::Reserved, None))
                .expect("append"),
            2
        );
    }

    #[test]
    fn an_unparseable_row_is_skipped_and_never_fatal() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = ledger(dir.path());
        l.append(&row(LedgerState::Confirmed, Some(7)))
            .expect("append");

        let mut f = OpenOptions::new()
            .append(true)
            .open(l.path())
            .expect("open");
        f.write_all(b"{ this is not json\n\n").expect("write");
        drop(f);

        l.append(&row(LedgerState::Running, Some(7)))
            .expect("append");
        let rows = l.rows().expect("rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn reserve_writes_the_row_before_anything_is_billed() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = ledger(dir.path());
        let pending = l.reserve(&rent_request(), &approval()).expect("reserve");

        // The row is on disk *now*, while the guard is still alive and no create call has
        // been made. This is the invariant a SIGKILL cannot skip.
        let rows = l.rows().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, LedgerState::Reserved);
        assert_eq!(rows[0].instance_id, None);
        assert_eq!(rows[0].offer_id, Some(12_345));
        assert_eq!(rows[0].approval_source.as_deref(), Some("cli"));
        assert_eq!(rows[0].approved_max_dph, Some(0.40));
        assert!(rows[0].est_cost.is_guess(), "a ceiling is not a bill");
        assert!(matches!(rows[0].est_cost, CostEstimate::Approximate { .. }));
        // A Reserved row must never look like a row that resolves another reservation.
        assert_eq!(reserve_ref(rows[0].note.as_deref()), None);

        pending.commit(InstanceId(9_001)).expect("commit");
    }

    #[test]
    fn a_dropped_reservation_appends_orphan_suspect() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = ledger(dir.path());
        let pending = l.reserve(&rent_request(), &approval()).expect("reserve");
        std::mem::drop(pending);

        let rows = l.rows().expect("rows");
        assert_eq!(rows.len(), 2, "{rows:#?}");
        assert_eq!(rows[1].state, LedgerState::OrphanSuspect);
        assert_eq!(reserve_ref(rows[1].note.as_deref()), Some(rows[0].seq));

        // And it is visible: exactly the orphan, not the reservation it supersedes.
        let active = l.active().expect("active");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].state, LedgerState::OrphanSuspect);
    }

    #[test]
    fn a_committed_reservation_appends_confirmed_and_no_orphan() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = ledger(dir.path());
        let pending = l.reserve(&rent_request(), &approval()).expect("reserve");
        pending.commit(InstanceId(9_001)).expect("commit");

        let rows = l.rows().expect("rows");
        assert_eq!(rows.len(), 2, "{rows:#?}");
        assert_eq!(rows[1].state, LedgerState::Confirmed);
        assert_eq!(rows[1].instance_id, Some(InstanceId(9_001)));
        assert_eq!(reserve_ref(rows[1].note.as_deref()), Some(rows[0].seq));
        assert!(!rows.iter().any(|r| r.state == LedgerState::OrphanSuspect));

        let active = l.active().expect("active");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].instance_id, Some(InstanceId(9_001)));
    }

    #[test]
    fn active_is_a_query_over_the_log() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = ledger(dir.path());
        assert!(l.active().expect("active").is_empty());

        // Instance 42 and 43 both rented and running.
        l.append(&row(LedgerState::Confirmed, Some(42))).expect("a");
        l.append(&row(LedgerState::Running, Some(42))).expect("b");
        l.append(&row(LedgerState::Confirmed, Some(43))).expect("c");
        l.append(&row(LedgerState::Running, Some(43))).expect("d");

        let active = l.active().expect("active");
        assert_eq!(
            active
                .iter()
                .filter_map(|r| r.instance_id)
                .collect::<Vec<_>>(),
            [InstanceId(42), InstanceId(43)]
        );

        let mut destroyed = row(LedgerState::Destroyed, Some(42));
        destroyed.destroyed_at_unix = Some(1_785_500_000);
        l.append(&destroyed).expect("e");

        let active = l.active().expect("active");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].instance_id, Some(InstanceId(43)));
        assert_eq!(active[0].state, LedgerState::Running);
    }

    #[test]
    fn a_destroy_that_was_only_requested_still_counts_as_billing() {
        // Architecture §4.8: destroy verifies before forgetting. A DestroyRequested row is
        // not proof the box is gone, so it stays visible until a Destroyed row lands.
        let dir = tempfile::tempdir().expect("tmp");
        let l = ledger(dir.path());
        l.append(&row(LedgerState::Confirmed, Some(42))).expect("a");
        l.append(&row(LedgerState::DestroyRequested, Some(42)))
            .expect("b");
        let active = l.active().expect("active");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].state, LedgerState::DestroyRequested);
    }

    #[test]
    fn concurrent_appends_never_tear_a_line_or_reuse_a_sequence() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = ledger(dir.path());
        std::thread::scope(|s| {
            for _ in 0..4 {
                let l = l.clone();
                s.spawn(move || {
                    for _ in 0..10 {
                        l.append(&row(LedgerState::Running, Some(42)))
                            .expect("append");
                    }
                });
            }
        });

        let rows = l.rows().expect("rows");
        assert_eq!(rows.len(), 40, "every row parsed, so no line was torn");
        let seqs: HashSet<u64> = rows.iter().map(|r| r.seq).collect();
        assert_eq!(seqs.len(), 40, "sequence numbers are unique");
    }

    #[test]
    fn reserve_ref_only_matches_the_documented_prefix() {
        assert_eq!(reserve_ref(None), None);
        assert_eq!(reserve_ref(Some("reserving offer 12345")), None);
        assert_eq!(reserve_ref(Some("reserve_seq=")), None);
        assert_eq!(reserve_ref(Some("reserve_seq=7 confirmed")), Some(7));
        assert_eq!(reserve_ref(Some("x reserve_seq=7")), None);
    }

    #[test]
    fn every_approval_source_has_a_stable_wire_string() {
        assert_eq!(source_str(ApprovalSource::Cli), "cli");
        assert_eq!(source_str(ApprovalSource::WebUi), "web_ui");
        assert_eq!(source_str(ApprovalSource::SlintUi), "slint_ui");
        assert_eq!(source_str(ApprovalSource::Api), "api");
        assert_eq!(
            source_str(ApprovalSource::Mcp {
                human_cleared: true
            }),
            "mcp"
        );
    }
}
