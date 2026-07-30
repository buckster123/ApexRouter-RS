//! OWNER: unit C-14 (core/usage.rs, core/pricing.rs). Do not edit outside that unit.
//!
//! The price table is **data fed by providers at runtime** — it never calls one, because
//! `core -> providers` would be a cycle.
//!
//! A `PerHour` price yields `CostEstimate::Approximate` with the throughput assumption **in
//! the string**, never a bare number. LocalRouter's `cost.py` buried a 100 tok/s constant;
//! this is the type that makes that impossible.
//!
//! ## What is in here
//!
//! * per-provider, per-model [`PriceModel`]s, as a provider's catalogue reported them;
//! * per-instance dollars-per-hour, as a vast.ai offer reported them.
//!
//! Nothing is persisted: the table is rebuilt from provider catalogues and the ledger at
//! startup, so a stale price can never outlive a process.

use apexrouter_protocol::{
    CostEstimate, InstanceId, Money, PriceModel, PriceSource, ProviderId, TokenCount,
};
use std::collections::BTreeMap;

/// Everything we currently believe about prices, from every source.
#[derive(Debug, Default)]
pub struct PriceTable {
    /* C-14 */
    /// provider → model → price. Both keys are normalised; see [`normalise`].
    providers: BTreeMap<String, BTreeMap<String, PriceModel>>,
    /// Rented instance → dollars per hour.
    instances: BTreeMap<u64, Money>,
}

impl PriceTable {
    /// Replace one provider's model prices. Called by the provider layer after a catalogue
    /// fetch.
    ///
    /// A *replacement*, not a merge: a model a provider stopped listing must stop having a
    /// price, or a delisted model keeps being quoted from a catalogue that no longer says so.
    pub fn set_provider_models(&mut self, id: &ProviderId, models: &[(String, PriceModel)]) {
        let table: BTreeMap<String, PriceModel> = models
            .iter()
            .map(|(model, price)| (normalise(model), price.clone()))
            .collect();
        self.providers.insert(normalise(id.as_str()), table);
    }

    /// Record what one rented instance is costing per hour.
    ///
    /// Reachable from [`estimate`](Self::estimate) by naming the instance in the `provider`
    /// argument — `"vast:12345"`, `"instance:12345"` or a bare `"12345"` — which is how a
    /// rented backend with no per-token price still produces a number.
    pub fn set_instance_dph(&mut self, id: InstanceId, dph: f64) {
        self.instances.insert(id.0, Money::from_usd(dph));
    }

    /// Estimate one request's cost, degrading to `Unknown` rather than inventing a number.
    ///
    /// The rules, in order:
    ///
    /// * no price for this `(provider, model)` and no instance behind `provider` →
    ///   [`CostEstimate::Unknown`]. **Never** a zero, which would read as "free".
    /// * [`PriceModel::Free`] → `Metered` zero.
    /// * [`PriceModel::PerToken`] → exact arithmetic; `Metered` when the upstream *reported*
    ///   both token counts, `Approximate` naming which count was estimated when it did not.
    /// * [`PriceModel::PerHour`] → wall-clock money divided by an assumed throughput. Without
    ///   a `tps_hint` that is unknowable, so the answer is `Unknown`; with one, the
    ///   assumption travels inside the returned string.
    pub fn estimate(
        &self,
        provider: &str,
        model: &str,
        prompt: TokenCount,
        completion: TokenCount,
        tps_hint: Option<f32>,
    ) -> CostEstimate {
        let Some(price) = self.lookup(provider, model) else {
            return CostEstimate::Unknown;
        };

        match price {
            PriceModel::Free => CostEstimate::Metered {
                usd: Money::ZERO,
                source: PriceSource::ConfigTable,
            },
            PriceModel::PerToken { input, output } => {
                let usd = input
                    .mul_f64(f64::from(prompt.value()) / 1_000_000.0)
                    .saturating_add(output.mul_f64(f64::from(completion.value()) / 1_000_000.0));
                match estimated_note(prompt, completion) {
                    None => CostEstimate::Metered {
                        usd,
                        source: PriceSource::ProviderApi,
                    },
                    Some(note) => CostEstimate::Approximate {
                        usd,
                        source: PriceSource::ProviderApi,
                        assumption: note,
                    },
                }
            }
            PriceModel::PerHour { .. } => {
                let total = u64::from(prompt.value()) + u64::from(completion.value());
                // One place owns the "$/hr at N tok/s" wording: the protocol type. We scale
                // its answer rather than re-deriving it, so the assumption cannot drift.
                match price.per_mtok(tps_hint) {
                    CostEstimate::Approximate {
                        usd,
                        source,
                        assumption,
                    } => {
                        let mut assumption =
                            format!("{assumption}; {total} tokens billed as wall-clock time");
                        if let Some(note) = estimated_note(prompt, completion) {
                            assumption.push_str("; ");
                            assumption.push_str(&note);
                        }
                        CostEstimate::Approximate {
                            usd: usd.mul_f64(total as f64 / 1_000_000.0),
                            source,
                            assumption,
                        }
                    }
                    // No throughput hint: an hourly price cannot be turned into a per-request
                    // number, and guessing one is exactly what cost.py did.
                    other => other,
                }
            }
        }
    }

    /// Find a price for `(provider, model)`, falling back to the instance behind `provider`.
    fn lookup(&self, provider: &str, model: &str) -> Option<PriceModel> {
        if let Some(price) = self
            .providers
            .get(&normalise(provider))
            .and_then(|models| models.get(&normalise(model)))
        {
            return Some(price.clone());
        }
        self.instance_dph(provider)
            .map(|dph| PriceModel::PerHour { dph })
    }

    /// The hourly price of the instance a provider string names, if it names one.
    fn instance_dph(&self, provider: &str) -> Option<Money> {
        let tail = provider.rsplit(':').next()?.trim();
        let id: u64 = tail.parse().ok()?;
        self.instances.get(&id).copied()
    }
}

/// Provider ids and model ids are matched case-insensitively, and `vast_gguf` is the same
/// provider as `vast-gguf` — that spelling split is a real trap in the legacy state.
fn normalise(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace('_', "-")
}

/// Name whichever token counts were estimated, or `None` when the upstream reported both.
fn estimated_note(prompt: TokenCount, completion: TokenCount) -> Option<String> {
    match (prompt.is_reported(), completion.is_reported()) {
        (true, true) => None,
        (false, true) => Some(format!(
            "prompt tokens estimated ({}), not reported by the upstream",
            prompt.value()
        )),
        (true, false) => Some(format!(
            "completion tokens estimated ({}), not reported by the upstream",
            completion.value()
        )),
        (false, false) => Some(format!(
            "both token counts estimated ({} prompt, {} completion), not reported by the upstream",
            prompt.value(),
            completion.value()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(s: &str) -> ProviderId {
        ProviderId::parse(s).expect("valid provider id")
    }

    fn together() -> PriceTable {
        let mut t = PriceTable::default();
        t.set_provider_models(
            &provider("together"),
            &[
                (
                    "meta-llama/Llama-3.1-8B-Instruct-Turbo".to_owned(),
                    PriceModel::PerToken {
                        input: Money::from_usd(0.18),
                        output: Money::from_usd(0.18),
                    },
                ),
                ("free-tier-model".to_owned(), PriceModel::Free),
            ],
        );
        t
    }

    #[test]
    fn an_unknown_provider_or_model_is_unknown_not_zero() {
        let t = together();
        assert_eq!(
            t.estimate(
                "nobody",
                "meta-llama/Llama-3.1-8B-Instruct-Turbo",
                TokenCount::Reported(100),
                TokenCount::Reported(50),
                None
            ),
            CostEstimate::Unknown
        );
        assert_eq!(
            t.estimate(
                "together",
                "a-model-nobody-lists",
                TokenCount::Reported(100),
                TokenCount::Reported(50),
                None
            ),
            CostEstimate::Unknown
        );
    }

    #[test]
    fn per_token_with_reported_counts_is_metered_and_exact() {
        let t = together();
        let e = t.estimate(
            "together",
            "meta-llama/Llama-3.1-8B-Instruct-Turbo",
            TokenCount::Reported(100),
            TokenCount::Reported(50),
            None,
        );
        // 150 tokens at $0.18/Mtok = $0.000027 — the number the real legacy log carries.
        assert_eq!(
            e,
            CostEstimate::Metered {
                usd: Money(27),
                source: PriceSource::ProviderApi,
            }
        );
        assert!(!e.is_guess());
    }

    #[test]
    fn an_estimated_token_count_demotes_the_answer_and_says_which() {
        let t = together();
        let e = t.estimate(
            "together",
            "meta-llama/Llama-3.1-8B-Instruct-Turbo",
            TokenCount::Reported(100),
            TokenCount::Estimated(50),
            None,
        );
        match e {
            CostEstimate::Approximate {
                usd, assumption, ..
            } => {
                assert_eq!(usd, Money(27), "the arithmetic is unchanged");
                assert!(
                    assumption.contains("completion tokens estimated (50)"),
                    "{assumption}"
                );
            }
            other => panic!("expected Approximate, got {other:?}"),
        }

        // Both estimated says both.
        let both = t.estimate(
            "together",
            "meta-llama/Llama-3.1-8B-Instruct-Turbo",
            TokenCount::Estimated(100),
            TokenCount::Estimated(50),
            None,
        );
        match both {
            CostEstimate::Approximate { assumption, .. } => {
                assert!(
                    assumption.contains("both token counts estimated"),
                    "{assumption}"
                );
            }
            other => panic!("expected Approximate, got {other:?}"),
        }
    }

    #[test]
    fn free_is_metered_zero() {
        let t = together();
        assert_eq!(
            t.estimate(
                "together",
                "free-tier-model",
                TokenCount::Estimated(1_000),
                TokenCount::Estimated(1_000),
                None
            ),
            CostEstimate::Metered {
                usd: Money::ZERO,
                source: PriceSource::ConfigTable,
            }
        );
    }

    #[test]
    fn provider_and_model_spelling_is_normalised() {
        let t = together();
        let e = t.estimate(
            "TOGETHER",
            "meta-llama/llama-3.1-8b-instruct-turbo",
            TokenCount::Reported(100),
            TokenCount::Reported(50),
            None,
        );
        assert_eq!(e.usd(), Some(Money(27)));

        // `vast_gguf` vs `vast-gguf` is the real trap; case is the cheap one.
        let mut t = PriceTable::default();
        t.set_provider_models(
            &provider("vast-gguf"),
            &[("m".to_owned(), PriceModel::Free)],
        );
        assert!(!t
            .estimate(
                "vast_gguf",
                "m",
                TokenCount::Reported(1),
                TokenCount::Reported(1),
                None
            )
            .is_guess());
    }

    // ---- the acceptance clause: PerHour never yields a bare number --------------------------

    fn rented() -> PriceTable {
        let mut t = PriceTable::default();
        t.set_provider_models(
            &provider("vast-gguf"),
            &[(
                "Qwen3.6-27B-Q8.gguf".to_owned(),
                PriceModel::PerHour {
                    dph: Money::from_usd(0.35),
                },
            )],
        );
        t
    }

    #[test]
    fn per_hour_without_a_throughput_hint_is_unknown() {
        assert_eq!(
            rented().estimate(
                "vast-gguf",
                "Qwen3.6-27B-Q8.gguf",
                TokenCount::Reported(100),
                TokenCount::Reported(50),
                None
            ),
            CostEstimate::Unknown,
            "an hourly price with no throughput is not a per-request price"
        );
    }

    #[test]
    fn per_hour_with_a_hint_is_approximate_with_the_assumption_in_the_string() {
        let e = rented().estimate(
            "vast-gguf",
            "Qwen3.6-27B-Q8.gguf",
            TokenCount::Reported(100_000),
            TokenCount::Reported(50_000),
            Some(40.0),
        );
        match e.clone() {
            CostEstimate::Approximate {
                usd,
                source,
                assumption,
            } => {
                assert_eq!(source, PriceSource::VastOffer);
                // $0.35/hr at 40 tok/s = 144000 tok/hr; 150000 tokens ≈ 1.0417 hr ≈ $0.3646.
                assert!(
                    (usd.as_usd() - 0.364_583).abs() < 0.001,
                    "{usd} is not the wall-clock arithmetic"
                );
                assert!(assumption.contains("40.0 tok/s"), "{assumption}");
                assert!(assumption.contains("$0.35"), "{assumption}");
                assert!(assumption.contains("150000 tokens"), "{assumption}");
            }
            other => panic!("expected Approximate, got {other:?}"),
        }
        assert!(
            e.is_guess(),
            "a throughput assumption is never billing truth"
        );
    }

    #[test]
    fn a_rented_instance_prices_by_the_hour_through_its_id() {
        let mut t = PriceTable::default();
        t.set_instance_dph(InstanceId(27_881_301), 0.35);

        // Every spelling the router might carry for "the box we rented".
        for provider in ["vast:27881301", "instance:27881301", "27881301"] {
            let e = t.estimate(
                provider,
                "whatever.gguf",
                TokenCount::Reported(100_000),
                TokenCount::Reported(50_000),
                Some(40.0),
            );
            assert!(
                matches!(e, CostEstimate::Approximate { .. }),
                "{provider}: {e:?}"
            );
        }
        // An instance we know nothing about stays unknown.
        assert_eq!(
            t.estimate(
                "vast:999",
                "whatever.gguf",
                TokenCount::Reported(1),
                TokenCount::Reported(1),
                Some(40.0)
            ),
            CostEstimate::Unknown
        );
    }

    #[test]
    fn a_catalogue_refresh_replaces_rather_than_merges() {
        let mut t = together();
        t.set_provider_models(
            &provider("together"),
            &[("new-model".to_owned(), PriceModel::Free)],
        );
        assert_eq!(
            t.estimate(
                "together",
                "meta-llama/Llama-3.1-8B-Instruct-Turbo",
                TokenCount::Reported(100),
                TokenCount::Reported(50),
                None
            ),
            CostEstimate::Unknown,
            "a delisted model must stop having a price"
        );
        assert!(!t
            .estimate(
                "together",
                "new-model",
                TokenCount::Reported(1),
                TokenCount::Reported(1),
                None
            )
            .is_guess());
    }
}
