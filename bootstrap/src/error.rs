use std::io;
use thiserror::Error;

/// Errors that can occur during bootstrap operations.
#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("Configuration error: {0}")]
    Config(#[from] crate::config::ConfigError),

    #[error("Source error: {0}")]
    Source(#[from] SourceError),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Errors specific to source management.
#[derive(Debug, Error)]
pub enum SourceError {
    #[error("Invalid source definition: {0}")]
    InvalidDefinition(String),

    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    #[error("Invalid SHA-256 checksum: {0}")]
    InvalidChecksum(String),

    #[error("Archive filename is invalid: {0}")]
    InvalidArchiveName(String),

    #[error("Download failed: {0}")]
    DownloadFailed(String),

    #[error("Checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("Cached file is corrupted or missing: {0}")]
    CacheCorrupted(String),

    #[error("Source not found: {0}")]
    NotFound(String),

    #[error("HTTP error: {status} - {message}")]
    HttpError { status: u16, message: String },

    #[error("Incomplete download: expected {expected} bytes, got {actual} bytes")]
    IncompleteDownload { expected: u64, actual: u64 },
}

impl From<reqwest::Error> for SourceError {
    fn from(err: reqwest::Error) -> Self {
        if let Some(status) = err.status() {
            SourceError::HttpError {
                status: status.as_u16(),
                message: err.to_string(),
            }
        } else {
            SourceError::DownloadFailed(err.to_string())
        }
    }
}

impl From<hex::FromHexError> for SourceError {
    fn from(err: hex::FromHexError) -> Self {
        SourceError::InvalidChecksum(err.to_string())
    }
}

pub type SourceResult<T> = std::result::Result<T, SourceError>;
