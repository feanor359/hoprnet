use axum::{
    extract::{Json, State},
    http::status::StatusCode,
    response::IntoResponse,
};
use std::sync::Arc;

use crate::{ApiError, ApiErrorStatus, InternalState, BASE_PATH};

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
#[schema(example = json!({
    "price": "30000000000000000"
}))]
#[serde(rename_all = "camelCase")]
pub(crate) struct TicketPriceResponse {
    /// Price of the ticket in HOPR tokens.
    price: String,
}

/// Obtains the current ticket price.
#[utoipa::path(
        get,
        path = const_format::formatcp!("{BASE_PATH}/network/price"),
        responses(
            (status = 200, description = "Current ticket price", body = TicketPriceResponse),
            (status = 401, description = "Invalid authorization token.", body = ApiError),
            (status = 422, description = "Unknown failure", body = ApiError)
        ),
        security(
            ("api_token" = []),
            ("bearer_token" = [])
        ),
        tag = "Network"
    )]
pub(super) async fn price(State(state): State<Arc<InternalState>>) -> impl IntoResponse {
    let hopr = state.hopr.clone();

    match hopr.get_ticket_price().await {
        Ok(Some(price)) => (
            StatusCode::OK,
            Json(TicketPriceResponse {
                price: price.to_string(),
            }),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiErrorStatus::UnknownFailure("The ticket price is not available".into()),
        )
            .into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, ApiErrorStatus::from(e)).into_response(),
    }
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
#[schema(example = json!({
    "probability": 0.5
}))]
#[serde(rename_all = "camelCase")]
pub(crate) struct TicketProbabilityResponse {
    /// Winning probability of a ticket.
    probability: f64,
}

/// Gets the current minimum incoming ticket winning probability defined by the network.
#[utoipa::path(
        get,
        path = const_format::formatcp!("{BASE_PATH}/network/probability"),
        responses(
            (status = 200, description = "Minimum incoming ticket winning probability defined by the network", body = TicketProbabilityResponse),
            (status = 401, description = "Invalid authorization token.", body = ApiError),
            (status = 422, description = "Unknown failure", body = ApiError)
        ),
        security(
            ("api_token" = []),
            ("bearer_token" = [])
        ),
        tag = "Network"
    )]
pub(super) async fn probability(State(state): State<Arc<InternalState>>) -> impl IntoResponse {
    let hopr = state.hopr.clone();

    match hopr.get_minimum_incoming_ticket_win_probability().await {
        Ok(probability) => (StatusCode::OK, Json(TicketProbabilityResponse { probability })).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, ApiErrorStatus::from(e)).into_response(),
    }
}
