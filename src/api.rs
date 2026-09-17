use axum::{extract::State, extract::Query, http::StatusCode, Json};
use serde::Serialize;

use crate::chains::LendingProvider;
use crate::http::HttpState;
use crate::service::types::{AllRatesResponse, QueryRatesParams};

#[derive(Serialize)]
pub struct ErrorBody {
    error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (
        status,
        Json(ErrorBody {
            error: msg.into(),
        }),
    )
}

/// GET /api/v1/rates — query rates with the same filters as the MCP tool.
/// `action`/`use_cache` from QueryRatesParams are accepted but `action` is ignored
/// (always "query").
/// "No results" sentinel returned by RateService::query when every source
/// either matched nothing or failed. For the REST API this is a normal empty
/// result, not a server error (the MCP tool keeps its historical error JSON).
const NO_RESULTS_MSG: &str = "No results found matching the query parameters";

pub async fn get_rates(
    State(state): State<HttpState>,
    Query(params): Query<QueryRatesParams>,
) -> Result<Json<AllRatesResponse>, (StatusCode, Json<ErrorBody>)> {
    let service = &state.service;
    match service.query(&params).await {
        Ok(resp) => Ok(Json(resp)),
        Err(e) if e.to_string() == NO_RESULTS_MSG => Ok(Json(AllRatesResponse {
            pools: Vec::new(),
            fetched_at: chrono::Utc::now().to_rfc3339(),
        })),
        Err(e) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query failed: {e}"),
        )),
    }
}

/// GET /api/v1/chains — supported chains (Aave V3 EVM chains + Stellar).
pub async fn get_chains(
    State(state): State<HttpState>,
) -> Result<Json<Vec<String>>, (StatusCode, Json<ErrorBody>)> {
    let mut chains = match state.service.aave_provider.list_pools().await {
        Ok(c) => c,
        Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    };
    if !chains.iter().any(|c| c == "stellar") {
        chains.push("stellar".to_string());
    }
    Ok(Json(chains))
}

/// GET /api/v1/protocols — supported protocol identifiers.
pub async fn get_protocols() -> Json<Vec<&'static str>> {
    Json(vec!["aave_v3", "spark", "blend"])
}

/// GET /api/v1/pools — monitored Blend pools.
pub async fn get_pools(State(state): State<HttpState>) -> Json<Vec<String>> {
    Json(state.service.list_pools().await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::types::QueryRatesParams;

    /// QueryRatesParams must deserialize from a query string with all filters
    /// optional and serde defaults applied (action defaults to "query").
    #[test]
    fn test_query_rates_params_query_string_defaults() {
        let params: QueryRatesParams = serde_urlencoded::from_str("").unwrap();
        assert_eq!(params.action, "query");
        assert!(params.chain.is_none());
        assert!(params.use_cache);
    }

    #[test]
    fn test_query_rates_params_query_string_filters() {
        let params: QueryRatesParams =
            serde_urlencoded::from_str("chain=ethereum&asset=USDC&min_supply_apy=0.05").unwrap();
        assert_eq!(params.chain.as_deref(), Some("ethereum"));
        assert_eq!(params.asset.as_deref(), Some("USDC"));
        assert_eq!(params.min_supply_apy, Some(0.05));
        assert!(params.max_borrow_apy.is_none());
    }
}
