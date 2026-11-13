use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use tar::Archive;
use xz2::read::XzDecoder;
use zstd::stream::read::Decoder as ZstdDecoder;

use crate::config::Config;
use crate::error::{ExtractError, ExtractResult};
use crate::source::SourceDefinition;

/// Result of an extract operation.
#[derive(Debug, Clone)]
pub struct ExtractResultInfo {
    /// Path to the extracted source directory.
    pub path: PathBuf,
    /// Whether the extraction was served from cache (already extracted).
    pub from_cache: bool,
    /// The source definition that was extracted.
    pub definition: SourceDefinition,
}

/// Manages extracting source archives.
pub struct ExtractManager {
    config: Config,
    sources: HashMap<String, SourceDefinition>,
}

impl ExtractManager {
    /// Creates a new extract manager with the given configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            sources: HashMap::new(),
        }
    }

    /// Loads source definitions from a TOML file.
    pub fn load_sources<P: AsRef<Path>>(&mut self, path: P) -> ExtractResult<()> {
        let content = fs::read_to_string(&path).map_err(|e| {
            ExtractError::ExtractionFailed(format!(
                "failed to read sources file '{}': {}",
                path.as_ref().display(),
                e
            ))
        })?;

        let sources: HashMap<String, SourceDefinition> = toml::from_str(&content).map_err(|e| {
            ExtractError::ExtractionFailed(format!("failed to parse sources TOML: {}", e))
        })?;

        // Validate that source key matches definition name
        for (name, def) in &sources {
            if name != &def.name {
                return Err(ExtractError::ExtractionFailed(format!(
                    "source key '{}' does not match definition name '{}'",
                    name, def.name
                )));
            }
        }

        self.sources = sources;
        Ok(())
    }

    /// Returns a reference to the loaded source definitions.
    pub fn sources(&self) -> &HashMap<String, SourceDefinition> {
        &self.sources
    }

    /// Computes the expected extraction path for a source definition.
    /// Uses package name and version for predictable naming.
    /// Sources are extracted to the persistent source directory (not the ephemeral build directory)
    /// so they survive build failures and can be reused across builds.
    pub fn extract_path(&self, def: &SourceDefinition) -> PathBuf {
        self.source_dir()
            .join(format!("{}-{}", def.name, def.version))
    }

    /// Returns the source directory path.
    fn source_dir(&self) -> &Path {
        &self.config.directories.source
    }

    /// Returns the build directory path (where sources are extracted).
    fn build_dir(&self) -> &Path {
        &self.config.directories.build
    }

    /// Computes the cache marker path for a source definition.
    fn cache_marker_path(&self, def: &SourceDefinition) -> PathBuf {
        self.extract_path(def).join(".extracted")
    }

    /// Computes a unique staging directory path.
    /// Uses a random suffix to avoid collisions.
    fn staging_path(&self, def: &SourceDefinition) -> PathBuf {
        let mut rng_state = Self::rng_seed();
        let random_suffix: u64 = loop {
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let suffix = rng_state >> 33;
            if suffix != 0 {
                break suffix;
            }
        };
        self.build_dir().join(format!(
            ".extract-{}-{}-{:x}",
            def.name, def.version, random_suffix
        ))
    }

    /// Simple deterministic RNG seed based on time and PID.
    fn rng_seed() -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let pid = process::id() as u64;
        now ^ (pid << 32)
    }

    /// Creates a unique staging directory atomically.
    /// Retries with different random suffixes on collision.
    fn create_staging_dir(&self, def: &SourceDefinition) -> ExtractResult<PathBuf> {
        let build_dir = self.build_dir();
        fs::create_dir_all(build_dir).map_err(|e| {
            ExtractError::ExtractionFailed(format!("failed to create build directory: {}", e))
        })?;

        const MAX_ATTEMPTS: u32 = 100;
        for _attempt in 1..=MAX_ATTEMPTS {
            let staging_path = self.staging_path(def);
            match fs::DirBuilder::new().mode(0o700).create(&staging_path) {
                Ok(()) => return Ok(staging_path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Collision - try again with new random suffix
                    continue;
                }
                Err(e) => {
                    return Err(ExtractError::ExtractionFailed(format!(
                        "failed to create staging directory: {}",
                        e
                    )));
                }
            }
        }

        Err(ExtractError::StagingCreationFailed {
            attempts: MAX_ATTEMPTS,
        })
    }

    /// Computes the source archive path for a source definition.
    fn archive_path(&self, def: &SourceDefinition) -> PathBuf {
        self.source_dir().join(def.cache_key())
    }

    /// Detects the archive format from the source definition's archive filename.
    fn detect_format(&self, def: &SourceDefinition) -> ExtractResult<ArchiveFormat> {
        let filename = &def.archive;

        if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
            Ok(ArchiveFormat::Gz)
        } else if filename.ends_with(".tar.xz") || filename.ends_with(".txz") {
            Ok(ArchiveFormat::Xz)
        } else if filename.ends_with(".tar.zst") || filename.ends_with(".tzst") {
            Ok(ArchiveFormat::Zst)
        } else {
            Err(ExtractError::UnsupportedArchiveType(format!(
                "unsupported archive format: {}",
                filename
            )))
        }
    }

    /// Validates a path within an archive for security.
    fn validate_archive_path(&self, path: &Path, dest_root: &Path) -> ExtractResult<()> {
        // Reject absolute paths
        if path.is_absolute() {
            return Err(ExtractError::AbsolutePath(path.display().to_string()));
        }

        // Reject path traversal
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(ExtractError::PathTraversal(path.display().to_string()));
        }

        // Ensure the resolved path stays within destination
        let resolved = dest_root.join(path);
        let dest_canonical = dest_root.canonicalize().map_err(|e| {
            ExtractError::ExtractionFailed(format!("failed to canonicalize destination: {}", e))
        })?;

        if !resolved.starts_with(&dest_canonical) {
            return Err(ExtractError::PathTraversal(path.display().to_string()));
        }

        Ok(())
    }

    /// Validates a symlink target for safety, considering already-created symlinks.
    fn validate_symlink(
        &self,
        target: &Path,
        dest_root: &Path,
        created_symlinks: &HashSet<PathBuf>,
    ) -> ExtractResult<()> {
        // Reject absolute symlink targets
        if target.is_absolute() {
            return Err(ExtractError::UnsafeSymlink(target.display().to_string()));
        }

        // Resolve the target against the destination root, following any created symlinks
        let resolved_target = self.resolve_path(dest_root, target, created_symlinks)?;

        // Ensure the resolved target stays within destination
        let dest_canonical = dest_root.canonicalize().map_err(|e| {
            ExtractError::ExtractionFailed(format!("failed to canonicalize destination: {}", e))
        })?;

        if !resolved_target.starts_with(&dest_canonical) {
            return Err(ExtractError::SymlinkEscape(target.display().to_string()));
        }

        Ok(())
    }

    /// Resolves a path relative to dest_root, following any created symlinks.
    fn resolve_path(
        &self,
        dest_root: &Path,
        rel_path: &Path,
        created_symlinks: &HashSet<PathBuf>,
    ) -> ExtractResult<PathBuf> {
        let mut resolved = dest_root.to_path_buf();
        for component in rel_path.components() {
            match component {
                std::path::Component::Normal(name) => {
                    resolved.push(name);
                }
                std::path::Component::ParentDir => {
                    resolved.pop();
                }
                std::path::Component::CurDir => {}
                _ => {
                    return Err(ExtractError::PathTraversal(rel_path.display().to_string()));
                }
            }

            // Check if this component is a symlink we created
            if created_symlinks.contains(&resolved) {
                // Read the symlink target and continue resolving from there
                let target = fs::read_link(&resolved).map_err(|e| {
                    ExtractError::ExtractionFailed(format!("failed to read symlink: {}", e))
                })?;
                if target.is_absolute() {
                    resolved = target;
                } else {
                    resolved = resolved.parent().unwrap().join(target);
                }
            }
        }
        Ok(resolved)
    }

    /// Validates a hard link target for safety.
    fn validate_hardlink(&self, target: &Path, dest_root: &Path) -> ExtractResult<()> {
        // Reject absolute hardlink targets
        if target.is_absolute() {
            return Err(ExtractError::UnsafeHardlink(target.display().to_string()));
        }

        // Reject path traversal in hardlink target
        if target
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(ExtractError::UnsafeHardlink(target.display().to_string()));
        }

        // Hard links must also stay within destination
        let resolved = dest_root.join(target);
        let dest_canonical = dest_root.canonicalize().map_err(|e| {
            ExtractError::ExtractionFailed(format!("failed to canonicalize destination: {}", e))
        })?;

        if !resolved.starts_with(&dest_canonical) {
            return Err(ExtractError::UnsafeHardlink(target.display().to_string()));
        }

        Ok(())
    }

    /// Checks if an extraction already exists and is valid by verifying the cache marker.
    fn check_extracted(&self, def: &SourceDefinition) -> bool {
        let marker_path = self.cache_marker_path(def);
        if !marker_path.exists() {
            return false;
        }

        // Read and validate the cache marker
        match fs::read_to_string(&marker_path) {
            Ok(content) => {
                // Format: name\nversion\nsha256\n
                let mut lines = content.lines();
                let name = lines.next().unwrap_or("");
                let version = lines.next().unwrap_or("");
                let sha256 = lines.next().unwrap_or("");

                name == def.name && version == def.version && sha256 == def.sha256
            }
            Err(_) => false,
        }
    }

    /// Writes a cache marker after successful extraction.
    fn write_cache_marker(&self, def: &SourceDefinition, base_path: &Path) -> ExtractResult<()> {
        let marker_path = base_path.join(".extracted");
        let content = format!("{}\n{}\n{}\n", def.name, def.version, def.sha256);
        fs::write(&marker_path, content).map_err(|e| {
            ExtractError::ExtractionFailed(format!("failed to write cache marker: {}", e))
        })
    }

    /// Safely removes a staging directory, ignoring "not found" errors.
    fn cleanup_staging(&self, staging_path: &Path) {
        let _ = fs::remove_dir_all(staging_path);
    }

    /// Removes an existing extraction directory.
    fn remove_extraction(&self, extract_path: &Path) -> ExtractResult<()> {
        if extract_path.exists() {
            fs::remove_dir_all(extract_path).map_err(|e| {
                ExtractError::ExtractionFailed(format!(
                    "failed to remove existing extraction: {}",
                    e
                ))
            })?;
        }
        Ok(())
    }

    /// Extracts a tar archive with security validation.
    fn extract_tar<R: Read>(
        &self,
        archive: &mut Archive<R>,
        dest_root: &Path,
    ) -> ExtractResult<()> {
        // Track created symlinks to prevent them from redirecting later extraction
        let mut created_symlinks: HashSet<PathBuf> = HashSet::new();

        // Single pass: validate and extract each entry
        let entries = archive.entries().map_err(|e| {
            ExtractError::MalformedArchive(format!("failed to read archive entries: {}", e))
        })?;
        for entry in entries {
            let mut entry = entry.map_err(|e| {
                ExtractError::MalformedArchive(format!("failed to read archive entry: {}", e))
            })?;
            let path = entry.path()?.to_path_buf();

            // Validate the path
            self.validate_archive_path(&path, dest_root)?;

            // Check for symlinks and validate their targets
            if entry.header().entry_type().is_symlink() {
                let target = entry
                    .link_name()?
                    .ok_or_else(|| ExtractError::UnsafeSymlink("missing link target".to_string()))?
                    .to_path_buf();
                self.validate_symlink(&target, dest_root, &created_symlinks)?;

                // Track this symlink
                created_symlinks.insert(path.clone());
            }

            // Check for hard links - any entry with a non-empty link name is a hard link
            if let Ok(Some(target)) = entry.link_name() {
                let target_path = target.to_path_buf();
                if !target_path.as_os_str().is_empty() {
                    self.validate_hardlink(&target_path, dest_root)?;
                }
            }

            // Reject unsupported special files
            let entry_type = entry.header().entry_type();
            if entry_type.is_block_special()
                || entry_type.is_character_special()
                || entry_type.is_fifo()
            {
                return Err(ExtractError::UnsupportedSpecialFile(format!(
                    "unsupported special file type: {:?}",
                    entry_type
                )));
            }

            let dest_path = dest_root.join(&path);

            // Ensure parent directory exists (using resolved path to avoid symlink escape)
            if let Some(parent) = dest_path.parent() {
                // Check if any parent component is a created symlink
                let mut parent_path = dest_root.to_path_buf();
                for component in parent
                    .strip_prefix(dest_root)
                    .unwrap_or(parent)
                    .components()
                {
                    parent_path.push(component);
                    if created_symlinks.contains(&parent_path) {
                        return Err(ExtractError::SymlinkEscape(format!(
                            "parent directory is a symlink: {}",
                            parent_path.display()
                        )));
                    }
                }
                fs::create_dir_all(parent)?;
            }

            // Handle different entry types
            let entry_type = entry.header().entry_type();

            if entry_type.is_dir() {
                fs::create_dir_all(&dest_path)?;
            } else if entry_type.is_file() {
                // Check if this is a hard link (has a link name)
                if let Ok(Some(target)) = entry.link_name() {
                    let target_path = target.to_path_buf();
                    if !target_path.as_os_str().is_empty() {
                        // This is a hard link - create the link
                        let full_target = dest_root.join(&target_path);
                        fs::hard_link(&full_target, &dest_path)?;
                    } else {
                        // Regular file - extract content
                        entry.unpack(&dest_path)?;
                    }
                } else {
                    // Regular file - extract content
                    entry.unpack(&dest_path)?;
                }
            } else if entry_type.is_symlink() {
                // Symlink already validated
                let target = entry
                    .link_name()?
                    .ok_or_else(|| ExtractError::UnsafeSymlink("missing link target".to_string()))?
                    .to_path_buf();

                // Create the symlink (do not apply permissions to symlink target)
                fs::create_dir_all(dest_path.parent().unwrap())?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &dest_path)?;
                #[cfg(not(unix))]
                {
                    std::os::unix::fs::symlink(&target, &dest_path)?;
                }

                // Track this symlink
                created_symlinks.insert(path.clone());
            } else if let Ok(Some(target)) = entry.link_name() {
                // Hard link (entry type Link) - create the link
                let target_path = target.to_path_buf();
                if !target_path.as_os_str().is_empty() {
                    let full_target = dest_root.join(&target_path);
                    fs::hard_link(&full_target, &dest_path)?;
                }
            } else {
                // Should have been caught in validation above
                return Err(ExtractError::UnsupportedSpecialFile(format!(
                    "unsupported file type: {:?}",
                    entry_type
                )));
            }

            // Preserve reasonable permissions for regular files and directories
            // Use symlink_metadata to avoid following symlinks
            let metadata = fs::symlink_metadata(&dest_path)?;
            let mut perms = metadata.permissions();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // Only apply permissions to regular files and directories, not symlinks
                if metadata.file_type().is_file() || metadata.file_type().is_dir() {
                    // Get the mode from the tar entry
                    let mode = entry.header().mode()?;
                    // Apply reasonable permissions: preserve owner bits, ensure group/other have at least read
                    let new_mode = (mode & 0o777) | 0o444;
                    perms.set_mode(new_mode);
                    fs::set_permissions(&dest_path, perms)?;
                }
                // For symlinks, we don't modify permissions (they don't have meaningful permissions)
            }
        }

        Ok(())
    }

    /// Extracts a source archive to a staging directory.
    fn extract_to_staging(&self, def: &SourceDefinition, staging_path: &Path) -> ExtractResult<()> {
        let archive_path = self.archive_path(def);

        if !archive_path.exists() {
            return Err(ExtractError::SourceNotFound(format!(
                "source archive not found: {}",
                archive_path.display()
            )));
        }

        let format = self.detect_format(def)?;
        let file = fs::File::open(&archive_path)?;

        // Create a temporary extraction directory inside staging
        let tmp_dir = staging_path.join("tmp");
        fs::create_dir_all(&tmp_dir).map_err(|e| {
            ExtractError::ExtractionFailed(format!("failed to create tmp directory: {}", e))
        })?;

        // Extract to temporary directory
        let extract_result = match format {
            ArchiveFormat::Gz => {
                let decoder = GzDecoder::new(file);
                let mut archive = Archive::new(decoder);
                self.extract_tar(&mut archive, &tmp_dir)
            }
            ArchiveFormat::Xz => {
                let decoder = XzDecoder::new(file);
                let mut archive = Archive::new(decoder);
                self.extract_tar(&mut archive, &tmp_dir)
            }
            ArchiveFormat::Zst => {
                let decoder = ZstdDecoder::new(file)?;
                let mut archive = Archive::new(decoder);
                self.extract_tar(&mut archive, &tmp_dir)
            }
        };

        // Clean up tmp_dir on extraction failure
        if let Err(e) = extract_result {
            let _ = fs::remove_dir_all(&tmp_dir);
            return Err(e);
        }

        // Post-process: strip common top-level directory if present
        self.strip_top_level_directory(&tmp_dir, staging_path)?;

        // Clean up temporary directory
        let _ = fs::remove_dir_all(&tmp_dir);

        Ok(())
    }

    /// Strips a common top-level directory from extracted contents.
    /// Many source tarballs contain a single top-level directory (e.g., "zlib-1.4/")
    /// which would cause double-nesting when extracted to a directory named after the package.
    /// This function detects and removes that extra level.
    fn strip_top_level_directory(&self, tmp_dir: &Path, staging_path: &Path) -> ExtractResult<()> {
        let entries: Vec<_> = fs::read_dir(tmp_dir)
            .map_err(|e| {
                ExtractError::ExtractionFailed(format!("failed to read tmp directory: {}", e))
            })?
            .filter_map(|e| e.ok())
            .collect();

        // If there's exactly one entry and it's a directory, move its contents up
        if entries.len() == 1 {
            let entry = &entries[0];
            let file_type = entry.file_type().map_err(|e| {
                ExtractError::ExtractionFailed(format!("failed to get file type: {}", e))
            })?;

            if file_type.is_dir() {
                let top_level = entry.path();
                // Move all contents from top_level to staging_path
                for entry in fs::read_dir(&top_level).map_err(|e| {
                    ExtractError::ExtractionFailed(format!(
                        "failed to read top-level directory: {}",
                        e
                    ))
                })? {
                    let entry = entry.map_err(|e| {
                        ExtractError::ExtractionFailed(format!(
                            "failed to read directory entry: {}",
                            e
                        ))
                    })?;
                    let file_name = entry.file_name();
                    // Validate filename to prevent path traversal (defense in depth)
                    let file_name_str = file_name.to_string_lossy();
                    if file_name_str == "."
                        || file_name_str == ".."
                        || file_name_str.contains('/')
                        || file_name_str.contains('\\')
                    {
                        return Err(ExtractError::ExtractionFailed(format!(
                            "invalid filename in archive: {}",
                            file_name_str
                        )));
                    }
                    let dest = staging_path.join(&file_name);
                    // Additional safety: verify dest is within staging_path after canonicalization
                    let dest_canon = fs::canonicalize(&dest).unwrap_or(dest.clone());
                    let staging_canon =
                        fs::canonicalize(staging_path).unwrap_or(staging_path.to_path_buf());
                    if !dest_canon.starts_with(&staging_canon) {
                        return Err(ExtractError::ExtractionFailed(format!(
                            "path traversal attempt: {}",
                            file_name_str
                        )));
                    }
                    fs::rename(entry.path(), &dest).map_err(|e| {
                        ExtractError::ExtractionFailed(format!(
                            "failed to move extracted file '{}': {}",
                            file_name_str, e
                        ))
                    })?;
                }
                return Ok(());
            }
        }

        // Otherwise, move all contents from tmp_dir to staging_path
        for entry in fs::read_dir(tmp_dir).map_err(|e| {
            ExtractError::ExtractionFailed(format!("failed to read tmp directory: {}", e))
        })? {
            let entry = entry.map_err(|e| {
                ExtractError::ExtractionFailed(format!("failed to read directory entry: {}", e))
            })?;
            let file_name = entry.file_name();
            // Validate filename to prevent path traversal (defense in depth)
            let file_name_str = file_name.to_string_lossy();
            if file_name_str == "."
                || file_name_str == ".."
                || file_name_str.contains('/')
                || file_name_str.contains('\\')
            {
                return Err(ExtractError::ExtractionFailed(format!(
                    "invalid filename in archive: {}",
                    file_name_str
                )));
            }
            let dest = staging_path.join(&file_name);
            // Additional safety: verify dest is within staging_path after canonicalization
            let dest_canon = fs::canonicalize(&dest).unwrap_or(dest.clone());
            let staging_canon =
                fs::canonicalize(staging_path).unwrap_or(staging_path.to_path_buf());
            if !dest_canon.starts_with(&staging_canon) {
                return Err(ExtractError::ExtractionFailed(format!(
                    "path traversal attempt: {}",
                    file_name_str
                )));
            }
            fs::rename(entry.path(), &dest).map_err(|e| {
                ExtractError::ExtractionFailed(format!(
                    "failed to move extracted file '{}': {}",
                    file_name_str, e
                ))
            })?;
        }
        Ok(())
    }

    /// Extracts a single source by name.
    pub fn extract(&mut self, name: &str, force: bool) -> ExtractResult<ExtractResultInfo> {
        let def = self
            .sources
            .get(name)
            .ok_or_else(|| ExtractError::ExtractionFailed(format!("source '{}' not found", name)))?
            .clone();

        // Validate the source definition before use
        def.validate()
            .map_err(|e| ExtractError::ExtractionFailed(e.to_string()))?;

        let extract_path = self.extract_path(&def);

        // Check cache first (unless forced)
        let cache_valid = !force && self.check_extracted(&def);
        let mut effective_force = force;

        if cache_valid {
            return Ok(ExtractResultInfo {
                path: extract_path,
                from_cache: true,
                definition: def,
            });
        } else if extract_path.exists() {
            // Cache invalid or missing, but extraction directory exists.
            // Treat as cache miss and force replacement.
            effective_force = true;
        }

        // Ensure source directory exists (where persistent extractions live)
        fs::create_dir_all(self.source_dir()).map_err(|e| {
            ExtractError::ExtractionFailed(format!("failed to create source directory: {}", e))
        })?;

        // Create unique staging directory
        let staging_path = self.create_staging_dir(&def)?;

        // Extract to staging
        let extract_result = self.extract_to_staging(&def, &staging_path);

        if let Err(e) = extract_result {
            // Clean up staging on failure
            self.cleanup_staging(&staging_path);
            return Err(e);
        }

        // Write cache marker in staging before publishing
        self.write_cache_marker(&def, &staging_path)?;

        // Atomic rename from staging to final location
        // For --force (or implied force from invalid cache), we remove the old extraction first
        if effective_force && extract_path.exists() {
            self.remove_extraction(&extract_path)?;
        } else if extract_path.exists() {
            // Should not happen due to check_extracted, but handle race
            self.cleanup_staging(&staging_path);
            return Err(ExtractError::DestinationExists(format!(
                "extraction already exists at {} (use --force to replace)",
                extract_path.display()
            )));
        }

        // Atomic rename - this is atomic on the same filesystem
        if let Err(e) = fs::rename(&staging_path, &extract_path) {
            // Clean up staging on rename failure
            self.cleanup_staging(&staging_path);
            return Err(ExtractError::ExtractionFailed(format!(
                "failed to move extraction to destination: {}",
                e
            )));
        }

        Ok(ExtractResultInfo {
            path: extract_path,
            from_cache: false,
            definition: def,
        })
    }

    /// Extracts all loaded sources.
    pub fn extract_all(&mut self, force: bool) -> Vec<ExtractResult<ExtractResultInfo>> {
        let names: Vec<String> = self.sources.keys().cloned().collect();
        names
            .into_iter()
            .map(|name| self.extract(&name, force))
            .collect()
    }

    /// Lists all available sources.
    pub fn list_sources(&self) -> Vec<&SourceDefinition> {
        let mut sources: Vec<&SourceDefinition> = self.sources.values().collect();
        sources.sort_by(|a, b| a.name.cmp(&b.name));
        sources
    }
}

/// Supported archive formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveFormat {
    Gz,
    Xz,
    Zst,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::fs;
    use std::io::Write;
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

    fn create_tar_gz(content: &[(&str, &[u8])]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use tar::Builder;

        let mut compressed = Vec::new();
        {
            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);
            for (path, data) in content {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append(&header, *data).unwrap();
            }
            drop(tar);
            gz.finish().unwrap();
        }
        compressed
    }

    fn create_tar_xz(content: &[(&str, &[u8])]) -> Vec<u8> {
        use tar::Builder;
        use xz2::write::XzEncoder;

        let mut compressed = Vec::new();
        {
            let mut xz = XzEncoder::new(&mut compressed, 6);
            let mut tar = Builder::new(&mut xz);
            for (path, data) in content {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append(&header, *data).unwrap();
            }
            drop(tar);
            xz.finish().unwrap();
        }
        compressed
    }

    fn create_tar_zst(content: &[(&str, &[u8])]) -> Vec<u8> {
        use tar::Builder;
        use zstd::stream::write::Encoder;

        let mut compressed = Vec::new();
        {
            let mut zst = Encoder::new(&mut compressed, 3).unwrap();
            let mut tar = Builder::new(&mut zst);
            for (path, data) in content {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append(&header, *data).unwrap();
            }
            drop(tar);
            zst.finish().unwrap();
        }
        compressed
    }

    /// Creates a raw tar.gz archive with manually constructed headers.
    /// This bypasses the tar crate's path validation, allowing creation of
    /// archives with absolute paths, path traversal, etc. for security testing.
    fn create_raw_tar_gz(entries: &[RawTarEntry]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let mut compressed = Vec::new();
        {
            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            for entry in entries {
                write_raw_tar_entry(&mut gz, entry).unwrap();
            }
            // Write two zero blocks (end of archive marker)
            gz.write_all(&[0u8; 1024]).unwrap();
            gz.finish().unwrap();
        }
        compressed
    }

    /// A raw tar entry for testing with arbitrary paths.
    struct RawTarEntry {
        path: String,
        data: Vec<u8>,
        mode: u32,
        entry_type: u8, // 0 = regular, 2 = symlink, 5 = directory
        link_name: Option<String>,
    }

    /// Writes a single raw tar entry (GNU tar format, 512-byte blocks).
    fn write_raw_tar_entry<W: Write>(writer: &mut W, entry: &RawTarEntry) -> std::io::Result<()> {
        // Tar header is 512 bytes
        let mut header = [0u8; 512];

        // Name (100 bytes)
        let name_bytes = entry.path.as_bytes();
        header[..name_bytes.len().min(100)]
            .copy_from_slice(&name_bytes[..name_bytes.len().min(100)]);

        // Mode (8 bytes, octal)
        let mode_str = format!("{:07o}\0", entry.mode);
        header[100..108].copy_from_slice(mode_str.as_bytes());

        // UID (8 bytes, octal) - 0
        header[108..116].copy_from_slice(b"0000000\0");

        // GID (8 bytes, octal) - 0
        header[116..124].copy_from_slice(b"0000000\0");

        // Size (12 bytes, octal)
        let size_str = format!("{:011o}\0", entry.data.len());
        header[124..136].copy_from_slice(size_str.as_bytes());

        // Mtime (12 bytes, octal) - 0
        header[136..148].copy_from_slice(b"00000000000\0");

        // Checksum (8 bytes) - fill with spaces for calculation
        header[148..156].copy_from_slice(b"        ");

        // Typeflag (1 byte)
        header[156] = entry.entry_type;

        // Linkname (100 bytes)
        if let Some(ref link) = entry.link_name {
            let link_bytes = link.as_bytes();
            header[157..157 + link_bytes.len().min(100)]
                .copy_from_slice(&link_bytes[..link_bytes.len().min(100)]);
        }

        // Magic (6 bytes) - "ustar\0"
        header[257..263].copy_from_slice(b"ustar\0");

        // Version (2 bytes) - "00"
        header[263..265].copy_from_slice(b"00");

        // Uname (32 bytes) - empty
        // Gname (32 bytes) - empty

        // Calculate checksum: sum of all 512 bytes with checksum field as spaces
        let mut checksum: u32 = 0;
        for &b in &header {
            checksum += b as u32;
        }

        // Write checksum as octal (6 digits + null + space)
        let checksum_str = format!("{:06o}\0 ", checksum);
        header[148..148 + checksum_str.len()].copy_from_slice(checksum_str.as_bytes());

        // Write header
        writer.write_all(&header)?;

        // Write data padded to 512 bytes
        if !entry.data.is_empty() {
            writer.write_all(&entry.data)?;
            let padding = (512 - (entry.data.len() % 512)) % 512;
            if padding > 0 {
                writer.write_all(&vec![0u8; padding])?;
            }
        }

        Ok(())
    }

    fn setup_source_archive(config: &Config, def: &SourceDefinition, data: &[u8]) {
        let source_dir = config.directories.source.clone();
        fs::create_dir_all(&source_dir).unwrap();
        let archive_path = source_dir.join(def.cache_key());
        fs::write(&archive_path, data).unwrap();
    }

    #[test]
    fn test_extract_tar_gz() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        let content = vec![
            ("file1.txt", b"hello world" as &[u8]),
            ("dir/file2.txt", b"nested file" as &[u8]),
        ];
        let archive_data = create_tar_gz(&content);
        setup_source_archive(&config, &def, &archive_data);

        let result = manager.extract("test-pkg", false).unwrap();

        assert!(!result.from_cache);
        assert!(result.path.exists());
        assert!(result.path.join("file1.txt").exists());
        assert!(result.path.join("dir/file2.txt").exists());

        // Verify content
        assert_eq!(
            fs::read(result.path.join("file1.txt")).unwrap(),
            b"hello world"
        );
        assert_eq!(
            fs::read(result.path.join("dir/file2.txt")).unwrap(),
            b"nested file"
        );
    }

    #[test]
    fn test_extract_tar_xz() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.xz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.xz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        let content = vec![("file1.txt", b"hello xz" as &[u8])];
        let archive_data = create_tar_xz(&content);
        setup_source_archive(&config, &def, &archive_data);

        let result = manager.extract("test-pkg", false).unwrap();

        assert!(!result.from_cache);
        assert!(result.path.join("file1.txt").exists());
        assert_eq!(
            fs::read(result.path.join("file1.txt")).unwrap(),
            b"hello xz"
        );
    }

    #[test]
    fn test_extract_tar_zst() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.zst".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.zst".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        let content = vec![("file1.txt", b"hello zst" as &[u8])];
        let archive_data = create_tar_zst(&content);
        setup_source_archive(&config, &def, &archive_data);

        let result = manager.extract("test-pkg", false).unwrap();

        assert!(!result.from_cache);
        assert!(result.path.join("file1.txt").exists());
        assert_eq!(
            fs::read(result.path.join("file1.txt")).unwrap(),
            b"hello zst"
        );
    }

    #[test]
    fn test_extract_cache_hit() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        let content = vec![("file.txt", b"cached content" as &[u8])];
        let archive_data = create_tar_gz(&content);
        setup_source_archive(&config, &def, &archive_data);

        // First extraction
        let result1 = manager.extract("test-pkg", false).unwrap();
        assert!(!result1.from_cache);

        // Second extraction should hit cache
        let result2 = manager.extract("test-pkg", false).unwrap();
        assert!(result2.from_cache);
        assert_eq!(result2.path, result1.path);
    }

    #[test]
    fn test_extract_force_replaces() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        let content1 = vec![("file.txt", b"original" as &[u8])];
        let archive_data1 = create_tar_gz(&content1);
        setup_source_archive(&config, &def, &archive_data1);

        let result1 = manager.extract("test-pkg", false).unwrap();
        assert!(!result1.from_cache);
        assert_eq!(
            fs::read(result1.path.join("file.txt")).unwrap(),
            b"original"
        );

        // Replace with new content (simulate different archive)
        let content2 = vec![("file.txt", b"replaced" as &[u8])];
        let archive_data2 = create_tar_gz(&content2);
        setup_source_archive(&config, &def, &archive_data2);

        // Force re-extraction
        let result2 = manager.extract("test-pkg", true).unwrap();
        assert!(!result2.from_cache);
        assert_eq!(
            fs::read(result2.path.join("file.txt")).unwrap(),
            b"replaced"
        );
    }

    #[test]
    fn test_extract_force_failed_preserves_original() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        let content1 = vec![("file.txt", b"original" as &[u8])];
        let archive_data1 = create_tar_gz(&content1);
        setup_source_archive(&config, &def, &archive_data1);

        let result1 = manager.extract("test-pkg", false).unwrap();
        assert!(!result1.from_cache);
        assert_eq!(
            fs::read(result1.path.join("file.txt")).unwrap(),
            b"original"
        );

        // Now create a corrupted archive that will fail extraction
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            header.set_path("file.txt").unwrap();
            header.set_size(100); // Claim 100 bytes but write nothing
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &b"short"[..]).unwrap();
            drop(tar);
            drop(gz);
        }
        compressed.truncate(compressed.len() / 2);
        setup_source_archive(&config, &def, &compressed);

        // Force re-extraction should fail
        let result2 = manager.extract("test-pkg", true);
        assert!(result2.is_err());

        // Original extraction should still exist and be intact
        assert!(result1.path.exists());
        assert_eq!(
            fs::read(result1.path.join("file.txt")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn test_extract_without_force_fails_on_existing() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        let content = vec![("file.txt", b"original" as &[u8])];
        let archive_data = create_tar_gz(&content);
        setup_source_archive(&config, &def, &archive_data);

        let result1 = manager.extract("test-pkg", false).unwrap();
        assert!(!result1.from_cache);

        // Second extraction without force should hit cache
        let result2 = manager.extract("test-pkg", false);
        assert!(result2.is_ok());
        let result2 = result2.unwrap();
        assert!(result2.from_cache);
        assert_eq!(result2.path, result1.path);
    }

    #[test]
    fn test_extract_rejects_absolute_path() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with absolute path (using raw tar to bypass tar crate validation)
        let entries = vec![RawTarEntry {
            path: "/etc/passwd".to_string(),
            data: vec![],
            mode: 0o644,
            entry_type: 0, // regular file
            link_name: None,
        }];
        let archive_data = create_raw_tar_gz(&entries);
        setup_source_archive(&config, &def, &archive_data);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::AbsolutePath(_) => {}
            e => panic!("Expected AbsolutePath, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_rejects_path_traversal() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with path traversal (using raw tar to bypass tar crate validation)
        let entries = vec![RawTarEntry {
            path: "../etc/passwd".to_string(),
            data: vec![],
            mode: 0o644,
            entry_type: 0, // regular file
            link_name: None,
        }];
        let archive_data = create_raw_tar_gz(&entries);
        setup_source_archive(&config, &def, &archive_data);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::PathTraversal(_) => {}
            e => panic!("Expected PathTraversal, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_rejects_unsafe_symlink() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with unsafe symlink (absolute target)
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            header.set_path("link.txt").unwrap();
            header.set_size(0);
            header.set_mode(0o777);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name("/etc/passwd").unwrap();
            header.set_cksum();
            tar.append(&header, &[][..]).unwrap();
            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::UnsafeSymlink(_) => {}
            e => panic!("Expected UnsafeSymlink, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_rejects_symlink_path_traversal() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with symlink containing path traversal
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            header.set_path("link.txt").unwrap();
            header.set_size(0);
            header.set_mode(0o777);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name("../etc/passwd").unwrap();
            header.set_cksum();
            tar.append(&header, &[][..]).unwrap();
            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::SymlinkEscape(_) => {}
            e => panic!("Expected SymlinkEscape, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_rejects_unsafe_hardlink() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with hard link to outside destination
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);
            // First file
            let mut header1 = tar::Header::new_gnu();
            header1.set_path("target.txt").unwrap();
            header1.set_size(4);
            header1.set_mode(0o644);
            header1.set_cksum();
            tar.append(&header1, &b"data"[..]).unwrap();

            // Hard link to it with path traversal
            let mut header2 = tar::Header::new_gnu();
            header2.set_path("link.txt").unwrap();
            header2.set_size(4);
            header2.set_mode(0o644);
            header2.set_entry_type(tar::EntryType::Link);
            header2.set_link_name("../target.txt").unwrap();
            header2.set_cksum();
            tar.append(&header2, &[][..]).unwrap();
            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::UnsafeHardlink(_) => {}
            e => panic!("Expected UnsafeHardlink, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_rejects_symlink_parent_escape() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with symlink as parent directory that escapes
        // Archive contains: link -> ../outside, then link/escape.txt
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);
            // Create symlink "link" pointing outside
            let mut header1 = tar::Header::new_gnu();
            header1.set_path("link").unwrap();
            header1.set_size(0);
            header1.set_mode(0o777);
            header1.set_entry_type(tar::EntryType::Symlink);
            header1.set_link_name("../outside").unwrap();
            header1.set_cksum();
            tar.append(&header1, &[][..]).unwrap();

            // Create file "link/escape.txt" which should escape via symlink
            let mut header2 = tar::Header::new_gnu();
            header2.set_path("link/escape.txt").unwrap();
            header2.set_size(4);
            header2.set_mode(0o644);
            header2.set_cksum();
            tar.append(&header2, &b"escaped"[..]).unwrap();

            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::SymlinkEscape(_) => {}
            e => panic!("Expected SymlinkEscape, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_rejects_symlink_chain_escape() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with symlink chain: a -> b, b -> ../outside, a/escape.txt
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);
            // Create symlink "a" pointing to "b"
            let mut header1 = tar::Header::new_gnu();
            header1.set_path("a").unwrap();
            header1.set_size(0);
            header1.set_mode(0o777);
            header1.set_entry_type(tar::EntryType::Symlink);
            header1.set_link_name("b").unwrap();
            header1.set_cksum();
            tar.append(&header1, &[][..]).unwrap();

            // Create symlink "b" pointing outside
            let mut header2 = tar::Header::new_gnu();
            header2.set_path("b").unwrap();
            header2.set_size(0);
            header2.set_mode(0o777);
            header2.set_entry_type(tar::EntryType::Symlink);
            header2.set_link_name("../outside").unwrap();
            header2.set_cksum();
            tar.append(&header2, &[][..]).unwrap();

            // Create file "a/escape.txt" which should escape via symlink chain
            let mut header3 = tar::Header::new_gnu();
            header3.set_path("a/escape.txt").unwrap();
            header3.set_size(4);
            header3.set_mode(0o644);
            header3.set_cksum();
            tar.append(&header3, &b"escaped"[..]).unwrap();

            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::SymlinkEscape(_) => {}
            e => panic!("Expected SymlinkEscape, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_rejects_block_device() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with block device
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            header.set_path("device").unwrap();
            header.set_size(0);
            header.set_mode(0o600);
            header.set_entry_type(tar::EntryType::Block);
            header.set_cksum();
            tar.append(&header, &[][..]).unwrap();
            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::UnsupportedSpecialFile(_) => {}
            e => panic!("Expected UnsupportedSpecialFile, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_rejects_char_device() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with char device
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            header.set_path("device").unwrap();
            header.set_size(0);
            header.set_mode(0o600);
            header.set_entry_type(tar::EntryType::Char);
            header.set_cksum();
            tar.append(&header, &[][..]).unwrap();
            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::UnsupportedSpecialFile(_) => {}
            e => panic!("Expected UnsupportedSpecialFile, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_rejects_fifo() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with FIFO
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            header.set_path("fifo").unwrap();
            header.set_size(0);
            header.set_mode(0o600);
            header.set_entry_type(tar::EntryType::Fifo);
            header.set_cksum();
            tar.append(&header, &[][..]).unwrap();
            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::UnsupportedSpecialFile(_) => {}
            e => panic!("Expected UnsupportedSpecialFile, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_malformed_archive() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create malformed archive (truncated)
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            header.set_path("file.txt").unwrap();
            header.set_size(100); // Claim 100 bytes but write nothing
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &b"short"[..]).unwrap();
            // Don't finish properly - truncate
            drop(tar);
            drop(gz);
        }
        // Truncate the compressed data
        compressed.truncate(compressed.len() / 2);
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::MalformedArchive(_) => {}
            e => panic!("Expected MalformedArchive, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_unsupported_format() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.zip".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.zip".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        let content = vec![("file.txt", b"content" as &[u8])];
        let archive_data = create_tar_gz(&content);
        setup_source_archive(&config, &def, &archive_data);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::UnsupportedArchiveType(_) => {}
            e => panic!("Expected UnsupportedArchiveType, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_source_not_found() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Don't create the archive file

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());
        match result.unwrap_err() {
            ExtractError::SourceNotFound(_) => {}
            e => panic!("Expected SourceNotFound, got {:?}", e),
        }
    }

    #[test]
    fn test_extract_cleanup_on_failure() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with path traversal (will fail validation)
        // Using raw tar to bypass tar crate validation
        let entries = vec![RawTarEntry {
            path: "../etc/passwd".to_string(),
            data: vec![],
            mode: 0o644,
            entry_type: 0, // regular file
            link_name: None,
        }];
        let archive_data = create_raw_tar_gz(&entries);
        setup_source_archive(&config, &def, &archive_data);

        let result = manager.extract("test-pkg", false);
        assert!(result.is_err());

        // Check that no staging directories remain
        let build_dir = config.directories.build.clone();
        let entries: Vec<_> = fs::read_dir(&build_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".extract-"))
            .collect();
        assert!(
            entries.is_empty(),
            "Staging directories should be cleaned up"
        );
    }

    #[test]
    fn test_extract_unique_staging_dirs() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        let content = vec![("file.txt", b"content" as &[u8])];
        let archive_data = create_tar_gz(&content);
        setup_source_archive(&config, &def, &archive_data);

        // Extract multiple times concurrently (simulated)
        let result1 = manager.extract("test-pkg", false).unwrap();
        let result2 = manager.extract("test-pkg", true).unwrap(); // force second extraction

        // Both should succeed
        assert!(!result1.from_cache);
        assert!(!result2.from_cache);

        // No leftover staging directories
        let build_dir = config.directories.build.clone();
        let entries: Vec<_> = fs::read_dir(&build_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".extract-"))
            .collect();
        assert!(entries.is_empty(), "No staging directories should remain");
    }

    #[test]
    fn test_extract_cache_marker_missing() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        let content = vec![("file.txt", b"content" as &[u8])];
        let archive_data = create_tar_gz(&content);
        setup_source_archive(&config, &def, &archive_data);

        // First extraction
        let result1 = manager.extract("test-pkg", false).unwrap();
        assert!(!result1.from_cache);

        // Remove the cache marker
        let marker_path = result1.path.join(".extracted");
        fs::remove_file(&marker_path).unwrap();

        // Next extraction should not hit cache (marker missing)
        let result2 = manager.extract("test-pkg", false).unwrap();
        assert!(
            !result2.from_cache,
            "Should not hit cache when marker is missing"
        );
    }

    #[test]
    fn test_extract_cache_marker_invalid() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        let content = vec![("file.txt", b"content" as &[u8])];
        let archive_data = create_tar_gz(&content);
        setup_source_archive(&config, &def, &archive_data);

        // First extraction
        let result1 = manager.extract("test-pkg", false).unwrap();
        assert!(!result1.from_cache);

        // Corrupt the cache marker
        let marker_path = result1.path.join(".extracted");
        fs::write(&marker_path, "invalid content").unwrap();

        // Next extraction should not hit cache (marker invalid)
        let result2 = manager.extract("test-pkg", false).unwrap();
        assert!(
            !result2.from_cache,
            "Should not hit cache when marker is invalid"
        );
    }

    #[test]
    fn test_extract_cache_marker_checksum_mismatch() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        let content = vec![("file.txt", b"content" as &[u8])];
        let archive_data = create_tar_gz(&content);
        setup_source_archive(&config, &def, &archive_data);

        // First extraction
        let result1 = manager.extract("test-pkg", false).unwrap();
        assert!(!result1.from_cache);

        // Modify the cache marker with wrong checksum
        let marker_path = result1.path.join(".extracted");
        fs::write(&marker_path, "test-pkg\n1.0\nwrong_checksum\n").unwrap();

        // Next extraction should not hit cache (checksum mismatch)
        let result2 = manager.extract("test-pkg", false).unwrap();
        assert!(
            !result2.from_cache,
            "Should not hit cache when checksum mismatches"
        );
    }

    #[test]
    fn test_extract_rejects_symlink_permissions_modification() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with symlink and a regular file target
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);

            // Create target file
            let mut header1 = tar::Header::new_gnu();
            header1.set_path("target.txt").unwrap();
            header1.set_size(4);
            header1.set_mode(0o644);
            header1.set_cksum();
            tar.append(&header1, &b"data"[..]).unwrap();

            // Create symlink to target
            let mut header2 = tar::Header::new_gnu();
            header2.set_path("link.txt").unwrap();
            header2.set_size(0);
            header2.set_mode(0o777);
            header2.set_entry_type(tar::EntryType::Symlink);
            header2.set_link_name("target.txt").unwrap();
            header2.set_cksum();
            tar.append(&header2, &[][..]).unwrap();

            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false).unwrap();

        // Verify symlink target permissions were not modified
        let target_path = result.path.join("target.txt");
        let link_path = result.path.join("link.txt");

        assert!(target_path.exists());
        assert!(link_path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let target_meta = fs::metadata(&target_path).unwrap();
            let link_meta = fs::symlink_metadata(&link_path).unwrap();

            // Target should have 0o644 permissions (not modified by symlink processing)
            let target_mode = target_meta.mode() & 0o777;
            assert_eq!(
                target_mode, 0o644,
                "Target file permissions should not be modified by symlink"
            );

            // Symlink should exist
            assert!(link_meta.file_type().is_symlink());
        }
    }

    #[test]
    fn test_extract_hardlink_permissions() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with hardlink
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);

            // Create target file
            let mut header1 = tar::Header::new_gnu();
            header1.set_path("target.txt").unwrap();
            header1.set_size(4);
            header1.set_mode(0o644);
            header1.set_cksum();
            tar.append(&header1, &b"data"[..]).unwrap();

            // Create hard link to target
            let mut header2 = tar::Header::new_gnu();
            header2.set_path("link.txt").unwrap();
            header2.set_size(4);
            header2.set_mode(0o644);
            header2.set_entry_type(tar::EntryType::Link);
            header2.set_link_name("target.txt").unwrap();
            header2.set_cksum();
            tar.append(&header2, &[][..]).unwrap();

            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false).unwrap();

        // Verify both files exist and share inode (hardlink)
        let target_path = result.path.join("target.txt");
        let link_path = result.path.join("link.txt");

        assert!(target_path.exists());
        assert!(link_path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let target_meta = fs::metadata(&target_path).unwrap();
            let link_meta = fs::metadata(&link_path).unwrap();

            // Both should have same inode (hardlink)
            assert_eq!(
                target_meta.ino(),
                link_meta.ino(),
                "Hardlink should share inode"
            );

            // Permissions should be consistent
            let target_mode = target_meta.mode() & 0o777;
            let link_mode = link_meta.mode() & 0o777;
            assert_eq!(target_mode, link_mode, "Hardlink permissions should match");
        }
    }

    #[test]
    fn test_list_sources_deterministic() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        manager.sources = HashMap::from([
            (
                "zlib".to_string(),
                SourceDefinition {
                    name: "zlib".to_string(),
                    version: "1.3".to_string(),
                    url: "https://example.com/zlib.tar.gz".to_string(),
                    sha256: "0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string(),
                    archive: "zlib.tar.gz".to_string(),
                },
            ),
            (
                "openssl".to_string(),
                SourceDefinition {
                    name: "openssl".to_string(),
                    version: "3.0".to_string(),
                    url: "https://example.com/openssl.tar.gz".to_string(),
                    sha256: "0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string(),
                    archive: "openssl.tar.gz".to_string(),
                },
            ),
            (
                "binutils".to_string(),
                SourceDefinition {
                    name: "binutils".to_string(),
                    version: "2.40".to_string(),
                    url: "https://example.com/binutils.tar.gz".to_string(),
                    sha256: "0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string(),
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

    #[test]
    fn test_extract_preserves_permissions() {
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with executable file
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            header.set_path("script.sh").unwrap();
            header.set_size(10);
            header.set_mode(0o755); // Executable
            header.set_cksum();
            tar.append(&header, &b"#!/bin/sh"[..]).unwrap();
            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::metadata(result.path.join("script.sh")).unwrap();
            let mode = metadata.mode();
            // Owner should have execute permission
            assert!(mode & 0o100 != 0, "File should be executable");
        }
    }

    #[test]
    fn test_load_sources_succeeds_with_placeholder_checksums() {
        // Regression test: load_sources should succeed even when some sources have
        // placeholder/invalid checksums, because validation is deferred to extract().
        let temp_dir = tempdir().unwrap();
        let sources_path = temp_dir.path().join("sources.toml");
        let content = r#"
zlib = { name = "zlib", version = "1.3.1", url = "https://example.com/zlib.tar.gz", sha256 = "0000000000000000000000000000000000000000000000000000000000000000", archive = "zlib.tar.gz" }
openssl = { name = "openssl", version = "3.0.0", url = "https://example.com/openssl.tar.gz", sha256 = "REPLACE_WITH_ACTUAL_SHA256_CHECKSUM", archive = "openssl.tar.gz" }
gcc = { name = "gcc", version = "13.0.0", url = "https://example.com/gcc.tar.gz", sha256 = "not_a_valid_sha256", archive = "gcc.tar.gz" }
"#;
        fs::write(&sources_path, content).unwrap();

        let config = create_test_config();
        let mut manager = ExtractManager::new(config);
        // load_sources should succeed - it only validates name/key matching, not checksums
        manager.load_sources(&sources_path).unwrap();

        let sources = manager.sources();
        assert_eq!(sources.len(), 3);
        assert!(sources.contains_key("zlib"));
        assert!(sources.contains_key("openssl"));
        assert!(sources.contains_key("gcc"));
    }

    #[test]
    fn test_extract_fails_on_invalid_checksum() {
        // Regression test: extracting a source with an invalid checksum should fail
        let temp_dir = tempdir().unwrap();
        let sources_path = temp_dir.path().join("sources.toml");
        let content = r#"
zlib = { name = "zlib", version = "1.3.1", url = "https://example.com/zlib.tar.gz", sha256 = "0000000000000000000000000000000000000000000000000000000000000000", archive = "zlib.tar.gz" }
openssl = { name = "openssl", version = "3.0.0", url = "https://example.com/openssl.tar.gz", sha256 = "REPLACE_WITH_ACTUAL_SHA256_CHECKSUM", archive = "openssl.tar.gz" }
"#;
        fs::write(&sources_path, content).unwrap();

        let config = create_test_config();
        let mut manager = ExtractManager::new(config);
        manager.load_sources(&sources_path).unwrap();

        // Extracting openssl (with placeholder) should fail validation
        let result = manager.extract("openssl", false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("SHA-256 must be 64 hex characters, got 35"));

        // Extracting zlib (with valid checksum format) should pass validation
        // (will fail on missing archive, but validation passes)
        let result = manager.extract("zlib", false);
        // zlib has valid checksum format but no archive - should fail on missing file, not validation
        assert!(result.is_err());
        // The error should be about missing file, not about checksum validation
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("SHA-256 must be 64 hex characters"));
    }

    #[test]
    fn test_extract_strips_top_level_directory() {
        // Test that archives with a single top-level directory (like zlib-1.4/)
        // have that directory stripped during extraction to avoid double-nesting.
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with a top-level directory "test-pkg-1.0/" containing files
        // This simulates typical source tarballs like zlib-1.4.tar.gz
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);

            // Add top-level directory entry
            let mut header_dir = tar::Header::new_gnu();
            header_dir.set_path("test-pkg-1.0/").unwrap();
            header_dir.set_size(0);
            header_dir.set_mode(0o755);
            header_dir.set_entry_type(tar::EntryType::Directory);
            header_dir.set_cksum();
            tar.append(&header_dir, &[][..]).unwrap();

            // Add file inside top-level directory
            let mut header_file = tar::Header::new_gnu();
            header_file.set_path("test-pkg-1.0/configure").unwrap();
            header_file.set_size(10);
            header_file.set_mode(0o755);
            header_file.set_cksum();
            tar.append(&header_file, &b"configure"[..]).unwrap();

            // Add another file inside top-level directory
            let mut header_file2 = tar::Header::new_gnu();
            header_file2.set_path("test-pkg-1.0/Makefile.in").unwrap();
            header_file2.set_size(10);
            header_file2.set_mode(0o644);
            header_file2.set_cksum();
            tar.append(&header_file2, &b"Makefile.in"[..]).unwrap();

            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false).unwrap();

        // Files should be directly in the extraction directory, not nested
        assert!(
            result.path.join("configure").exists(),
            "configure should be at root of extraction dir"
        );
        assert!(
            result.path.join("Makefile.in").exists(),
            "Makefile.in should be at root of extraction dir"
        );
        // The top-level directory should NOT exist
        assert!(
            !result.path.join("test-pkg-1.0").exists(),
            "top-level directory should be stripped"
        );
    }

    #[test]
    fn test_extract_no_top_level_directory() {
        // Test that archives WITHOUT a single top-level directory work normally
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with multiple top-level entries (no single common directory)
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);

            // Add file at root
            let mut header1 = tar::Header::new_gnu();
            header1.set_path("file1.txt").unwrap();
            header1.set_size(10);
            header1.set_mode(0o644);
            header1.set_cksum();
            tar.append(&header1, &b"file1"[..]).unwrap();

            // Add another file at root
            let mut header2 = tar::Header::new_gnu();
            header2.set_path("file2.txt").unwrap();
            header2.set_size(10);
            header2.set_mode(0o644);
            header2.set_cksum();
            tar.append(&header2, &b"file2"[..]).unwrap();

            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false).unwrap();

        // Files should be at root of extraction directory
        assert!(result.path.join("file1.txt").exists());
        assert!(result.path.join("file2.txt").exists());
    }

    #[test]
    fn test_extract_single_file_at_root() {
        // Test that a single file at archive root (not in a directory) works
        let config = create_test_config();
        let mut manager = ExtractManager::new(config.clone());

        let def = SourceDefinition {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            url: "https://example.com/test.tar.gz".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            archive: "test.tar.gz".to_string(),
        };

        manager.sources.insert("test-pkg".to_string(), def.clone());

        // Create archive with a single file at root (no directory)
        let mut compressed = Vec::new();
        {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use tar::Builder;

            let mut gz = GzEncoder::new(&mut compressed, Compression::default());
            let mut tar = Builder::new(&mut gz);

            let mut header = tar::Header::new_gnu();
            header.set_path("single.txt").unwrap();
            header.set_size(6);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, &b"single"[..]).unwrap();

            drop(tar);
            gz.finish().unwrap();
        }
        setup_source_archive(&config, &def, &compressed);

        let result = manager.extract("test-pkg", false).unwrap();

        assert!(result.path.join("single.txt").exists());
        assert_eq!(fs::read(result.path.join("single.txt")).unwrap(), b"single");
    }
}
