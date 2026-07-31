//! OWNER: unit P-01 (providers/src/local/**). Do not edit outside that unit.
//!
//! **The fit plan is the input to the launch, not a report about it.**
//!
//! MK1 ACCEPTANCE caught the solver — the one function that replaced 54 hand-solved recipe
//! strings, with its per-backend budget and its `why` list — computing a plan, storing it in
//! the record, printing it, and then *not* being used to build argv. `fit.parallel = 1` sat
//! in the record while llama.cpp logged `n_parallel is set to auto, using n_parallel = 4`,
//! because `-np` was never emitted at all. Same for `-c`, `-ngl`, `-dev` and the KV type.
//! ApexRouter was reporting a plan it did not execute, which made every number the GUI shows
//! about a running endpoint a guess.
//!
//! [`ResolvedSpec`] closes that **by construction**:
//!
//! * It is the only thing `LocalProvisioner::argv_for` accepts, and the only way to make one
//!   is [`ResolvedSpec::resolve`], which folds a [`FitPlan`] into the operator's draft.
//!   Handing the argv builder an unresolved draft does not compile.
//! * Every parameter the solver decided is emitted **explicitly**, so nothing is left to
//!   llama.cpp's own auto-sizing behind the plan's back.
//! * An explicit operator value still wins — and is recorded as a [`SpecOverride`], in
//!   [`ResolvedSpec::notes`] for the operator and in [`FitPlan::why`] for the record.
//! * [`ResolvedSpec::disagreements`] is the invariant itself, executable: it reports every
//!   way a rendered argv fails to carry the plan it claims to execute. A launch logs it; the
//!   acceptance test asserts it is empty for both the printed preview and the argv the child
//!   was actually exec'd with.
//!
//! # Why the operator's number and the solver's number usually agree
//!
//! `fit()` is *given* the draft's `ctx`, `parallel`, `kv_type` and `-dev` list as its
//! `want_*` inputs, so for those four the plan already carries the operator's answer and an
//! override is provenance rather than conflict. `-ngl` is the exception: the solver derives
//! it from the verdict and is never told what the operator asked for, so `NglPlan::All`
//! against a `NeedsOffload` plan is a genuine contradiction — and the one that OOMs. That
//! case gets a warning, not just a note.

use apexrouter_protocol::{
    ArgvPreview, EndpointRecord, EndpointSpec, FitPlan, LocalLlamaSpec, NglPlan, SplitMode,
    SplitPlan,
};

/// The prefix every line this module appends to [`FitPlan::why`] carries, so the launch
/// provenance is distinguishable from the solver's own arithmetic — and so re-resolving an
/// already-annotated plan is idempotent.
const WHY_PREFIX: &str = "launch:";

/// The alternative spellings llama.cpp accepts for each flag we plan.
///
/// `extra_args` goes through verbatim and llama.cpp's parser lets a later occurrence win, so
/// checking only the short form would miss `--extra-args --ctx-size 999` quietly replacing
/// the planned `-c`.
const ALIASES: &[(&str, &[&str])] = &[
    ("-c", &["-c", "--ctx-size"]),
    ("-np", &["-np", "--parallel"]),
    ("-ngl", &["-ngl", "--n-gpu-layers", "--gpu-layers"]),
    ("-ctk", &["-ctk", "--cache-type-k"]),
    ("-ctv", &["-ctv", "--cache-type-v"]),
    ("-dev", &["-dev", "--device"]),
    ("-sm", &["-sm", "--split-mode"]),
    ("--port", &["--port"]),
];

/// One launch parameter the operator pinned by hand, next to what the solver planned.
///
/// Recorded rather than merged away: "the operator asked for this" and "the solver chose
/// this" are different facts, and a plan that cannot tell them apart cannot explain why a
/// launch OOMed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecOverride {
    /// The llama.cpp flag this parameter becomes.
    pub flag: &'static str,
    /// The value that will reach argv — the operator's.
    pub value: String,
    /// The value the solver put in the [`FitPlan`].
    pub planned: String,
}

impl SpecOverride {
    /// True when the operator's value contradicts the plan instead of confirming it.
    ///
    /// For `-c`, `-np`, `-ctk`/`-ctv` and `-dev` this is normally false: the solver was
    /// *given* the operator's value and honoured it. `-ngl` is the field where it bites.
    pub fn contradicts_the_plan(&self) -> bool {
        self.value != self.planned
    }

    /// The sentence that goes in the record and, when it contradicts, in the warnings.
    pub fn line(&self) -> String {
        if self.contradicts_the_plan() {
            format!(
                "{WHY_PREFIX} {} {} pinned by the operator, overriding the planned {} — \
                 the plan below describes the solver's answer, not this one",
                self.flag, self.value, self.planned
            )
        } else {
            format!(
                "{WHY_PREFIX} {} {} pinned by the operator (the solver planned the same)",
                self.flag, self.value
            )
        }
    }
}

/// An [`EndpointSpec`] with the solver's [`FitPlan`] folded in — the **only** thing the argv
/// builder accepts.
///
/// Construction is deliberately narrow. [`ResolvedSpec::resolve`] is a pure function of
/// `(draft, port, fit)`, which is what lets `plan()` and `launch()` render argv independently
/// and still be unable to disagree: they resolve the same draft against the same plan, and
/// only the port can differ between them.
#[derive(Clone, Debug)]
pub struct ResolvedSpec {
    spec: EndpointSpec,
    fit: Option<FitPlan>,
    overrides: Vec<SpecOverride>,
}

impl ResolvedSpec {
    /// Fold `fit` into `draft`, bind `port`, and record what the operator pinned.
    ///
    /// The merge, field by field:
    ///
    /// | argv | when the draft names it | when it does not |
    /// |---|---|---|
    /// | `-c` | the operator's `ctx`, recorded as an override | [`FitPlan::ctx`] |
    /// | `-np` | the operator's `parallel` | [`FitPlan::parallel`] |
    /// | `-ctk`/`-ctv` | the operator's `kv_type` | [`FitPlan::kv_type`] |
    /// | `-ngl` | anything but [`NglPlan::Auto`] | [`FitPlan::ngl`] |
    /// | `-dev` | a non-empty device list | [`FitPlan::split`]`.devices` |
    ///
    /// `-sm`, `-mg` and `--tensor-split` stay the operator's: the solver copies them through
    /// rather than deciding them, so there is nothing to fold.
    ///
    /// A draft with **no** plan — a model whose GGUF header could not be read, a vLLM spec,
    /// a backend this supervisor does not own — resolves to itself plus the port. There is
    /// nothing to execute faithfully, and inventing numbers would be worse than the defect
    /// this type exists to close.
    ///
    /// Annotating [`FitPlan::why`] is idempotent: `launch()` re-resolves the plan `plan()`
    /// already annotated, and the lines must not double.
    pub fn resolve(draft: &EndpointSpec, port: u16, fit: Option<FitPlan>) -> ResolvedSpec {
        let (s, mut plan) = match (draft, fit) {
            (EndpointSpec::LocalLlama(s), Some(plan)) => (s, plan),
            (other, fit) => {
                return ResolvedSpec {
                    spec: with_port(other, port),
                    fit,
                    overrides: Vec::new(),
                }
            }
        };

        let mut out: LocalLlamaSpec = s.clone();
        out.port = Some(port);
        let mut overrides: Vec<SpecOverride> = Vec::new();

        out.ctx = Some(match s.ctx {
            Some(ctx) => {
                overrides.push(SpecOverride {
                    flag: "-c",
                    value: ctx.to_string(),
                    planned: plan.ctx.to_string(),
                });
                ctx
            }
            None => plan.ctx,
        });

        out.parallel = Some(match s.parallel {
            Some(np) => {
                overrides.push(SpecOverride {
                    flag: "-np",
                    value: np.to_string(),
                    planned: plan.parallel.to_string(),
                });
                np
            }
            None => plan.parallel,
        });

        out.kv_type = Some(match s.kv_type {
            Some(kv) => {
                overrides.push(SpecOverride {
                    flag: "-ctk",
                    value: kv.as_flag().to_owned(),
                    planned: plan.kv_type.as_flag().to_owned(),
                });
                kv
            }
            None => plan.kv_type,
        });

        // The one field the solver decides without ever being told the operator's answer, so
        // the one where an override is a real contradiction rather than a confirmation.
        out.ngl = match s.ngl {
            NglPlan::Auto => plan.ngl,
            explicit => {
                overrides.push(SpecOverride {
                    flag: "-ngl",
                    value: ngl_word(explicit),
                    planned: ngl_word(plan.ngl),
                });
                explicit
            }
        };

        out.split = SplitPlan {
            devices: if s.split.devices.is_empty() {
                plan.split.devices.clone()
            } else {
                overrides.push(SpecOverride {
                    flag: "-dev",
                    value: s.split.devices.join(","),
                    planned: plan.split.devices.join(","),
                });
                s.split.devices.clone()
            },
            mode: s.split.mode,
            main_gpu: s.split.main_gpu,
            tensor_split: s.split.tensor_split.clone(),
        };

        // The record has to carry what was executed, not only what was computed. `why` is
        // the one field that travels with the plan into `$STATE/endpoints/<id>.json` and out
        // to every surface that renders a fit, so the provenance goes there.
        for line in annotations(&out, &overrides) {
            if !plan.why.contains(&line) {
                plan.why.push(line);
            }
        }

        ResolvedSpec {
            spec: EndpointSpec::LocalLlama(out),
            fit: Some(plan),
            overrides,
        }
    }

    /// Re-derive the resolved spec of an endpoint that is already on disk, so a surface that
    /// only has the record can render the argv the child was really started with.
    ///
    /// `EndpointRecord::spec` deliberately stores the operator's **draft** — that is what
    /// keeps `same_endpoint` matching, and therefore what stops a restart leaving a second
    /// record behind — while the resolved numbers live in `EndpointRecord::fit`. Putting them
    /// back together is this function, and it is the whole of the fix for any caller that
    /// rebuilds an argv from a record.
    pub fn from_record(record: &EndpointRecord) -> ResolvedSpec {
        ResolvedSpec::resolve(
            &record.spec,
            record.port.unwrap_or_default(),
            record.fit.clone(),
        )
    }

    /// The spec argv is built from.
    pub fn spec(&self) -> &EndpointSpec {
        &self.spec
    }

    /// The plan this spec executes, annotated with the overrides. `None` when there was no
    /// plan to execute.
    pub fn fit(&self) -> Option<&FitPlan> {
        self.fit.as_ref()
    }

    /// Take the annotated plan, for storing on the [`super::LaunchPlan`].
    pub fn into_fit(self) -> Option<FitPlan> {
        self.fit
    }

    /// What the operator pinned by hand.
    pub fn overrides(&self) -> &[SpecOverride] {
        &self.overrides
    }

    /// The lines the operator must read before confirming.
    ///
    /// Only the **contradictions**: an override that merely confirms what the solver already
    /// planned is provenance, and putting it in front of the operator on every launch would
    /// train them to skip the list that matters.
    pub fn notes(&self) -> Vec<String> {
        self.overrides
            .iter()
            .filter(|o| o.contradicts_the_plan())
            .map(|o| {
                let extra = if o.flag == "-ngl" {
                    " — the solver sized the offload against live free VRAM; overruling it is \
                     how a launch OOMs"
                } else {
                    ""
                };
                format!(
                    "{} {} overrides the planned {}{extra}",
                    o.flag, o.value, o.planned
                )
            })
            .collect()
    }

    /// Every way `argv` fails to execute this plan. **Empty is the invariant.**
    ///
    /// The expectation is built from the [`FitPlan`] and the recorded overrides, **not** from
    /// the resolved spec. That distinction is the whole value of the check: comparing argv
    /// against the spec argv was rendered from can only catch a rendering bug, and the defect
    /// this closes was one layer earlier — the plan never reaching the spec at all. Checked
    /// against the plan, a resolution that quietly dropped every number fails loudly.
    ///
    /// Reports only *silent* disagreements:
    ///
    /// * a planned flag carrying the wrong value — including the case where a later
    ///   `extra_args` occurrence wins in llama.cpp's parser, which is why the **last**
    ///   occurrence of any spelling is the one compared;
    /// * a planned flag absent from argv with nothing in [`ArgvPreview::warnings`] to explain
    ///   it. A flag dropped because the build does not advertise it is already visible to the
    ///   operator, so it is a reported gap rather than a silent one.
    ///
    /// A spec with no plan has nothing to disagree with and returns empty.
    pub fn disagreements(&self, argv: &ArgvPreview) -> Vec<String> {
        let (EndpointSpec::LocalLlama(s), Some(plan)) = (&self.spec, self.fit.as_ref()) else {
            return Vec::new();
        };

        // An override is the operator's word standing in for the solver's, and it is on the
        // record; anything else must be the solver's number.
        let pinned = |flag: &str| {
            self.overrides
                .iter()
                .find(|o| o.flag == flag)
                .map(|o| o.value.clone())
        };

        let mut expected: Vec<(&str, String)> = vec![
            ("-c", pinned("-c").unwrap_or_else(|| plan.ctx.to_string())),
            (
                "-np",
                pinned("-np").unwrap_or_else(|| plan.parallel.to_string()),
            ),
        ];
        let kv = pinned("-ctk").unwrap_or_else(|| plan.kv_type.as_flag().to_owned());
        expected.push(("-ctk", kv.clone()));
        expected.push(("-ctv", kv));
        // `auto` is the plan deliberately emitting no `-ngl`, so there is nothing to check.
        let ngl = pinned("-ngl").unwrap_or_else(|| ngl_word(plan.ngl));
        if ngl != ngl_word(NglPlan::Auto) {
            expected.push(("-ngl", ngl));
        }
        let devices = pinned("-dev").unwrap_or_else(|| plan.split.devices.join(","));
        if !devices.is_empty() {
            expected.push(("-dev", devices));
        }
        expected.push(("-sm", split_mode_flag(plan.split.mode).to_owned()));
        // The port is the one thing the plan does not carry: it is leased, not solved.
        if let Some(port) = s.port {
            expected.push(("--port", port.to_string()));
        }

        let mut out = Vec::new();
        for (flag, want) in expected {
            match last_value_of(&argv.args, flag) {
                Some(got) if got == want => {}
                Some(got) => out.push(format!(
                    "argv ends with `{flag} {got}` where the plan says {want} \
                     (a later `extra_args` occurrence wins in llama.cpp's parser)"
                )),
                None if warned_about(&argv.warnings, flag) => {}
                None => out.push(format!(
                    "the plan says `{flag} {want}` but argv carries no {flag} at all, \
                     so llama.cpp will choose for itself"
                )),
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------------------

/// The `why` lines one resolution adds: every override, then the flags that carry the plan.
///
/// The summary line is the persisted evidence that the record and the argv describe the same
/// launch — the thing MK1 ACCEPTANCE had no way to check.
fn annotations(spec: &LocalLlamaSpec, overrides: &[SpecOverride]) -> Vec<String> {
    let mut out: Vec<String> = overrides.iter().map(SpecOverride::line).collect();
    let mut argv: Vec<String> = Vec::new();
    if let Some(ctx) = spec.ctx {
        argv.push(format!("-c {ctx}"));
    }
    if let Some(np) = spec.parallel {
        argv.push(format!("-np {np}"));
    }
    if let Some(kv) = spec.kv_type {
        argv.push(format!("-ctk {0} -ctv {0}", kv.as_flag()));
    }
    match ngl_arg(spec.ngl) {
        Some(n) => argv.push(format!("-ngl {n}")),
        None => argv.push("no -ngl (the plan leaves the offload to llama.cpp's own --fit)".into()),
    }
    if !spec.split.devices.is_empty() {
        argv.push(format!("-dev {}", spec.split.devices.join(",")));
    }
    argv.push(format!("-sm {}", split_mode_flag(spec.split.mode)));
    out.push(format!("{WHY_PREFIX} argv carries {}", argv.join(" ")));
    out
}

/// The `-ngl` value this plan becomes, or `None` when it emits no `-ngl` at all.
fn ngl_arg(ngl: NglPlan) -> Option<String> {
    match ngl {
        NglPlan::Auto => None,
        NglPlan::All => Some("999".to_owned()),
        NglPlan::Layers(n) => Some(n.to_string()),
    }
}

/// `NglPlan` as one word, for an override record. `Auto` has no argv form, so it is named.
fn ngl_word(ngl: NglPlan) -> String {
    ngl_arg(ngl).unwrap_or_else(|| "auto".to_owned())
}

/// The `-sm` value. Duplicated from `core::argv`, where it is private; the enum is the
/// shared truth and a divergence here would show up as a `disagreements` line, not a silent
/// launch.
fn split_mode_flag(mode: SplitMode) -> &'static str {
    match mode {
        SplitMode::None => "none",
        SplitMode::Layer => "layer",
        SplitMode::Row => "row",
        SplitMode::Tensor => "tensor",
    }
}

/// The value after the **last** occurrence of `flag` or any of its spellings.
///
/// Last, not first: llama.cpp's argument parser lets a later occurrence overwrite an earlier
/// one, so the last is what the process will actually run with.
fn last_value_of(args: &[String], flag: &str) -> Option<String> {
    let spellings: &[&str] = ALIASES
        .iter()
        .find(|(k, _)| *k == flag)
        .map(|(_, v)| *v)
        .unwrap_or(&[]);
    let mut found: Option<String> = None;
    for (i, arg) in args.iter().enumerate() {
        if spellings.contains(&arg.as_str()) {
            if let Some(v) = args.get(i + 1) {
                found = Some(v.clone());
            }
        }
    }
    found
}

/// Did the argv builder already say out loud that it dropped this flag?
///
/// `core::argv` renders "`<build>` does not advertise `<flag>`; it was not emitted" — the
/// backticks are what keep `-c` from matching the warning about `-ctk`.
fn warned_about(warnings: &[String], flag: &str) -> bool {
    let needle = format!("`{flag}`");
    warnings.iter().any(|w| w.contains(&needle))
}

/// The same spec, bound to a port. Mirrors `super::with_port` for the arms this module can
/// reach before it has a `LocalLlamaSpec` in hand.
fn with_port(spec: &EndpointSpec, port: u16) -> EndpointSpec {
    match spec {
        EndpointSpec::LocalLlama(s) => EndpointSpec::LocalLlama(LocalLlamaSpec {
            port: Some(port),
            ..s.clone()
        }),
        EndpointSpec::LocalVllm(s) => {
            let mut s = s.clone();
            s.port = Some(port);
            EndpointSpec::LocalVllm(s)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apexrouter_protocol::{BuildId, FitVerdict, KvType, SamplingMode};

    fn plan() -> FitPlan {
        FitPlan {
            ctx: 32_768,
            parallel: 4,
            kv_type: KvType::Q8_0,
            ngl: NglPlan::All,
            split: SplitPlan {
                devices: vec!["Vulkan0".to_owned()],
                mode: SplitMode::Layer,
                main_gpu: None,
                tensor_split: None,
            },
            weights_mb: 4_861,
            kv_mb: 594,
            compute_mb: 501,
            headroom_mb: 14_000,
            per_device_mb: vec![("Vulkan0".to_owned(), 5_956)],
            verdict: FitVerdict::Fits {
                headroom_mb: 14_000,
            },
            why: vec!["fits with 14000 MiB spare".to_owned()],
        }
    }

    fn draft() -> EndpointSpec {
        EndpointSpec::LocalLlama(LocalLlamaSpec {
            build: BuildId::parse("build-vulkan").expect("id"),
            model_path: "/models/Fake-9b-Q6_K.gguf".to_owned(),
            mmproj: None,
            alias_flag: "fake-9b".to_owned(),
            host: "127.0.0.1".to_owned(),
            port: None,
            ctx: None,
            parallel: None,
            kv_type: None,
            ngl: NglPlan::Auto,
            split: SplitPlan::default(),
            mode: SamplingMode::Coding,
            flash_attn: None,
            api_key: None,
            extra_args: Vec::new(),
        })
    }

    fn llama(r: &ResolvedSpec) -> &LocalLlamaSpec {
        match r.spec() {
            EndpointSpec::LocalLlama(s) => s,
            other => panic!("not a llama spec: {other:?}"),
        }
    }

    #[test]
    fn a_draft_that_asks_for_nothing_gets_every_number_from_the_solver() {
        let r = ResolvedSpec::resolve(&draft(), 8100, Some(plan()));
        let s = llama(&r);
        assert_eq!(s.ctx, Some(32_768));
        assert_eq!(s.parallel, Some(4), "the -np the defect never emitted");
        assert_eq!(s.kv_type, Some(KvType::Q8_0));
        assert_eq!(s.ngl, NglPlan::All);
        assert_eq!(s.split.devices, vec!["Vulkan0".to_owned()]);
        assert_eq!(s.port, Some(8100));
        assert!(r.overrides().is_empty(), "nothing was pinned by hand");
        assert!(r.notes().is_empty());
    }

    #[test]
    fn an_explicit_value_beats_the_solver_and_is_recorded_as_an_override() {
        let EndpointSpec::LocalLlama(mut s) = draft() else {
            unreachable!()
        };
        s.ctx = Some(8_192);
        s.ngl = NglPlan::Layers(20);
        let r = ResolvedSpec::resolve(&EndpointSpec::LocalLlama(s), 8100, Some(plan()));

        assert_eq!(llama(&r).ctx, Some(8_192));
        assert_eq!(llama(&r).ngl, NglPlan::Layers(20));

        let ctx = r
            .overrides()
            .iter()
            .find(|o| o.flag == "-c")
            .expect("-c recorded");
        assert_eq!(ctx.value, "8192");
        assert_eq!(ctx.planned, "32768");
        assert!(ctx.contradicts_the_plan());

        let ngl = r
            .overrides()
            .iter()
            .find(|o| o.flag == "-ngl")
            .expect("-ngl recorded");
        assert_eq!((ngl.value.as_str(), ngl.planned.as_str()), ("20", "999"));

        // …and the record carries it, not just the console.
        let why = &r.fit().expect("fit").why;
        assert!(
            why.iter().any(|w| w.contains("-c 8192 pinned")),
            "the override never reached the record: {why:?}"
        );
        assert!(r.notes().iter().any(|n| n.contains("-ngl 20 overrides")));
    }

    #[test]
    fn an_override_that_merely_confirms_the_plan_is_provenance_not_a_warning() {
        let EndpointSpec::LocalLlama(mut s) = draft() else {
            unreachable!()
        };
        s.parallel = Some(4); // exactly what the solver planned
        let r = ResolvedSpec::resolve(&EndpointSpec::LocalLlama(s), 8100, Some(plan()));
        let np = r
            .overrides()
            .iter()
            .find(|o| o.flag == "-np")
            .expect("recorded");
        assert!(!np.contradicts_the_plan());
        assert!(r.notes().is_empty(), "no warning for a confirmation");
    }

    #[test]
    fn resolving_twice_does_not_double_the_record() {
        let first = ResolvedSpec::resolve(&draft(), 8100, Some(plan()));
        let once = first.fit().expect("fit").why.len();
        let again = ResolvedSpec::resolve(&draft(), 8100, first.into_fit());
        assert_eq!(again.fit().expect("fit").why.len(), once);
    }

    #[test]
    fn a_spec_with_no_plan_resolves_to_itself_and_invents_nothing() {
        let r = ResolvedSpec::resolve(&draft(), 8100, None);
        let s = llama(&r);
        assert_eq!(s.ctx, None);
        assert_eq!(s.parallel, None);
        assert_eq!(s.ngl, NglPlan::Auto);
        assert_eq!(s.port, Some(8100));
        assert!(r.fit().is_none());
        assert!(r.disagreements(&preview(&[])).is_empty());
    }

    fn preview(args: &[&str]) -> ArgvPreview {
        ArgvPreview {
            program: "/bin/llama-server".to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: Vec::new(),
            cwd: "/state".to_owned(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn an_argv_that_carries_the_plan_disagrees_with_nothing() {
        let r = ResolvedSpec::resolve(&draft(), 8100, Some(plan()));
        let argv = preview(&[
            "-c", "32768", "-np", "4", "-ctk", "q8_0", "-ctv", "q8_0", "-ngl", "999", "-dev",
            "Vulkan0", "-sm", "layer", "--port", "8100",
        ]);
        let found = r.disagreements(&argv);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_missing_flag_is_the_defect_and_is_named() {
        let r = ResolvedSpec::resolve(&draft(), 8100, Some(plan()));
        // The MK1 argv exactly: no -np at all.
        let argv = preview(&[
            "-c", "32768", "-ctk", "q8_0", "-ctv", "q8_0", "-ngl", "999", "-dev", "Vulkan0", "-sm",
            "layer", "--port", "8100",
        ]);
        let found = r.disagreements(&argv);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("-np"), "{found:?}");
    }

    #[test]
    fn a_flag_the_build_could_not_emit_is_reported_not_silent_so_it_is_not_a_disagreement() {
        let r = ResolvedSpec::resolve(&draft(), 8100, Some(plan()));
        let mut argv = preview(&[
            "-c", "32768", "-ctk", "q8_0", "-ctv", "q8_0", "-ngl", "999", "-dev", "Vulkan0", "-sm",
            "layer", "--port", "8100",
        ]);
        argv.warnings
            .push("build-old does not advertise `-np`; it was not emitted".to_owned());
        assert!(r.disagreements(&argv).is_empty());
    }

    #[test]
    fn a_later_extra_arg_that_overwrites_the_plan_is_caught() {
        let r = ResolvedSpec::resolve(&draft(), 8100, Some(plan()));
        let argv = preview(&[
            "-c",
            "32768",
            "-np",
            "4",
            "-ctk",
            "q8_0",
            "-ctv",
            "q8_0",
            "-ngl",
            "999",
            "-dev",
            "Vulkan0",
            "-sm",
            "layer",
            "--port",
            "8100",
            // the operator's own words, last, which is what llama.cpp honours
            "--ctx-size",
            "999",
        ]);
        let found = r.disagreements(&argv);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("999"), "{found:?}");
        assert!(found[0].contains("32768"), "{found:?}");
    }

    #[test]
    fn the_operators_split_mode_survives_and_the_solvers_devices_fill_the_gap() {
        let EndpointSpec::LocalLlama(mut s) = draft() else {
            unreachable!()
        };
        s.split = SplitPlan {
            devices: Vec::new(),
            mode: SplitMode::Row,
            main_gpu: Some(1),
            tensor_split: Some(vec![0.6, 0.4]),
        };
        let r = ResolvedSpec::resolve(&EndpointSpec::LocalLlama(s), 8100, Some(plan()));
        let split = &llama(&r).split;
        assert_eq!(split.devices, vec!["Vulkan0".to_owned()], "from the plan");
        assert_eq!(split.mode, SplitMode::Row, "the solver never decides -sm");
        assert_eq!(split.main_gpu, Some(1));
        assert_eq!(split.tensor_split, Some(vec![0.6, 0.4]));
    }

    #[test]
    fn a_plan_that_leaves_the_offload_to_llama_cpp_emits_no_ngl() {
        let mut p = plan();
        p.ngl = NglPlan::Auto;
        let r = ResolvedSpec::resolve(&draft(), 8100, Some(p));
        assert_eq!(llama(&r).ngl, NglPlan::Auto);
        // …and an argv with no -ngl is then correct, not a disagreement.
        let argv = preview(&[
            "-c", "32768", "-np", "4", "-ctk", "q8_0", "-ctv", "q8_0", "-dev", "Vulkan0", "-sm",
            "layer", "--port", "8100",
        ]);
        assert!(r.disagreements(&argv).is_empty());
    }

    #[test]
    fn a_record_resolves_back_into_the_argv_its_child_was_started_with() {
        let record = EndpointRecord {
            id: apexrouter_protocol::BackendId::parse("fake-9b").expect("id"),
            spec: draft(),
            desired: apexrouter_protocol::DesiredState::Running,
            proc: None,
            port: Some(8100),
            log_path: None,
            started_at_unix: 0,
            fit: Some(plan()),
            adopted: false,
            alias_bindings: Vec::new(),
        };
        let r = ResolvedSpec::from_record(&record);
        assert_eq!(llama(&r).parallel, Some(4));
        assert_eq!(llama(&r).port, Some(8100));
    }
}
