#[derive(Debug, thiserror::Error)]
pub enum BrainError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("{transport} {status}: {body}")]
    Api {
        transport: &'static str,
        status: u16,
        body: String,
    },

    #[error("stream: {0}")]
    Stream(String),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, BrainError>;
