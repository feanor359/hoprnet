use axum::{
    extract::{Json, Path, State},
    http::status::StatusCode,
    response::IntoResponse,
};
use hopr_lib::Address;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::{collections::HashMap, str::FromStr, sync::Arc};

use hoprd_db_api::aliases::HoprdDbAliasesOperations;
use hoprd_db_api::errors::DbError;

use crate::{
    types::{HoprIdentifier, PeerOrAddress},
    ApiError, ApiErrorStatus, InternalState, BASE_PATH,
};

#[serde_as]
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(example = json!({
        "peerId": "12D3KooWRWeTozREYHzWTbuCYskdYhED1MXpDwTrmccwzFrd2mEA"
    }))]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerIdResponse {
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String)]
    pub peer_id: String,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[schema(example = json!({
        "address": "0x07eaf07d6624f741e04f4092a755a9027aaab7f6"
    }))]
#[serde(rename_all = "camelCase")]
pub(crate) struct AddressResponse {
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String)]
    pub address: Address,
}

#[serde_as]
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[schema(example = json!({
        "alias": "Alice",
        "destination": "12D3KooWRWeTozREYHzWTbuCYskdYhED1MXpDwTrmccwzFrd2mEA"
    }))]
#[serde(rename_all = "camelCase")]
pub(crate) struct AliasDestinationBodyRequest {
    pub alias: String,
    #[serde_as(as = "DisplayFromStr")]
    #[schema(value_type = String)]
    pub destination: PeerOrAddress,
}

/// (deprecated, will be removed in v3.0) Get each previously set alias and its corresponding PeerId as a hashmap.
#[utoipa::path(
        get,
        path = const_format::formatcp!("{BASE_PATH}/aliases"),
        responses(
            (status = 200, description = "Each alias with its corresponding PeerId", body = HashMap<String, String>, example = json!({
                    "alice": "12D3KooWPWD5P5ZzMRDckgfVaicY5JNoo7JywGotoAv17d7iKx1z",
                    "me": "12D3KooWJmLm8FnBfvYQ5BAZ5qcYBxQFFBzAAEYUBUNJNE8cRsYS"
            })),
            (status = 401, description = "Invalid authorization token.", body = ApiError),
            (status = 404, description = "No aliases found", body = ApiError),
        ),
        security(
            ("api_token" = []),
            ("bearer_token" = [])
        ),
        tag = "Alias",
    )]
pub(super) async fn aliases(State(state): State<Arc<InternalState>>) -> impl IntoResponse {
    let aliases = state.hoprd_db.get_aliases().await;

    match aliases {
        Ok(aliases) => {
            let aliases = aliases
                .iter()
                .map(|alias| (alias.alias.clone(), alias.peer_id.clone()))
                .collect::<HashMap<_, _>>();
            (StatusCode::OK, Json(aliases)).into_response()
        }
        Err(DbError::BackendError(_)) => (StatusCode::NOT_FOUND, ApiErrorStatus::AliasNotFound).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, ApiErrorStatus::DatabaseError).into_response(),
    }
}

// TODO(deprecated, will be removed in v3.0) Get each previously set alias and its corresponding ETH address as a hashmap.
#[utoipa::path(
        get,
        path = const_format::formatcp!("{BASE_PATH}/aliases_addresses"),
        responses(
            (status = 200, description = "Each alias with its corresponding Address", body = HashMap<String, String>, example = json!({
                    "alice": "0xb4ce7e6e36ac8b01a974725d5ba730af2b156fbe",
                    "me": "0x07eaf07d6624f741e04f4092a755a9027aaab7f6"
            })),
            (status = 401, description = "Invalid authorization token.", body = ApiError),
            (status = 404, description = "No aliases found", body = ApiError),
        ),
        security(
            ("api_token" = []),
            ("bearer_token" = [])
        ),
        tag = "Alias",
    )]
pub(super) async fn aliases_addresses(State(state): State<Arc<InternalState>>) -> impl IntoResponse {
    let aliases = state.hoprd_db.get_aliases().await;

    match aliases {
        Ok(aliases) => {
            let aliases = aliases
                .into_iter()
                .map(|alias| (alias.alias.clone(), alias.peer_id.clone()))
                .collect::<HashMap<_, _>>();

            let aliases_addresses_futures = aliases.into_iter().map(|(alias, peer_id)| {
                let hopr = state.hopr.clone();
                async move {
                    let peer_or_address = match PeerOrAddress::from_str(peer_id.as_str()) {
                        Ok(destination) => destination,
                        Err(_) => return (alias, ApiErrorStatus::PeerNotFound.to_string()),
                    };

                    match HoprIdentifier::new_with(peer_or_address, hopr.peer_resolver()).await {
                        Ok(destination) => (alias, destination.address.to_string()),
                        Err(_) => (alias, ApiErrorStatus::PeerNotFound.to_string()),
                    }
                }
            });

            let aliases_addresses = futures::future::join_all(aliases_addresses_futures)
                .await
                .into_iter()
                .collect::<HashMap<_, _>>();

            (StatusCode::OK, Json(aliases_addresses)).into_response()
        }
        Err(DbError::BackendError(_)) => (StatusCode::NOT_FOUND, ApiErrorStatus::AliasNotFound).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, ApiErrorStatus::DatabaseError).into_response(),
    }
}

/// (deprecated, will be removed in v3.0) Set alias for a peer with a specific PeerId.
#[utoipa::path(
        post,
        path = const_format::formatcp!("{BASE_PATH}/aliases"),
        request_body(
            content = AliasDestinationBodyRequest,
            description = "Alias name along with the PeerId to be aliased",
            content_type = "application/json"),
        responses(
            (status = 201, description = "Alias set successfully.", body = PeerIdResponse),
            (status = 400, description = "Invalid PeerId: The format or length of the peerId is incorrect.", body = ApiError),
            (status = 401, description = "Invalid authorization token.", body = ApiError),
            (status = 409, description = "Either alias exists or the peer_id is already aliased.", body = ApiError),
            (status = 422, description = "Unknown failure", body = ApiError)
        ),
        security(
            ("api_token" = []),
            ("bearer_token" = [])
        ),
        tag = "Alias",
    )]
pub(super) async fn set_alias(
    State(state): State<Arc<InternalState>>,
    Json(args): Json<AliasDestinationBodyRequest>,
) -> impl IntoResponse {
    let hopr = state.hopr.clone();

    match HoprIdentifier::new_with(args.destination, hopr.peer_resolver()).await {
        Ok(destination) => match state
            .hoprd_db
            .set_alias(destination.peer_id.to_string(), args.alias)
            .await
        {
            Ok(()) => (
                StatusCode::CREATED,
                Json(PeerIdResponse {
                    peer_id: destination.peer_id.to_string(),
                }),
            )
                .into_response(),
            Err(DbError::LogicalError(_)) => (StatusCode::BAD_REQUEST, ApiErrorStatus::PeerNotFound).into_response(),
            Err(DbError::AliasOrPeerIdAlreadyExists) => {
                (StatusCode::CONFLICT, ApiErrorStatus::AliasOrPeerIdAliasAlreadyExists).into_response()
            }
            Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, ApiErrorStatus::from(e)).into_response(),
        },
        Err(e) => (StatusCode::NOT_FOUND, e).into_response(),
    }
}

#[serde_as]
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(example = json!({
    "alias": "Alice",
}))]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetAliasRequest {
    alias: String,
}

/// (deprecated, will be removed in v3.0) Get alias for the PeerId (Hopr address) that have this alias assigned to it.
#[utoipa::path(
        get,
        path = const_format::formatcp!("{BASE_PATH}/aliases/{{alias}}"),
        params(
            ("alias" = String, Path, description = "Alias to be shown"),
        ),
        responses(
            (status = 200, description = "Get PeerId for an alias", body = PeerIdResponse),
            (status = 401, description = "Invalid authorization token.", body = ApiError),
            (status = 404, description = "PeerId not found", body = ApiError),
        ),
        security(
            ("api_token" = []),
            ("bearer_token" = [])
        ),
        tag = "Alias",
    )]
pub(super) async fn get_alias(
    Path(GetAliasRequest { alias }): Path<GetAliasRequest>,
    State(state): State<Arc<InternalState>>,
) -> impl IntoResponse {
    let alias = urlencoding::decode(&alias);

    if alias.is_err() {
        return (StatusCode::BAD_REQUEST, ApiErrorStatus::InvalidInput).into_response();
    }

    let alias = alias.unwrap().into_owned();

    match state.hoprd_db.resolve_alias(alias.clone()).await {
        Ok(Some(entry)) => (StatusCode::OK, Json(PeerIdResponse { peer_id: entry })).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, ApiErrorStatus::AliasNotFound).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, ApiErrorStatus::from(e)).into_response(),
    }
}

// TODO(deprecated, will be removed in v3.0) Get alias for the address (ETH address) that have this alias assigned to it.
#[utoipa::path(
        get,
        path = const_format::formatcp!("{BASE_PATH}/aliases_addresses/{{alias}}"),
        params(
            ("alias" = String, Path, description = "Alias to be shown"),
        ),
        responses(
            (status = 200, description = "Get ETH address for an alias", body = AddressResponse),
            (status = 401, description = "Invalid authorization token.", body = ApiError),
            (status = 404, description = "Address not found", body = ApiError),
        ),
        security(
            ("api_token" = []),
            ("bearer_token" = [])
        ),
        tag = "Alias",
    )]
pub(super) async fn get_alias_address(
    Path(GetAliasRequest { alias }): Path<GetAliasRequest>,
    State(state): State<Arc<InternalState>>,
) -> impl IntoResponse {
    let hopr = state.hopr.clone();

    let alias = urlencoding::decode(&alias);

    if alias.is_err() {
        return (StatusCode::BAD_REQUEST, ApiErrorStatus::InvalidInput).into_response();
    }

    let alias = alias.unwrap().into_owned();

    match state.hoprd_db.resolve_alias(alias.clone()).await {
        Ok(Some(entry)) => {
            let peer_or_address = match PeerOrAddress::from_str(entry.as_str()) {
                Ok(destination) => destination,
                Err(_) => return (StatusCode::NOT_FOUND, ApiErrorStatus::AliasNotFound).into_response(),
            };

            match HoprIdentifier::new_with(peer_or_address, hopr.peer_resolver()).await {
                Ok(destination) => (
                    StatusCode::OK,
                    Json(AddressResponse {
                        address: destination.address,
                    }),
                )
                    .into_response(),
                Err(_) => (StatusCode::BAD_REQUEST, ApiErrorStatus::PeerNotFound).into_response(),
            }
        }
        Ok(None) => (StatusCode::NOT_FOUND, ApiErrorStatus::AliasNotFound).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, ApiErrorStatus::from(e)).into_response(),
    }
}

#[serde_as]
#[derive(Deserialize, utoipa::ToSchema)]
#[schema(example = json!({
    "alias": "Alice",
}))]
pub(crate) struct DeleteAliasRequest {
    alias: String,
}

/// (deprecated, will be removed in v3.0) Delete an alias.
#[utoipa::path(
        delete,
        path = const_format::formatcp!("{BASE_PATH}/aliases/{{alias}}"),
        params(
            ("alias" = String, Path, description = "Alias to be shown"),
        ),
        responses(
            (status = 204, description = "Alias removed successfully"),
            (status = 401, description = "Invalid authorization token.", body = ApiError),
            (status = 422, description = "Unknown failure", body = ApiError)   // This can never happen
        ),
        security(
            ("api_token" = []),
            ("bearer_token" = [])
        ),
        tag = "Alias",
    )]
pub(super) async fn delete_alias(
    Path(DeleteAliasRequest { alias }): Path<DeleteAliasRequest>,
    State(state): State<Arc<InternalState>>,
) -> impl IntoResponse {
    let alias = urlencoding::decode(&alias);

    if alias.is_err() {
        return (StatusCode::BAD_REQUEST, ApiErrorStatus::InvalidInput).into_response();
    }

    let alias = alias.unwrap().into_owned();

    match state.hoprd_db.delete_alias(alias.clone()).await {
        Ok(_) => (StatusCode::NO_CONTENT,).into_response(),
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, ApiErrorStatus::from(e)).into_response(),
    }
}

/// (deprecated, will be removed in v3.0) Clear all aliases.
#[utoipa::path(
        delete,
        path = const_format::formatcp!("{BASE_PATH}/aliases"),
        responses(
            (status = 204, description = "All aliases removed successfully"),
            (status = 401, description = "Invalid authorization token.", body = ApiError),
            (status = 422, description = "Unknown failure", body = ApiError)   // This can never happen
        ),
        security(
            ("api_token" = []),
            ("bearer_token" = [])
        ),
        tag = "Alias",
    )]
pub(super) async fn clear_aliases(State(state): State<Arc<InternalState>>) -> impl IntoResponse {
    match state.hoprd_db.clear_aliases().await {
        Ok(_) => (StatusCode::NO_CONTENT,).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, ApiErrorStatus::from(e)).into_response(),
    }
}
