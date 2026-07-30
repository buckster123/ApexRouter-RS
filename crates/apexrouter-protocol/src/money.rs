//! Money and honesty types. A guess must never render as a fact.
//!
//! [`Money`] is integer micro-USD: no float dust accumulates over an append-only usage log.
//! [`CostEstimate`] carries *how* a number was arrived at, and can say `Unknown`.
//! [`TokenCount`] distinguishes a count the provider reported from one we estimated.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Integer micro-USD. `Money(3_340_000)` is `$3.34`.
///
/// Integers because the ledger and the usage log are append-only and summed repeatedly;
/// f64 dust would make two readers of the same file disagree in the sixth decimal.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Money(pub i64);

impl Money {
    /// Zero dollars.
    pub const ZERO: Money = Money(0);

    /// Convert from dollars, rounding to the nearest micro-USD.
    ///
    /// A non-finite input yields [`Money::ZERO`] rather than a garbage integer.
    pub fn from_usd(usd: f64) -> Money {
        if !usd.is_finite() {
            return Money::ZERO;
        }
        let micros = (usd * 1_000_000.0).round();
        if micros >= i64::MAX as f64 {
            Money(i64::MAX)
        } else if micros <= i64::MIN as f64 {
            Money(i64::MIN)
        } else {
            Money(micros as i64)
        }
    }

    /// Convert to dollars for display or for a legacy `cost_usd` field.
    pub fn as_usd(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    /// Add without overflowing.
    pub fn saturating_add(self, o: Money) -> Money {
        Money(self.0.saturating_add(o.0))
    }

    /// Scale by a factor (tokens, hours, a throughput assumption), rounding to nearest micro.
    pub fn mul_f64(self, k: f64) -> Money {
        if !k.is_finite() {
            return Money::ZERO;
        }
        let v = (self.0 as f64 * k).round();
        if v >= i64::MAX as f64 {
            Money(i64::MAX)
        } else if v <= i64::MIN as f64 {
            Money(i64::MIN)
        } else {
            Money(v as i64)
        }
    }
}

impl fmt::Display for Money {
    /// Adaptive: whole cents render as `$3.34`, sub-cent values keep all six digits
    /// (`$0.000305`) so a per-token price never rounds to `$0.00`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        if abs % 10_000 == 0 {
            write!(
                f,
                "{sign}${}.{:02}",
                abs / 1_000_000,
                (abs % 1_000_000) / 10_000
            )
        } else {
            write!(f, "{sign}${}.{:06}", abs / 1_000_000, abs % 1_000_000)
        }
    }
}

/// Where a price came from. Rendered next to every money figure so a derived number is
/// visibly derived.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceSource {
    /// The provider's own API told us.
    ProviderApi,
    /// A vast.ai offer's `dph_total`.
    VastOffer,
    /// A table in `config.toml`.
    ConfigTable,
    /// A field the user typed into a recipe.
    RecipeField,
    /// Computed from other numbers — always paired with an `assumption`.
    Derived,
}

/// A cost, with its provenance and its honesty attached.
///
/// `Approximate` always carries the `assumption` that produced the number, so the UI can
/// render "at 100 tok/s" beside it instead of burying the constant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CostEstimate {
    /// The provider billed us this, or the arithmetic is exact.
    Metered {
        /// The amount.
        usd: Money,
        /// Where the price came from.
        source: PriceSource,
    },
    /// A number derived under a stated assumption.
    Approximate {
        /// The amount.
        usd: Money,
        /// Where the price came from.
        source: PriceSource,
        /// The assumption, in words, for the UI to render verbatim.
        assumption: String,
    },
    /// We do not know and will not invent one.
    Unknown,
}

impl CostEstimate {
    /// The amount, if there is one.
    pub fn usd(&self) -> Option<Money> {
        match self {
            CostEstimate::Metered { usd, .. } | CostEstimate::Approximate { usd, .. } => Some(*usd),
            CostEstimate::Unknown => None,
        }
    }

    /// True when this number must not be rendered as a fact.
    pub fn is_guess(&self) -> bool {
        !matches!(self, CostEstimate::Metered { .. })
    }

    /// Sum two estimates, degrading honestly.
    ///
    /// `Metered + Metered` stays `Metered` (the source becomes `Derived` if the two
    /// disagree). Anything touching an `Approximate` becomes `Approximate` with both
    /// assumptions carried. Adding to an `Unknown` yields an `Approximate` that says so —
    /// a whole day's spend must not collapse to "unknown" because one row lacked a price,
    /// and it must not silently claim to be complete either.
    ///
    /// Deliberately **not** `std::ops::Add`: `a + b` reads as arithmetic, and the whole
    /// point of this operation is that it can *demote* the result's honesty. A caller
    /// should see the word `add` and remember that.
    #[allow(clippy::should_implement_trait)]
    pub fn add(self, other: CostEstimate) -> CostEstimate {
        const MISSING: &str = "one or more components had no known price and count as $0";
        match (self, other) {
            (CostEstimate::Unknown, CostEstimate::Unknown) => CostEstimate::Unknown,
            (CostEstimate::Unknown, known) | (known, CostEstimate::Unknown) => {
                let (usd, source, mut assumption) = match known {
                    CostEstimate::Metered { usd, source } => (usd, source, String::new()),
                    CostEstimate::Approximate {
                        usd,
                        source,
                        assumption,
                    } => (usd, source, assumption),
                    CostEstimate::Unknown => unreachable!("both-Unknown handled above"),
                };
                if assumption.is_empty() {
                    assumption.push_str(MISSING);
                } else {
                    assumption.push_str("; ");
                    assumption.push_str(MISSING);
                }
                CostEstimate::Approximate {
                    usd,
                    source,
                    assumption,
                }
            }
            (
                CostEstimate::Metered { usd: a, source: sa },
                CostEstimate::Metered { usd: b, source: sb },
            ) => CostEstimate::Metered {
                usd: a.saturating_add(b),
                source: if sa == sb { sa } else { PriceSource::Derived },
            },
            (a, b) => {
                let usd = a
                    .usd()
                    .unwrap_or(Money::ZERO)
                    .saturating_add(b.usd().unwrap_or(Money::ZERO));
                let mut parts: Vec<String> = Vec::new();
                for e in [&a, &b] {
                    if let CostEstimate::Approximate { assumption, .. } = e {
                        if !assumption.is_empty() && !parts.iter().any(|p| p == assumption) {
                            parts.push(assumption.clone());
                        }
                    }
                }
                CostEstimate::Approximate {
                    usd,
                    source: PriceSource::Derived,
                    assumption: parts.join("; "),
                }
            }
        }
    }
}

/// A token count that remembers whether anybody actually counted.
///
/// llama.cpp reports real numbers in `timings`; a provider that streams without
/// `stream_options.include_usage` reports nothing, and the router degrades to `Estimated`
/// rather than presenting a fabricated number as billing truth.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", content = "n")]
pub enum TokenCount {
    /// The upstream told us this number.
    Reported(u32),
    /// We derived it. Never billed as fact.
    Estimated(u32),
}

impl TokenCount {
    /// The number, whichever kind it is.
    pub fn value(self) -> u32 {
        match self {
            TokenCount::Reported(n) | TokenCount::Estimated(n) => n,
        }
    }

    /// True when an upstream reported it.
    pub fn is_reported(self) -> bool {
        matches!(self, TokenCount::Reported(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_round_trips_as_a_bare_integer() {
        let m = Money(3_340_000);
        assert_eq!(serde_json::to_string(&m).expect("ser"), "3340000");
        assert_eq!(serde_json::from_str::<Money>("3340000").expect("de"), m);
    }

    #[test]
    fn money_display_is_adaptive() {
        assert_eq!(Money(3_340_000).to_string(), "$3.34");
        assert_eq!(Money(305).to_string(), "$0.000305");
        assert_eq!(Money::ZERO.to_string(), "$0.00");
        assert_eq!(Money(-3_340_000).to_string(), "-$3.34");
    }

    #[test]
    fn money_conversions_round_to_the_nearest_micro() {
        assert_eq!(Money::from_usd(3.34), Money(3_340_000));
        assert_eq!(Money::from_usd(0.000_000_5), Money(1));
        assert!((Money(3_340_000).as_usd() - 3.34).abs() < 1e-9);
        assert_eq!(Money::from_usd(f64::NAN), Money::ZERO);
        assert_eq!(Money(1_000_000).mul_f64(2.5), Money(2_500_000));
        assert_eq!(Money(i64::MAX).saturating_add(Money(1)), Money(i64::MAX));
    }

    #[test]
    fn cost_estimate_round_trips_every_variant() {
        let cases = [
            CostEstimate::Metered {
                usd: Money(1_000),
                source: PriceSource::ProviderApi,
            },
            CostEstimate::Approximate {
                usd: Money(9),
                source: PriceSource::VastOffer,
                assumption: "$0.34/hr at 100 tok/s".into(),
            },
            CostEstimate::Unknown,
        ];
        for c in cases {
            let s = serde_json::to_string(&c).expect("ser");
            assert_eq!(serde_json::from_str::<CostEstimate>(&s).expect("de"), c);
        }
        assert_eq!(
            serde_json::to_string(&CostEstimate::Unknown).expect("ser"),
            r#"{"kind":"unknown"}"#
        );
    }

    #[test]
    fn cost_estimate_add_degrades_honestly() {
        let metered = |n| CostEstimate::Metered {
            usd: Money(n),
            source: PriceSource::ProviderApi,
        };
        assert_eq!(
            metered(10).add(metered(5)),
            CostEstimate::Metered {
                usd: Money(15),
                source: PriceSource::ProviderApi
            }
        );

        let mixed = metered(10).add(CostEstimate::Approximate {
            usd: Money(5),
            source: PriceSource::VastOffer,
            assumption: "at 100 tok/s".into(),
        });
        assert!(mixed.is_guess());
        assert_eq!(mixed.usd(), Some(Money(15)));

        assert_eq!(
            CostEstimate::Unknown.add(CostEstimate::Unknown),
            CostEstimate::Unknown
        );
        let partial = metered(10).add(CostEstimate::Unknown);
        assert!(partial.is_guess());
        assert_eq!(partial.usd(), Some(Money(10)));
    }

    #[test]
    fn token_count_is_adjacently_tagged() {
        let t = TokenCount::Reported(42);
        assert_eq!(
            serde_json::to_string(&t).expect("ser"),
            r#"{"kind":"reported","n":42}"#
        );
        assert_eq!(
            serde_json::from_str::<TokenCount>(r#"{"kind":"estimated","n":7}"#).expect("de"),
            TokenCount::Estimated(7)
        );
        assert!(t.is_reported());
        assert_eq!(t.value(), 42);
        assert!(!TokenCount::Estimated(7).is_reported());
    }
}
