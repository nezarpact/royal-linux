use std::collections::HashMap;
use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    #[error("Conflicting options: {0}")]
    ConflictingOptions(String),

    #[error("Missing required argument: {0}")]
    MissingArgument(String),

    #[error("Unknown command: {0}")]
    UnknownCommand(String),

    #[error("Unknown option: {0}")]
    UnknownOption(String),

    #[error("Prerequisite check failed")]
    PrerequisiteCheckFailed,

    #[error("Help requested")]
    HelpRequested,

    #[error("Configuration load failed: {0}")]
    ConfigLoadFailed(String),

    #[error("Bootstrap error: {0}")]
    BootstrapError(#[from] BootstrapError),

    #[error("Source error: {0}")]
    SourceError(#[from] SourceError),

    #[error("Extract error: {0}")]
    ExtractError(#[from] ExtractError),

    #[error("Build error: {0}")]
    BuildError(#[from] BuildError),

    #[error("Config error: {0}")]
    ConfigError(#[from] crate::config::ConfigError),
}

#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("Configuration error: {0}")]
    Config(#[from] crate::config::ConfigError),

    #[error("Source error: {0}")]
    Source(#[from] SourceError),

    #[error("Extraction error: {0}")]
    Extract(#[from] ExtractError),

    #[error("Build error: {0}")]
    Build(Box<BuildError>),

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
    #[expect(dead_code, reason = "validates archive format during extraction")]
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
    #[expect(dead_code, reason = "reports staging cleanup failures")]
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
    #[expect(dead_code, reason = "validates cache marker integrity")]
    InvalidCacheMarker(String),

    #[error("Cache marker checksum mismatch: expected {expected}, got {actual}")]
    #[expect(dead_code, reason = "detects cache corruption via checksum")]
    CacheChecksumMismatch { expected: String, actual: String },

    #[error("Failed to create unique staging directory after {attempts} attempts")]
    StagingCreationFailed { attempts: u32 },
}

impl From<std::io::Error> for ExtractError {
    fn from(err: std::io::Error) -> Self {
        ExtractError::ExtractionFailed(err.to_string())
    }
}

impl From<BuildError> for BootstrapError {
    fn from(err: BuildError) -> Self {
        BootstrapError::Build(Box::new(err))
    }
}

pub type SourceResult<T> = std::result::Result<T, SourceError>;
pub type ExtractResult<T> = std::result::Result<T, ExtractError>;
pub type BuildResult<T> = std::result::Result<T, BuildError>;

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Recipe not found: {0}")]
    RecipeNotFound(String),

    #[error("Recipe validation failed: {0}")]
    RecipeValidationFailed(String),

    #[error("Recipe load failed: {0}")]
    RecipeLoadFailed(String),

    #[error("Recipe parse failed: {0}")]
    RecipeParseFailed(String),

    #[error("Source not found: {0}")]
    SourceNotFound(String),

    #[error("Source not extracted: {0}")]
    SourceNotExtracted(String),

    #[error("Dependency failed: {0}")]
    DependencyFailed(String),

    #[error(
        "Build step failed: package={}, step={}, command={}, args={:?}, exit_code={}, workdir={}",
        .0.package, .0.step, .0.command, .0.args, .0.exit_code, .0.workdir
    )]
    StepFailed(Box<StepFailedInfo>),

    #[error("Execution failed: {0}")]
    ExecutionFailed(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Extraction error: {0}")]
    Extract(#[from] ExtractError),

    #[error("Source error: {0}")]
    Source(#[from] SourceError),
}

/// Detailed information about a failed build step.
/// Boxed to keep BuildError variant size small (clippy::result_large_err).
#[derive(Debug, Clone)]
pub struct StepFailedInfo {
    pub package: String,
    pub step: String,
    pub command: String,
    pub args: Vec<String>,
    pub exit_code: i32,
    #[allow(dead_code, reason = "kept for completeness and future diagnostics")]
    pub stdout: String,
    #[allow(dead_code, reason = "kept for completeness and future diagnostics")]
    pub stderr: String,
    /// Working directory where the step was executed
    pub workdir: String,
    #[allow(dead_code, reason = "kept for completeness and future diagnostics")]
    pub sanitized_env: HashMap<String, String>,
}

impl StepFailedInfo {
    /// Creates a new StepFailedInfo with all fields.
    #[allow(
        clippy::too_many_arguments,
        reason = "matches struct fields for error reporting"
    )]
    pub fn new(
        package: String,
        step: String,
        command: String,
        args: Vec<String>,
        exit_code: i32,
        stdout: String,
        stderr: String,
        workdir: String,
        sanitized_env: HashMap<String, String>,
    ) -> Self {
        Self {
            package,
            step,
            command,
            args,
            exit_code,
            stdout,
            stderr,
            workdir,
            sanitized_env,
        }
    }
}
