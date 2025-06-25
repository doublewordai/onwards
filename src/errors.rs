#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("Unknown error occurred: {0}")]
    UnknownError(String),
}
