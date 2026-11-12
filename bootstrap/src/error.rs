use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("Configuration error: {0}")]
    Config(#[from] crate::config::ConfigError),

    #[error("Source error: {0}")]
    Source(#[from] SourceError),

    #[error("Extraction error: {0}")]
    Extract(#[from] ExtractError),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

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

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("Invalid archive format: {0}")]
    #[expect(dead_code)]
    InvalidFormat(String),

    #[error("Archive contains absolute path: {0}")]
    AbsolutePath(String),

    #[error("Archive contains path traversal: {0}")]
    PathTraversal(String),

    #[error("Archive contains unsafe symbolic link: {0}")]
    UnsafeSymlink(String),

    #[error("Archive contains unsafe hard link: {0}")]
    UnsafeHardlink(String),

    #[error("Archive contains unsupported special file: {0}")]
    UnsupportedSpecialFile(String),

    #[error("Extraction destination already exists: {0}")]
    DestinationExists(String),

    #[error("Extraction failed: {0}")]
    ExtractionFailed(String),

    #[error("Staging directory cleanup failed: {0}")]
    #[expect(dead_code)]
    StagingCleanupFailed(String),

    #[error("Source archive not found: {0}")]
    SourceNotFound(String),

    #[error("Archive type not supported: {0}")]
    UnsupportedArchiveType(String),

    #[error("Archive is malformed or truncated: {0}")]
    MalformedArchive(String),

    #[error("Symlink target escaped staging directory: {0}")]
    SymlinkEscape(String),

    #[error("Cache marker missing or invalid: {0}")]
    #[expect(dead_code)]
    InvalidCacheMarker(String),

    #[error("Cache marker checksum mismatch: expected {expected}, got {actual}")]
    #[expect(dead_code)]
    CacheChecksumMismatch { expected: String, actual: String },

    #[error("Failed to create unique staging directory after {attempts} attempts")]
    StagingCreationFailed { attempts: u32 },
}

impl From<std::io::Error> for ExtractError {
    fn from(err: std::io::Error) -> Self {
        ExtractError::ExtractionFailed(err.to_string())
    }
}

pub type SourceResult<T> = std::result::Result<T, SourceError>;
pub type ExtractResult<T> = std::result::Result<T, ExtractError>;
