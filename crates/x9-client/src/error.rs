use thiserror::Error;

#[derive(Debug, Error)]
pub enum X9Error {
    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("invalid base URL: {0}")]
    Url(#[from] url::ParseError),

    #[error("instrument returned status {status} for {path}: {body}")]
    Api {
        status: u16,
        path: String,
        body: String,
    },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, X9Error>;
