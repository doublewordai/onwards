use axum::{self, Json, extract::State, response::IntoResponse};
use reqwest::{self};

use crate::{
    errors::GatewayError,
    models::{
        ChatCompletionParameters, ChatCompletionResponse, EmbeddingParameters, EmbeddingResponse,
    },
    state::AppState,
};

#[utoipa::path(
        post,
        path = "/v1/chat/completions",
        responses(
            (status = 200, description = "Takes in a JSON payload and returns the response all at once.", body = ChatCompletionResponse),
            (status = 422, description = "Malformed request body"),
            (status = 400, description = "Bad request"),
            (status = 503, description = "The server is not ready to process requests yet."),
        )
    )]
#[tracing::instrument(
    name = "openai",
    skip(state, openai_input_payload),
    fields(consumer_group)
)]
pub async fn chat_completions(
    State(state): State<AppState>,
    Json(openai_input_payload): Json<ChatCompletionParameters>,
) -> Result<impl IntoResponse, GatewayError> {
    unimplemented!()
}

#[utoipa::path(
        post,
        path = "/v1/embeddings",
        responses(
            (status = 200, description = "Takes in a JSON payload and returns the response all at once.", body = EmbeddingResponse),
            (status = 422, description = "Malformed request body"),
            (status = 400, description = "Bad request"),
            (status = 503, description = "The server is not ready to process requests yet."),
        )
    )]
#[tracing::instrument(
    name = "openai",
    skip(state, openai_input_payload),
    fields(consumer_group)
)]
pub async fn embeddings(
    State(state): State<AppState>,
    Json(openai_input_payload): Json<EmbeddingParameters>,
) -> Result<axum::Json<EmbeddingResponse>, GatewayError> {
    unimplemented!()
}
