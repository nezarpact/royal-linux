use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::blocking::Client;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{SourceError, SourceResult};

/// Represents a source package definition.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct SourceDefinition {
    /// Package name (used as identifier).
    pub name: String,
    /// Package version.
    pub version: String,
    /// Download URL (HTTP or HTTPS).
    pub url: String,
    /// Expected SHA-256 checksum (64 hex characters).
    pub sha256: String,
    /// Archive filename to save as.
    pub archive: String,
}

impl SourceDefinition {
    /// Validates the source definition.
    pub fn validate(&self) -> SourceResult<()> {
        // Validate name
        if self.name.is_empty() {
            return Err(SourceError::InvalidDefinition(
                "package name cannot be empty".to_string(),
            ));
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(SourceError::InvalidDefinition(
                "package name can only contain alphanumeric characters, hyphens, and underscores"
                    .to_string(),
            ));
        }

        // Validate version
        if self.version.is_empty() {
            return Err(SourceError::InvalidDefinition(
                "version cannot be empty".to_string(),
            ));
        }

        // Validate URL
        if self.url.is_empty() {
            return Err(SourceError::InvalidDefinition(
                "URL cannot be empty".to_string(),
            ));
        }
        let parsed_url = url::Url::parse(&self.url)
            .map_err(|e| SourceError::InvalidUrl(format!("invalid URL '{}': {}", self.url, e)))?;
        if parsed_url.scheme() != "http" && parsed_url.scheme() != "https" {
            return Err(SourceError::InvalidUrl(
                "URL must use HTTP or HTTPS".to_string(),
            ));
        }
        // Require a valid host
        if parsed_url.host_str().is_none() {
            return Err(SourceError::InvalidUrl(
                "URL must have a valid host".to_string(),
            ));
        }

        // Validate SHA-256: normalize to lowercase and check format
        let normalized_sha256 = self.sha256.to_lowercase();
        if normalized_sha256.len() != 64 {
            return Err(SourceError::InvalidChecksum(format!(
                "SHA-256 must be 64 hex characters, got {}",
                normalized_sha256.len()
            )));
        }
        hex::decode(&normalized_sha256)
            .map_err(|e| SourceError::InvalidChecksum(format!("invalid hex in SHA-256: {}", e)))?;

        // Validate archive name
        if self.archive.is_empty() {
            return Err(SourceError::InvalidDefinition(
                "archive filename cannot be empty".to_string(),
            ));
        }
        if self.archive.contains('/') || self.archive.contains('\\') {
            return Err(SourceError::InvalidArchiveName(
                "archive filename cannot contain path separators".to_string(),
            ));
        }
        if self.archive == "." || self.archive == ".." {
            return Err(SourceError::InvalidArchiveName(
                "archive filename cannot be '.' or '..'".to_string(),
            ));
        }
        // Prevent path traversal attempts
        let path = Path::new(&self.archive);
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(SourceError::InvalidArchiveName(
                "archive filename cannot contain path traversal".to_string(),
            ));
        }
        // Reject other invalid filename components
        if self.archive.contains('\0') {
            return Err(SourceError::InvalidArchiveName(
                "archive filename cannot contain null bytes".to_string(),
            ));
        }

        Ok(())
    }

    /// Returns the normalized (lowercase) SHA-256 checksum.
    pub fn normalized_sha256(&self) -> String {
        self.sha256.to_lowercase()
    }

    /// Returns the cache key for this source.
    /// Uses full SHA-256 to prevent collisions between different source definitions.
    pub fn cache_key(&self) -> String {
        format!(
            "{}-{}-{}",
            self.name,
            self.version,
            self.normalized_sha256()
        )
    }
}

/// Result of a fetch operation.
#[derive(Debug, Clone)]
pub struct FetchResult {
    /// Path to the downloaded (and verified) archive.
    pub path: PathBuf,
    /// Whether the file was served from cache.
    pub from_cache: bool,
    /// The source definition that was fetched.
    pub definition: SourceDefinition,
}

/// Manages downloading and verifying source packages.
pub struct SourceManager {
    config: Config,
    client: Client,
    sources: HashMap<String, SourceDefinition>,
}

impl SourceManager {
    /// Creates a new source manager with the given configuration.
    pub fn new(config: Config) -> SourceResult<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| {
                SourceError::DownloadFailed(format!("failed to create HTTP client: {}", e))
            })?;

        Ok(Self {
            config,
            client,
            sources: HashMap::new(),
        })
    }

    /// Loads source definitions from a TOML file.
    pub fn load_sources<P: AsRef<Path>>(&mut self, path: P) -> SourceResult<()> {
        let content = fs::read_to_string(&path).map_err(|e| {
            SourceError::InvalidDefinition(format!(
                "failed to read sources file '{}': {}",
                path.as_ref().display(),
                e
            ))
        })?;

        let sources: HashMap<String, SourceDefinition> = toml::from_str(&content).map_err(|e| {
            SourceError::InvalidDefinition(format!("failed to parse sources TOML: {}", e))
        })?;

        // Validate each source definition
        for (name, def) in &sources {
            if name != &def.name {
                return Err(SourceError::InvalidDefinition(format!(
                    "source key '{}' does not match definition name '{}'",
                    name, def.name
                )));
            }
            def.validate()?;
        }

        self.sources = sources;
        Ok(())
    }

    /// Returns a reference to a source definition by name.
    pub fn get_source(&self, name: &str) -> Option<&SourceDefinition> {
        self.sources.get(name)
    }

    /// Returns all loaded source definitions, sorted deterministically by name.
    pub fn list_sources(&self) -> Vec<&SourceDefinition> {
        let mut sources: Vec<&SourceDefinition> = self.sources.values().collect();
        sources.sort_by(|a, b| a.name.cmp(&b.name));
        sources
    }

    /// Returns the source directory path.
    pub fn source_dir(&self) -> &Path {
        &self.config.directories.source
    }

    /// Returns the cache directory path (same as source directory for now).
    fn cache_dir(&self) -> &Path {
        &self.config.directories.source
    }

    /// Computes the expected cache path for a source definition.
    /// Uses cache key to prevent collisions between different source definitions.
    fn cache_path(&self, def: &SourceDefinition) -> PathBuf {
        self.cache_dir().join(def.cache_key())
    }

    /// Computes the temporary download path with unique identifier.
    fn temp_path(&self, def: &SourceDefinition) -> PathBuf {
        // Use a unique temp filename with PID to avoid conflicts between concurrent processes
        let pid = std::process::id();
        self.cache_dir()
            .join(format!("{}.{}.part", def.cache_key(), pid))
    }

    /// Verifies the SHA-256 checksum of a file.
    fn verify_checksum(&self, path: &Path, expected: &str) -> SourceResult<()> {
        let mut file = fs::File::open(path).map_err(|e| {
            SourceError::CacheCorrupted(format!("failed to open file for verification: {}", e))
        })?;

        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 8192];
        loop {
            let bytes_read = file.read(&mut buffer).map_err(|e| {
                SourceError::CacheCorrupted(format!("failed to read file for verification: {}", e))
            })?;
            if bytes_read == 0 {
                break;
            }
            hasher.update(&buffer[..bytes_read]);
        }

        let actual = hex::encode(hasher.finalize());
        if actual != expected {
            return Err(SourceError::ChecksumMismatch {
                expected: expected.to_string(),
                actual,
            });
        }

        Ok(())
    }

    /// Checks if a cached file exists and is valid.
    fn check_cache(&self, def: &SourceDefinition) -> bool {
        let cache_path = self.cache_path(def);
        if !cache_path.exists() {
            return false;
        }
        // Always re-verify cached files
        self.verify_checksum(&cache_path, &def.normalized_sha256())
            .is_ok()
    }

    /// Safely removes a temporary file, ignoring "not found" errors.
    fn cleanup_temp_file(&self, temp_path: &Path) {
        let _ = fs::remove_file(temp_path);
    }

    /// Downloads a source to a temporary file.
    fn download_to_temp(&self, def: &SourceDefinition, temp_path: &Path) -> SourceResult<()> {
        let mut response = self
            .client
            .get(&def.url)
            .send()
            .map_err(SourceError::from)?;

        if !response.status().is_success() {
            // Clean up temp file on HTTP error
            self.cleanup_temp_file(temp_path);
            return Err(SourceError::HttpError {
                status: response.status().as_u16(),
                message: format!("HTTP {}", response.status()),
            });
        }

        // Get content length if available
        let expected_len = response.content_length();

        let mut file = fs::File::create(temp_path).map_err(|e| {
            SourceError::DownloadFailed(format!("failed to create temp file: {}", e))
        })?;

        let mut downloaded: u64 = 0;
        let mut buffer = [0u8; 8192];
        loop {
            let bytes_read = response.read(&mut buffer).map_err(|e| {
                // Clean up temp file on read error
                self.cleanup_temp_file(temp_path);
                SourceError::DownloadFailed(format!("failed to read response: {}", e))
            })?;
            if bytes_read == 0 {
                break;
            }
            file.write_all(&buffer[..bytes_read]).map_err(|e| {
                // Clean up temp file on write error
                self.cleanup_temp_file(temp_path);
                SourceError::DownloadFailed(format!("failed to write to temp file: {}", e))
            })?;
            downloaded += bytes_read as u64;
        }

        // Verify content length if provided
        if let Some(expected) = expected_len {
            if downloaded != expected {
                self.cleanup_temp_file(temp_path);
                return Err(SourceError::IncompleteDownload {
                    expected,
                    actual: downloaded,
                });
            }
        }

        Ok(())
    }

    /// Fetches a single source by name.
    pub fn fetch(&mut self, name: &str, force: bool) -> SourceResult<FetchResult> {
        let def = self
            .sources
            .get(name)
            .ok_or_else(|| SourceError::NotFound(format!("source '{}' not found", name)))?
            .clone();

        // Check cache first (unless forced)
        if !force && self.check_cache(&def) {
            return Ok(FetchResult {
                path: self.cache_path(&def),
                from_cache: true,
                definition: def,
            });
        }

        // Ensure cache directory exists before downloading
        fs::create_dir_all(self.cache_dir()).map_err(|e| {
            SourceError::DownloadFailed(format!("failed to create cache directory: {}", e))
        })?;

        // Download to temporary file
        let temp_path = self.temp_path(&def);

        // Remove any existing temp file
        self.cleanup_temp_file(&temp_path);

        // Download
        self.download_to_temp(&def, &temp_path)?;

        // Verify checksum
        if let Err(e) = self.verify_checksum(&temp_path, &def.normalized_sha256()) {
            self.cleanup_temp_file(&temp_path);
            return Err(e);
        }

        // Atomic rename to final location
        let cache_path = self.cache_path(&def);
        fs::rename(&temp_path, &cache_path).map_err(|e| {
            // Clean up temp file on rename failure
            self.cleanup_temp_file(&temp_path);
            SourceError::DownloadFailed(format!("failed to move file to cache: {}", e))
        })?;

        Ok(FetchResult {
            path: cache_path,
            from_cache: false,
            definition: def,
        })
    }

    /// Fetches all loaded sources.
    pub fn fetch_all(&mut self, force: bool) -> Vec<SourceResult<FetchResult>> {
        let names: Vec<String> = self.sources.keys().cloned().collect();
        names
            .into_iter()
            .map(|name| self.fetch(&name, force))
            .collect()
    }
}

/// Loads source definitions from a TOML file.
pub fn load_sources<P: AsRef<Path>>(path: P) -> SourceResult<HashMap<String, SourceDefinition>> {
    let content = fs::read_to_string(&path).map_err(|e| {
        SourceError::InvalidDefinition(format!(
            "failed to read sources file '{}': {}",
            path.as_ref().display(),
            e
        ))
    })?;

    let sources: HashMap<String, SourceDefinition> = toml::from_str(&content).map_err(|e| {
        SourceError::InvalidDefinition(format!("failed to parse sources TOML: {}", e))
    })?;

    // Validate each source definition
    for (name, def) in &sources {
        if name != &def.name {
            return Err(SourceError::InvalidDefinition(format!(
                "source key '{}' does not match definition name '{}'",
                name, def.name
            )));
        }
        def.validate()?;
    }

    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::fs;
    use tempfile::tempdir;

    fn create_test_config() -> Config {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("bootstrap.toml");
        let config_content = format!(
            r#"
[target]
arch = "x86_64"

[directories]
workspace = "{}"
source = "workspace/sources"
build = "workspace/build"
output = "workspace/output"
toolchain = "workspace/toolchain"
sysroot = "workspace/sysroot"
logs = "workspace/logs"
"#,
            temp_dir.path().display()
        );
        fs::write(&config_path, config_content).unwrap();
        crate::config::load_config(&config_path).unwrap()
    }

    fn create_test_content() -> (Vec<u8>, String) {
        let content = b"test content for sha256 verification";
        let mut hasher = Sha256::new();
        hasher.update(content);
        let sha256 = hex::encode(hasher.finalize());
        (content.to_vec(), sha256)
    }

    #[test]
    fn test_source_definition_validate_valid() {
        let (_, sha256) = create_test_content();
        let def = SourceDefinition {
            name: "zlib".to_string(),
            version: "1.3.1".to_string(),
            url: "https://zlib.net/zlib-1.3.1.tar.gz".to_string(),
            sha256: sha256.clone(),
            archive: "zlib-1.3.1.tar.gz".to_string(),
        };
        assert!(def.validate().is_ok());
        assert_eq!(def.normalized_sha256(), sha256.to_lowercase());
    }

    #[test]
    fn test_source_definition_validate_uppercase_sha256() {
        let (_, sha256) = create_test_content();
        let def = SourceDefinition {
            name: "zlib".to_string(),
            version: "1.3.1".to_string(),
            url: "https://zlib.net/zlib-1.3.1.tar.gz".to_string(),
            sha256: sha256.to_uppercase(), // uppercase should be normalized
            archive: "zlib-1.3.1.tar.gz".to_string(),
        };
        assert!(def.validate().is_ok());
        assert_eq!(def.normalized_sha256(), sha256.to_lowercase());
    }

    #[test]
    fn test_source_definition_validate_empty_name() {
        let (_, sha256) = create_test_content();
        let def = SourceDefinition {
            name: "".to_string(),
            version: "1.3.1".to_string(),
            url: "https://example.com/file.tar.gz".to_string(),
            sha256,
            archive: "file.tar.gz".to_string(),
        };
        assert!(def.validate().is_err());
    }

    #[test]
    fn test_source_definition_validate_invalid_name_chars() {
        let (_, sha256) = create_test_content();
        let def = SourceDefinition {
            name: "zlib@1.3".to_string(),
            version: "1.3.1".to_string(),
            url: "https://example.com/file.tar.gz".to_string(),
            sha256,
            archive: "file.tar.gz".to_string(),
        };
        assert!(def.validate().is_err());
    }

    #[test]
    fn test_source_definition_validate_empty_version() {
        let (_, sha256) = create_test_content();
        let def = SourceDefinition {
            name: "zlib".to_string(),
            version: "".to_string(),
            url: "https://example.com/file.tar.gz".to_string(),
            sha256,
            archive: "file.tar.gz".to_string(),
        };
        assert!(def.validate().is_err());
    }

    #[test]
    fn test_source_definition_validate_invalid_url_scheme() {
        let (_, sha256) = create_test_content();
        let def = SourceDefinition {
            name: "zlib".to_string(),
            version: "1.3.1".to_string(),
            url: "ftp://example.com/file.tar.gz".to_string(),
            sha256,
            archive: "file.tar.gz".to_string(),
        };
        assert!(def.validate().is_err());
    }

    #[test]
    fn test_source_definition_validate_url_without_host() {
        let (_, sha256) = create_test_content();
        let def = SourceDefinition {
            name: "zlib".to_string(),
            version: "1.3.1".to_string(),
            url: "https://".to_string(),
            sha256,
            archive: "file.tar.gz".to_string(),
        };
        assert!(def.validate().is_err());
    }

    #[test]
    fn test_source_definition_validate_invalid_sha256_length() {
        let def = SourceDefinition {
            name: "zlib".to_string(),
            version: "1.3.1".to_string(),
            url: "https://example.com/file.tar.gz".to_string(),
            sha256: "abc".to_string(),
            archive: "file.tar.gz".to_string(),
        };
        assert!(def.validate().is_err());
    }

    #[test]
    fn test_source_definition_validate_invalid_sha256_hex() {
        let def = SourceDefinition {
            name: "zlib".to_string(),
            version: "1.3.1".to_string(),
            url: "https://example.com/file.tar.gz".to_string(),
            sha256: "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg".to_string(),
            archive: "file.tar.gz".to_string(),
        };
        assert!(def.validate().is_err());
    }

    #[test]
    fn test_source_definition_validate_archive_with_path_traversal() {
        let (_, sha256) = create_test_content();
        let def = SourceDefinition {
            name: "zlib".to_string(),
            version: "1.3.1".to_string(),
            url: "https://example.com/file.tar.gz".to_string(),
            sha256,
            archive: "../etc/passwd".to_string(),
        };
        assert!(def.validate().is_err());
    }

    #[test]
    fn test_source_definition_validate_archive_with_slash() {
        let (_, sha256) = create_test_content();
        let def = SourceDefinition {
            name: "zlib".to_string(),
            version: "1.3.1".to_string(),
            url: "https://example.com/file.tar.gz".to_string(),
            sha256,
            archive: "subdir/file.tar.gz".to_string(),
        };
        assert!(def.validate().is_err());
    }

    #[test]
    fn test_source_definition_validate_archive_with_null_byte() {
        let (_, sha256) = create_test_content();
        let def = SourceDefinition {
            name: "zlib".to_string(),
            version: "1.3.1".to_string(),
            url: "https://example.com/file.tar.gz".to_string(),
            sha256,
            archive: "file\0.tar.gz".to_string(),
        };
        assert!(def.validate().is_err());
    }

    #[test]
    fn test_cache_key() {
        let (_, sha256) = create_test_content();
        let def = SourceDefinition {
            name: "zlib".to_string(),
            version: "1.3.1".to_string(),
            url: "https://example.com/file.tar.gz".to_string(),
            sha256,
            archive: "zlib-1.3.1.tar.gz".to_string(),
        };
        let key = def.cache_key();
        assert!(key.starts_with("zlib-1.3.1-"));
        assert_eq!(key.len(), "zlib-1.3.1-".len() + 64); // full SHA-256
    }

    #[test]
    fn test_cache_key_collision_resistance() {
        let (_, sha256_a) = create_test_content();
        // Use different content for the second hash
        let content_b = b"different test content for sha256 verification";
        let mut hasher = Sha256::new();
        hasher.update(content_b);
        let sha256_b = hex::encode(hasher.finalize());

        let def_a = SourceDefinition {
            name: "pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/a.tar.gz".to_string(),
            sha256: sha256_a,
            archive: "pkg-1.0.tar.gz".to_string(),
        };

        let def_b = SourceDefinition {
            name: "pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/b.tar.gz".to_string(),
            sha256: sha256_b,
            archive: "pkg-1.0.tar.gz".to_string(),
        };

        // Different SHA-256 should produce different cache keys
        assert_ne!(def_a.cache_key(), def_b.cache_key());
    }

    #[test]
    fn test_load_sources_valid() {
        let temp_dir = tempdir().unwrap();
        let sources_path = temp_dir.path().join("sources.toml");
        let (_, sha256) = create_test_content();
        let content = format!(
            r#"
zlib = {{ name = "zlib", version = "1.3.1", url = "https://example.com/zlib.tar.gz", sha256 = "{}", archive = "zlib-1.3.1.tar.gz" }}
"#,
            sha256
        );
        fs::write(&sources_path, content).unwrap();

        let sources = load_sources(&sources_path).unwrap();
        assert_eq!(sources.len(), 1);
        assert!(sources.contains_key("zlib"));
    }

    #[test]
    fn test_load_sources_key_mismatch() {
        let temp_dir = tempdir().unwrap();
        let sources_path = temp_dir.path().join("sources.toml");
        let (_, sha256) = create_test_content();
        let content = format!(
            r#"
wrong_key = {{ name = "zlib", version = "1.3.1", url = "https://example.com/zlib.tar.gz", sha256 = "{}", archive = "zlib.tar.gz" }}
"#,
            sha256
        );
        fs::write(&sources_path, content).unwrap();

        let result = load_sources(&sources_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_sources_malformed_toml() {
        let temp_dir = tempdir().unwrap();
        let sources_path = temp_dir.path().join("sources.toml");
        let content = r#"
[target
arch = "x86_64"
"#;
        fs::write(&sources_path, content).unwrap();

        let result = load_sources(&sources_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_source_manager_creation() {
        let config = create_test_config();
        let manager = SourceManager::new(config);
        assert!(manager.is_ok());
    }

    // Integration tests with mock HTTP server
    mod integration {
        use super::*;

        // Returns (MockServer, test_url, expected_sha256)
        fn setup_test_server(content: &[u8]) -> (MockServer, String, String) {
            let server = MockServer::start();
            let base_url = server.base_url();
            let mut hasher = Sha256::new();
            hasher.update(content);
            let sha256 = hex::encode(hasher.finalize());

            (server, format!("{}/test.tar.gz", base_url), sha256)
        }

        fn create_manager_with_source(
            config: Config,
            source_name: &str,
            url: &str,
            sha256: &str,
        ) -> SourceManager {
            let mut manager = SourceManager::new(config).unwrap();
            let sources = HashMap::from([(
                source_name.to_string(),
                SourceDefinition {
                    name: source_name.to_string(),
                    version: "1.0".to_string(),
                    url: url.to_string(),
                    sha256: sha256.to_string(),
                    archive: "test.tar.gz".to_string(),
                },
            )]);
            manager.sources = sources;
            manager
        }

        #[test]
        fn test_successful_download_with_correct_checksum() {
            let content = b"hello world test content for download";
            let (server, url, sha256) = setup_test_server(content);
            let mock = server.mock(|when, then| {
                when.method(GET).path("/test.tar.gz");
                then.status(200)
                    .header("content-type", "application/gzip")
                    .body(content);
            });

            let config = create_test_config();
            let mut manager = create_manager_with_source(config, "test-pkg", &url, &sha256);

            let result = manager.fetch("test-pkg", false).unwrap();

            assert!(!result.from_cache);
            assert!(result.path.exists());

            // Verify file content
            let downloaded = fs::read(&result.path).unwrap();
            assert_eq!(downloaded, content);

            // Verify mock was called
            mock.assert();

            // Second fetch should hit cache
            let result2 = manager.fetch("test-pkg", false).unwrap();
            assert!(result2.from_cache);
            assert_eq!(result2.path, result.path);
        }

        #[test]
        fn test_checksum_mismatch() {
            let content = b"actual content from server";
            let (server, url, _) = setup_test_server(content);
            let mock = server.mock(|when, then| {
                when.method(GET).path("/test.tar.gz");
                then.status(200).body(content);
            });

            // Use wrong checksum
            let wrong_sha256 = "0000000000000000000000000000000000000000000000000000000000000000";

            let config = create_test_config();
            let mut manager = create_manager_with_source(config, "test-pkg", &url, wrong_sha256);

            let result = manager.fetch("test-pkg", false);

            assert!(result.is_err());
            match result.unwrap_err() {
                SourceError::ChecksumMismatch { expected, actual } => {
                    assert_eq!(expected, wrong_sha256);
                    assert_ne!(actual, wrong_sha256);
                }
                e => panic!("Expected ChecksumMismatch, got {:?}", e),
            }

            // Cache should not contain the file
            let cache_path = manager
                .cache_dir()
                .join(format!("test-pkg-1.0-{}", wrong_sha256));
            assert!(!cache_path.exists());

            // Mock was called once for the failed attempt
            mock.assert_hits(1);
        }

        #[test]
        fn test_http_error_response() {
            let server = MockServer::start();
            let mock = server.mock(|when, then| {
                when.method(GET).path("/notfound.tar.gz");
                then.status(404).body("Not Found");
            });

            let url = format!("{}/notfound.tar.gz", server.base_url());
            let config = create_test_config();
            let mut manager = create_manager_with_source(
                config,
                "test-pkg",
                &url,
                "0000000000000000000000000000000000000000000000000000000000000000",
            );

            let result = manager.fetch("test-pkg", false);

            assert!(result.is_err());
            match result.unwrap_err() {
                SourceError::HttpError { status, .. } => {
                    assert_eq!(status, 404);
                }
                e => panic!("Expected HttpError, got {:?}", e),
            }

            mock.assert();
        }

        #[test]
        fn test_incomplete_download_content_length() {
            let content = b"short content";
            let server = MockServer::start();

            // Server provides correct content-length
            let mock = server.mock(|when, then| {
                when.method(GET).path("/test.tar.gz");
                then.status(200)
                    .header("content-length", &content.len().to_string())
                    .body(content);
            });

            let url = format!("{}/test.tar.gz", server.base_url());
            let mut hasher = Sha256::new();
            hasher.update(content);
            let sha256 = hex::encode(hasher.finalize());

            let config = create_test_config();
            let mut manager = create_manager_with_source(config, "test-pkg", &url, &sha256);

            let result = manager.fetch("test-pkg", false);

            // Should succeed with correct content-length
            assert!(
                result.is_ok(),
                "Download with correct content-length should succeed: {:?}",
                result.err()
            );

            mock.assert();
        }

        #[test]
        fn test_cache_hit() {
            let content = b"cache hit test content";
            let (server, url, sha256) = setup_test_server(content);
            let mock = server.mock(|when, then| {
                when.method(GET).path("/test.tar.gz");
                then.status(200).body(content);
            });

            let config = create_test_config();
            let mut manager = create_manager_with_source(config, "test-pkg", &url, &sha256);

            // First download
            let result1 = manager.fetch("test-pkg", false).unwrap();
            assert!(!result1.from_cache);
            mock.assert_hits(1);

            // Second fetch should use cache
            let result2 = manager.fetch("test-pkg", false).unwrap();
            assert!(result2.from_cache);
            assert_eq!(result2.path, result1.path);
            // Mock should not be called again
            mock.assert_hits(1);
        }

        #[test]
        fn test_corrupted_cache_detection_and_replacement() {
            let content = b"original content";
            let (server, url, sha256) = setup_test_server(content);
            let mock = server.mock(|when, then| {
                when.method(GET).path("/test.tar.gz");
                then.status(200).body(content);
            });

            let config = create_test_config();
            let mut manager = create_manager_with_source(config, "test-pkg", &url, &sha256);

            // First download and cache
            let result1 = manager.fetch("test-pkg", false).unwrap();
            assert!(!result1.from_cache);
            mock.assert_hits(1);

            // Corrupt the cached file
            fs::write(&result1.path, b"corrupted content").unwrap();

            // Next fetch should detect corruption and re-download
            let result2 = manager.fetch("test-pkg", false).unwrap();
            assert!(!result2.from_cache);
            assert_eq!(result2.path, result1.path);
            mock.assert_hits(2);

            // File should be restored to original content
            let downloaded = fs::read(&result2.path).unwrap();
            assert_eq!(downloaded, content);
        }

        #[test]
        fn test_forced_redownload() {
            let content = b"original content";
            let (server, url, sha256) = setup_test_server(content);
            let mock = server.mock(|when, then| {
                when.method(GET).path("/test.tar.gz");
                then.status(200).body(content);
            });

            let config = create_test_config();
            let mut manager = create_manager_with_source(config, "test-pkg", &url, &sha256);

            // First download
            let result1 = manager.fetch("test-pkg", false).unwrap();
            assert!(!result1.from_cache);
            mock.assert_hits(1);

            // Force re-download
            let result2 = manager.fetch("test-pkg", true).unwrap();
            assert!(!result2.from_cache);
            assert_eq!(result2.path, result1.path);
            mock.assert_hits(2);
        }

        #[test]
        fn test_temp_file_cleanup_after_failure() {
            let content = b"content that will fail checksum";
            let (server, url, _) = setup_test_server(content);
            let mock = server.mock(|when, then| {
                when.method(GET).path("/test.tar.gz");
                then.status(200).body(content);
            });

            // Use wrong checksum to trigger verification failure
            let wrong_sha256 = "0000000000000000000000000000000000000000000000000000000000000000";

            let config = create_test_config();
            let mut manager = create_manager_with_source(config, "test-pkg", &url, wrong_sha256);

            let result = manager.fetch("test-pkg", false);
            assert!(result.is_err());
            mock.assert_hits(1);

            // Check that no temp files remain
            let cache_dir = manager.cache_dir();
            let entries: Vec<_> = fs::read_dir(cache_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
                .collect();
            assert!(
                entries.is_empty(),
                "Temp files should be cleaned up after verification failure"
            );
        }

        #[test]
        fn test_multiple_downloads_no_temp_conflicts() {
            let content1 = b"content for package 1";
            let content2 = b"content for package 2";

            let server = MockServer::start();

            let mut hasher = Sha256::new();
            hasher.update(content1);
            let sha256_1 = hex::encode(hasher.finalize());

            let mut hasher = Sha256::new();
            hasher.update(content2);
            let sha256_2 = hex::encode(hasher.finalize());

            let mock1 = server.mock(|when, then| {
                when.method(GET).path("/pkg1.tar.gz");
                then.status(200).body(content1);
            });

            let mock2 = server.mock(|when, then| {
                when.method(GET).path("/pkg2.tar.gz");
                then.status(200).body(content2);
            });

            let config = create_test_config();
            let mut manager = SourceManager::new(config).unwrap();
            manager.sources = HashMap::from([
                (
                    "pkg1".to_string(),
                    SourceDefinition {
                        name: "pkg1".to_string(),
                        version: "1.0".to_string(),
                        url: format!("{}/pkg1.tar.gz", server.base_url()),
                        sha256: sha256_1,
                        archive: "pkg1.tar.gz".to_string(),
                    },
                ),
                (
                    "pkg2".to_string(),
                    SourceDefinition {
                        name: "pkg2".to_string(),
                        version: "1.0".to_string(),
                        url: format!("{}/pkg2.tar.gz", server.base_url()),
                        sha256: sha256_2,
                        archive: "pkg2.tar.gz".to_string(),
                    },
                ),
            ]);

            // Fetch both - should not conflict on temp files
            let results = manager.fetch_all(false);

            assert_eq!(results.len(), 2);
            for result in results {
                assert!(result.is_ok());
            }

            mock1.assert();
            mock2.assert();

            // No temp files should remain
            let cache_dir = manager.cache_dir();
            let entries: Vec<_> = fs::read_dir(cache_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
                .collect();
            assert!(entries.is_empty(), "Temp files should be cleaned up");
        }

        #[test]
        fn test_cache_naming_collisions_prevented() {
            // Two different packages with same archive name but different content
            let content1 = b"content for package A";
            let content2 = b"content for package B";

            let server = MockServer::start();

            let mut hasher = Sha256::new();
            hasher.update(content1);
            let sha256_1 = hex::encode(hasher.finalize());

            let mut hasher = Sha256::new();
            hasher.update(content2);
            let sha256_2 = hex::encode(hasher.finalize());

            let mock1 = server.mock(|when, then| {
                when.method(GET).path("/pkg-a.tar.gz");
                then.status(200).body(content1);
            });

            let mock2 = server.mock(|when, then| {
                when.method(GET).path("/pkg-b.tar.gz");
                then.status(200).body(content2);
            });

            let config = create_test_config();
            let mut manager = SourceManager::new(config).unwrap();
            manager.sources = HashMap::from([
                (
                    "pkg-a".to_string(),
                    SourceDefinition {
                        name: "pkg-a".to_string(),
                        version: "1.0".to_string(),
                        url: format!("{}/pkg-a.tar.gz", server.base_url()),
                        sha256: sha256_1,
                        archive: "same-name.tar.gz".to_string(), // Same archive name!
                    },
                ),
                (
                    "pkg-b".to_string(),
                    SourceDefinition {
                        name: "pkg-b".to_string(),
                        version: "1.0".to_string(),
                        url: format!("{}/pkg-b.tar.gz", server.base_url()),
                        sha256: sha256_2,
                        archive: "same-name.tar.gz".to_string(), // Same archive name!
                    },
                ),
            ]);

            // Fetch both - should not collide in cache due to cache_key using full SHA-256
            let results = manager.fetch_all(false);

            assert_eq!(results.len(), 2);
            for result in results {
                assert!(result.is_ok(), "Fetch failed: {:?}", result);
            }

            mock1.assert();
            mock2.assert();

            // Both files should exist in cache with different names
            let cache_dir = manager.cache_dir();
            let cache_files: Vec<_> = fs::read_dir(cache_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect();
            assert_eq!(
                cache_files.len(),
                2,
                "Both packages should be cached separately"
            );
        }

        #[test]
        fn test_list_sources_deterministic() {
            let content = b"test";
            let (server, url, sha256) = setup_test_server(content);

            let config = create_test_config();
            let mut manager = SourceManager::new(config).unwrap();
            manager.sources = HashMap::from([
                (
                    "zlib".to_string(),
                    SourceDefinition {
                        name: "zlib".to_string(),
                        version: "1.3".to_string(),
                        url: url.clone(),
                        sha256: sha256.clone(),
                        archive: "zlib.tar.gz".to_string(),
                    },
                ),
                (
                    "openssl".to_string(),
                    SourceDefinition {
                        name: "openssl".to_string(),
                        version: "3.0".to_string(),
                        url: url.clone(),
                        sha256: sha256.clone(),
                        archive: "openssl.tar.gz".to_string(),
                    },
                ),
                (
                    "binutils".to_string(),
                    SourceDefinition {
                        name: "binutils".to_string(),
                        version: "2.40".to_string(),
                        url: url.clone(),
                        sha256: sha256.clone(),
                        archive: "binutils.tar.gz".to_string(),
                    },
                ),
            ]);

            let sources = manager.list_sources();
            assert_eq!(sources.len(), 3);
            // Should be sorted by name
            assert_eq!(sources[0].name, "binutils");
            assert_eq!(sources[1].name, "openssl");
            assert_eq!(sources[2].name, "zlib");
        }
    }
}
