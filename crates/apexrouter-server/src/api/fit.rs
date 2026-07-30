//! OWNER: unit S-04 (server/src/api/{rig,fit,catalog,usage,requests,jobs}.rs,
//! server/src/jobs.rs). Do not edit outside that unit.
//!
//! `GET /v1/fit` (query form) and `POST /v1/fit` (body = [`FitInput`]). The same pure
//! function the CLI, the MCP tool and both Launch drawers call.
//!
//! The two verbs are deliberately different in kind, not just in encoding:
//!
//! * **`POST`** takes a whole [`FitInput`] and is *pure* — no disk, no rig, no reservations.
//!   It is the "what if" call: a GUI slider that wants a new verdict on every drag must not
//!   re-walk the model roots sixty times a second.
//! * **`GET`** is the *convenience* form. It resolves a model name against what is actually
//!   on disk and builds the budget from the **live** rig minus what running endpoints have
//!   already reserved, which is the same arithmetic `Provisioner::plan` does. So a `GET`
//!   verdict and a launch refusal agree, which is the whole point of having one solver.
//!
//! `why` is never empty on the way out: a number nobody can explain is a number nobody
//! should trust, and both GUIs render it as the tooltip beside every derived field.

use crate::api::rig::{find_model, local_model_list, model_not_found, rig_snapshot};
use crate::api::{ApiError, ApiResult};
use crate::state::AppState;
use apexrouter_core::fit::{budget_from_rig, fit};
use apexrouter_protocol::{FitInput, FitPlan, KvType, SplitMode, SplitPlan};
use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

/// `GET /v1/fit?model=&ctx=&parallel=&kv=&devices=&split_mode=&tensor_split=&main_gpu=`.
///
/// Every field but `model` is optional, and every omission means "let the solver choose",
/// never "assume a default that happens to be true on the laptop".
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FitQuery {
    /// Model id, display name, or the path of a shard.
    pub model: String,
    /// TOTAL context pool, shared across `parallel` slots. Omit to make the solver search.
    #[serde(default)]
    pub ctx: Option<u32>,
    /// Slot count (`-np`).
    #[serde(default)]
    pub parallel: Option<u32>,
    /// KV element type, spelled exactly as the `-ctk`/`-ctv` flag value.
    #[serde(default)]
    pub kv: Option<String>,
    /// Comma-separated `-dev` tokens. Empty selects every non-software GPU.
    #[serde(default)]
    pub devices: Option<String>,
    /// `-sm` value: `none`, `layer`, `row` or `tensor`.
    #[serde(default)]
    pub split_mode: Option<String>,
    /// Comma-separated `--tensor-split` ratios.
    #[serde(default)]
    pub tensor_split: Option<String>,
    /// `-mg` value.
    #[serde(default)]
    pub main_gpu: Option<u32>,
    /// Logical batch size, for the compute-buffer estimate.
    #[serde(default)]
    pub batch: Option<u32>,
}

/// The `/v1/fit` routes.
pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/v1/fit", get(fit_query).post(fit_body))
        .route("/v1/fit/input", post(fit_input_for))
}

/// `GET /v1/fit` — resolve a model, build a live budget, solve.
pub async fn fit_query(
    State(s): State<Arc<AppState>>,
    Query(q): Query<FitQuery>,
) -> ApiResult<FitPlan> {
    let input = build_input(&s, &q).await?;
    Ok(Json(fit(&input)))
}

/// `POST /v1/fit` — solve a caller-supplied [`FitInput`]. Pure: no disk, no rig.
pub async fn fit_body(Json(input): Json<FitInput>) -> ApiResult<FitPlan> {
    Ok(Json(fit(&input)))
}

/// `POST /v1/fit/input` — the [`FitInput`] a `GET` would have solved, without solving it.
///
/// Not in `ARCHITECTURE.md` §6.2's table, and additive to it: it is what lets a GUI resolve
/// the model and the live budget **once** and then hammer the pure `POST /v1/fit` while the
/// user drags a context slider, instead of re-walking the model roots on every frame.
pub async fn fit_input_for(
    State(s): State<Arc<AppState>>,
    Json(q): Json<FitQuery>,
) -> ApiResult<FitInput> {
    Ok(Json(build_input(&s, &q).await?))
}

/// Turn a [`FitQuery`] into a [`FitInput`] against what is really on this machine.
///
/// # Errors
/// `404` when no local model matches, `400` when the model's GGUF header could not be read
/// (the solver's arithmetic is entirely header-driven, so guessing here would be inventing
/// the answer), or when a flag value does not parse.
pub async fn build_input(state: &Arc<AppState>, q: &FitQuery) -> Result<FitInput, ApiError> {
    let models = local_model_list(state, false).await?;
    let model = find_model(&models, &q.model).ok_or_else(|| model_not_found(&q.model, &models))?;
    let gguf = model.gguf.clone().ok_or_else(|| {
        ApiError::bad_request(
            "unreadable_gguf",
            format!(
                "the GGUF header of `{}` could not be read, so its KV arithmetic cannot be \
                 computed; nothing here will guess it",
                model.id
            ),
        )
        .with_param("model")
    })?;

    let devices = parse_list(q.devices.as_deref());
    let rig = rig_snapshot(state, false).await?;
    let running = state.store.list_endpoints().unwrap_or_default();
    let cfg = state.cfg.load_full();
    let budget = budget_from_rig(&rig, &devices, cfg.endpoints.vram_margin_mb, &running);

    Ok(FitInput {
        weights_bytes: model.total_bytes,
        gguf,
        budget,
        want_ctx: q.ctx,
        want_parallel: q.parallel,
        want_kv: q.kv.as_deref().map(parse_kv).transpose()?,
        split: SplitPlan {
            devices,
            mode: q
                .split_mode
                .as_deref()
                .map(parse_split_mode)
                .transpose()?
                .unwrap_or(SplitMode::Layer),
            main_gpu: q.main_gpu,
            tensor_split: parse_ratios(q.tensor_split.as_deref())?,
        },
        batch: q.batch,
    })
}

/// Split a comma-separated list, dropping empties.
fn parse_list(v: Option<&str>) -> Vec<String> {
    v.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parse a `-ctk`/`-ctv` value through the same serde spelling the flag uses.
fn parse_kv(v: &str) -> Result<KvType, ApiError> {
    serde_json::from_value(serde_json::Value::String(v.trim().to_lowercase())).map_err(|_| {
        ApiError::bad_request(
            "invalid",
            format!(
                "`{v}` is not a KV type; expected one of \
                 f32, f16, bf16, q8_0, q5_1, q5_0, q4_1, q4_0, iq4_nl"
            ),
        )
        .with_param("kv")
    })
}

/// Parse an `-sm` value.
fn parse_split_mode(v: &str) -> Result<SplitMode, ApiError> {
    serde_json::from_value(serde_json::Value::String(v.trim().to_lowercase())).map_err(|_| {
        ApiError::bad_request(
            "invalid",
            format!("`{v}` is not a split mode; expected none, layer, row or tensor"),
        )
        .with_param("split_mode")
    })
}

/// Parse `--tensor-split` ratios. An absent or empty value is `None`, not `Some(vec![])` —
/// an empty ratio list would emit a flag with no argument.
fn parse_ratios(v: Option<&str>) -> Result<Option<Vec<f32>>, ApiError> {
    let raw = parse_list(v);
    if raw.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(raw.len());
    for part in raw {
        let n: f32 = part.parse().map_err(|_| {
            ApiError::bad_request(
                "invalid",
                format!("`{part}` is not a number; --tensor-split takes one ratio per device"),
            )
            .with_param("tensor_split")
        })?;
        if !n.is_finite() || n < 0.0 {
            return Err(ApiError::bad_request(
                "invalid",
                format!("`{part}` is not a usable ratio; ratios are finite and non-negative"),
            )
            .with_param("tensor_split"));
        }
        out.push(n);
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testkit::{app, test_config};
    use apexrouter_protocol::{DeviceBudget, FitVerdict, GgufMeta, VramBudget};

    /// Carnice-9b's real header, from `docs/port/00-machine-ground-truth.md`: 8 of 32
    /// attention layers carry KV, which is what makes its full 262144-token context fit on a
    /// 4 CU iGPU.
    fn carnice() -> GgufMeta {
        GgufMeta {
            arch: "qwen3".to_owned(),
            n_layer: 32,
            n_head_kv: 8,
            n_embd_head_k: 128,
            n_embd_head_v: 128,
            n_ctx_train: 262_144,
            full_attn_layers: Some(8),
            n_expert: None,
            quant_desc: Some("Q6_K".to_owned()),
        }
    }

    fn budget(free_mb: u64) -> VramBudget {
        VramBudget {
            devices: vec![DeviceBudget {
                device: "Vulkan0".to_owned(),
                free_mb,
                reserved_mb: 0,
            }],
            margin_mb: 1024,
            host_ram_free_mb: 8192,
        }
    }

    #[test]
    fn the_pure_solver_always_explains_itself() {
        let input = FitInput {
            weights_bytes: 7_000_000_000,
            gguf: carnice(),
            budget: budget(16_384),
            want_ctx: Some(32_768),
            want_parallel: Some(1),
            want_kv: Some(KvType::Q8_0),
            split: SplitPlan::default(),
            batch: None,
        };
        let plan = fit(&input);
        assert_eq!(plan.ctx, 32_768);
        assert!(!plan.why.is_empty(), "an unexplained verdict is a bug");
    }

    #[test]
    fn a_budget_that_cannot_hold_the_weights_does_not_pretend_it_can() {
        let input = FitInput {
            weights_bytes: 7_000_000_000,
            gguf: carnice(),
            budget: budget(2_048),
            want_ctx: Some(262_144),
            want_parallel: Some(1),
            want_kv: Some(KvType::Q8_0),
            split: SplitPlan::default(),
            batch: None,
        };
        let plan = fit(&input);
        assert!(
            !matches!(plan.verdict, FitVerdict::Fits { .. }),
            "{:?}",
            plan.verdict
        );
    }

    #[test]
    fn kv_and_split_mode_parse_by_their_flag_spelling() {
        assert_eq!(parse_kv("q8_0").expect("kv"), KvType::Q8_0);
        assert_eq!(parse_kv(" F16 ").expect("kv"), KvType::F16);
        assert_eq!(parse_kv("iq4_nl").expect("kv"), KvType::Iq4Nl);
        assert_eq!(parse_split_mode("layer").expect("mode"), SplitMode::Layer);
        let e = parse_kv("q3_k").expect_err("no such kv type");
        assert_eq!(e.body.param.as_deref(), Some("kv"));
        assert_eq!(e.status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn device_and_ratio_lists_are_trimmed_and_validated() {
        assert_eq!(
            parse_list(Some("Vulkan0, Vulkan1 ,")),
            vec!["Vulkan0".to_owned(), "Vulkan1".to_owned()]
        );
        assert_eq!(parse_list(None), Vec::<String>::new());
        assert_eq!(
            parse_ratios(Some("0.6,0.4")).expect("ok"),
            Some(vec![0.6, 0.4])
        );
        assert_eq!(parse_ratios(Some("  ")).expect("ok"), None);
        assert!(parse_ratios(Some("0.5,-1")).is_err());
        assert!(parse_ratios(Some("a")).is_err());
    }

    #[tokio::test]
    async fn an_unknown_model_is_a_404_that_names_what_is_known() {
        let mut cfg = test_config();
        let dir = tempfile::TempDir::new().expect("tempdir");
        cfg.endpoints.model_roots = vec![dir.path().display().to_string()];
        let state = app(cfg);
        crate::api::rig::invalidate_models();
        let err = build_input(
            &state,
            &FitQuery {
                model: "no-such-model".to_owned(),
                ..FitQuery::default()
            },
        )
        .await
        .expect_err("must not resolve");
        assert_eq!(err.status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(err.body.param.as_deref(), Some("model"));
        crate::api::rig::invalidate_models();
    }
}
