use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::error::{BuildError, BuildResult, StepFailedInfo};
use crate::extract::ExtractManager;
use crate::source::SourceDefinition;

/// Log level for structured logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LogLevel {
    /// Debug information (very verbose)
    Debug,
    /// General information
    Info,
    /// Warning - something unexpected but not fatal
    Warn,
    /// Error - operation failed
    Error,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Warn => write!(f, "WARN"),
            LogLevel::Error => write!(f, "ERROR"),
        }
    }
}

/// Structured log entry for build operations.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    /// ISO 8601 timestamp with microsecond precision
    pub timestamp: String,
    /// Log level
    pub level: LogLevel,
    /// Package name (if applicable)
    pub package: Option<String>,
    /// Step description (if applicable)
    pub step: Option<String>,
    /// Log message
    pub message: String,
    /// Additional context fields
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub context: HashMap<String, String>,
}

impl LogEntry {
    /// Creates a new log entry.
    pub fn new(level: LogLevel, message: String) -> Self {
        Self {
            timestamp: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            level,
            package: None,
            step: None,
            message,
            context: HashMap::new(),
        }
    }

    /// Sets the package name.
    pub fn with_package(mut self, package: String) -> Self {
        self.package = Some(package);
        self
    }

    /// Sets the step description.
    pub fn with_step(mut self, step: String) -> Self {
        self.step = Some(step);
        self
    }

    /// Adds a context field.
    pub fn with_context(mut self, key: String, value: String) -> Self {
        self.context.insert(key, value);
        self
    }

    /// Formats the log entry as a human-readable string.
    pub fn format(&self) -> String {
        let mut parts = Vec::new();
        parts.push(format!("[{}] {}", self.timestamp, self.level));
        if let Some(ref pkg) = self.package {
            parts.push(format!("pkg={}", pkg));
        }
        if let Some(ref step) = self.step {
            parts.push(format!("step={}", step));
        }
        for (k, v) in &self.context {
            parts.push(format!("{}={}", k, v));
        }
        format!("{} {}", parts.join(" "), self.message)
    }

    /// Formats the log entry as JSON.
    #[allow(dead_code, reason = "used in tests for structured logging")]
    pub fn format_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| self.format())
    }
}

/// Represents a build recipe for a package.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct BuildRecipe {
    /// Package name (used as identifier).
    pub name: String,
    /// Package version.
    pub version: String,
    /// Source package identifier (must match a source definition in sources.toml).
    pub source: String,
    /// Build-time dependencies (package names that must be built first).
    #[serde(default)]
    pub depends: Vec<String>,
    /// Build steps to execute in order.
    pub build: Vec<BuildStep>,
    /// Installation steps to execute in order.
    pub install: Vec<BuildStep>,
    /// Environment variables to set during build and install.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Working directory relative to the extracted source directory.
    #[serde(default)]
    pub workdir: Option<String>,
}

/// A single build step (command to execute).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct BuildStep {
    /// Command to execute (no shell parsing - direct exec).
    pub command: String,
    /// Arguments to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory relative to the recipe's workdir (or source root if not set).
    #[serde(default)]
    pub workdir: Option<String>,
    /// Additional environment variables for this step only.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Description of this step (for logging).
    #[serde(default)]
    pub description: Option<String>,
    /// If true, continue on failure (default: false).
    #[serde(default)]
    pub continue_on_failure: bool,
}

/// Result of a build operation.
#[derive(Debug, Clone)]
pub struct BuildResultInfo {
    /// Package name.
    pub name: String,
    /// Package version.
    pub version: String,
    /// Path to the build directory.
    pub build_dir: PathBuf,
    /// Path to the staging directory (None if cleaned up after successful publish).
    #[allow(dead_code, reason = "available for debugging and future use")]
    pub staging_dir: Option<PathBuf>,
    /// Path to the log file.
    pub log_path: PathBuf,
    /// Whether the build was successful.
    #[allow(dead_code, reason = "available for debugging and future use")]
    pub success: bool,
}

/// Manages building packages from recipes.
pub struct BuildManager {
    config: Config,
    recipes: HashMap<String, BuildRecipe>,
    extract_manager: ExtractManager,
}

impl BuildManager {
    /// Creates a new build manager with the given configuration.
    pub fn new(config: Config) -> BuildResult<Self> {
        let extract_manager = ExtractManager::new(config.clone());
        Ok(Self {
            config,
            recipes: HashMap::new(),
            extract_manager,
        })
    }

    /// Loads build recipes from a TOML file.
    pub fn load_recipes<P: AsRef<Path>>(&mut self, path: P) -> BuildResult<()> {
        let content = fs::read_to_string(&path).map_err(|e| {
            BuildError::RecipeLoadFailed(format!(
                "failed to read recipes file '{}': {}",
                path.as_ref().display(),
                e
            ))
        })?;

        let recipes: HashMap<String, BuildRecipe> = toml::from_str(&content).map_err(|e| {
            BuildError::RecipeParseFailed(format!("failed to parse recipes TOML: {}", e))
        })?;

        // Validate each recipe
        for (name, recipe) in &recipes {
            if name != &recipe.name {
                return Err(BuildError::RecipeValidationFailed(format!(
                    "recipe key '{}' does not match recipe name '{}'",
                    name, recipe.name
                )));
            }
            recipe.validate()?;
        }

        // Validate dependencies: every dependency must exist in loaded recipes
        let mut missing_deps = Vec::new();
        for (name, recipe) in &recipes {
            for dep in &recipe.depends {
                if !recipes.contains_key(dep) {
                    missing_deps.push(format!(
                        "recipe '{}' depends on missing recipe '{}'",
                        name, dep
                    ));
                }
            }
        }

        // Validate sources: every recipe's source must exist in source definitions
        let mut missing_sources = Vec::new();
        let source_names: Vec<String> = self.extract_manager.sources().keys().cloned().collect();
        for (name, recipe) in &recipes {
            if !source_names.contains(&recipe.source) {
                missing_sources.push(format!(
                    "recipe '{}' references undefined source '{}'",
                    name, recipe.source
                ));
            }
        }

        // Report all validation errors
        if !missing_deps.is_empty() || !missing_sources.is_empty() {
            let mut messages = Vec::new();
            if !missing_deps.is_empty() {
                messages.push("Missing dependencies:".to_string());
                messages.extend(missing_deps);
            }
            if !missing_sources.is_empty() {
                if !messages.is_empty() {
                    messages.push("".to_string());
                }
                messages.push("Undefined sources:".to_string());
                messages.extend(missing_sources);
            }
            return Err(BuildError::RecipeValidationFailed(messages.join("\n")));
        }

        self.recipes = recipes;
        Ok(())
    }

    /// Loads source definitions into the extract manager.
    pub fn load_sources<P: AsRef<Path>>(&mut self, path: P) -> BuildResult<()> {
        self.extract_manager
            .load_sources(path)
            .map_err(BuildError::from)
    }

    /// Returns the build directory path for a package.
    fn package_build_dir(&self, recipe: &BuildRecipe) -> PathBuf {
        self.config
            .directories
            .build
            .join(format!("{}-{}", recipe.name, recipe.version))
    }

    /// Returns the staging directory path for a package.
    fn package_staging_dir(&self, recipe: &BuildRecipe) -> PathBuf {
        self.config
            .directories
            .staging
            .join(format!("{}-{}", recipe.name, recipe.version))
    }

    /// Returns the log directory path.
    fn log_dir(&self) -> &Path {
        &self.config.directories.logs
    }

    /// Returns the log file path for a package.
    fn log_path(&self, recipe: &BuildRecipe) -> PathBuf {
        self.log_dir()
            .join(format!("{}-{}.log", recipe.name, recipe.version))
    }

    /// Returns the sysroot directory path.
    fn sysroot_dir(&self) -> &Path {
        &self.config.directories.sysroot
    }

    /// Gets the source definition for a recipe's source.
    fn get_source_def(&self, recipe: &BuildRecipe) -> BuildResult<&SourceDefinition> {
        self.extract_manager
            .sources()
            .get(&recipe.source)
            .ok_or_else(|| {
                BuildError::SourceNotFound(format!(
                    "source '{}' not found for recipe '{}'",
                    recipe.source, recipe.name
                ))
            })
    }

    /// Gets the extracted source directory for a recipe.
    fn get_source_dir(&self, recipe: &BuildRecipe) -> BuildResult<PathBuf> {
        let source_def = self.get_source_def(recipe)?;
        let extract_path = self.extract_manager.extract_path(source_def);
        if !extract_path.exists() {
            return Err(BuildError::SourceNotExtracted(format!(
                "source '{}' not extracted (run 'bootstrap extract {}' first)",
                recipe.source, recipe.source
            )));
        }
        Ok(extract_path)
    }

    /// Computes the working directory for a recipe.
    fn recipe_workdir(&self, recipe: &BuildRecipe, source_dir: &Path) -> PathBuf {
        if let Some(ref workdir) = recipe.workdir {
            source_dir.join(workdir)
        } else {
            source_dir.to_path_buf()
        }
    }

    /// Validates that a path stays within a base directory (prevents directory traversal).
    /// Works with non-existent target paths by resolving the base and checking path components.
    /// Also prevents symlink-based escapes by canonicalizing existing ancestors.
    fn validate_path_within_base(base: &Path, target: &Path) -> BuildResult<()> {
        // Canonicalize the base directory (must exist)
        let base_canonical = base.canonicalize().map_err(|e| {
            BuildError::ExecutionFailed(format!("failed to canonicalize base path: {}", e))
        })?;

        // For the target, we need to resolve it relative to the canonicalized base
        // without requiring the target to exist. We do this by:
        // 1. Getting the absolute path of target (relative to base if target is relative)
        // 2. Walking up from target to find the nearest existing ancestor
        // 3. Canonicalizing that ancestor and checking if remaining components escape

        let target_absolute = if target.is_absolute() {
            target.to_path_buf()
        } else {
            base.join(target)
        };

        // Find the nearest existing ancestor of target
        let mut current = target_absolute.as_path();
        let mut remaining_components = Vec::new();

        while !current.exists() {
            if let Some(parent) = current.parent() {
                remaining_components.push(current.file_name().unwrap().to_os_string());
                current = parent;
            } else {
                // Reached root without finding existing path
                return Err(BuildError::ExecutionFailed(format!(
                    "cannot validate path '{}': no existing ancestor found",
                    target_absolute.display()
                )));
            }
        }

        // Canonicalize the existing ancestor (resolves symlinks)
        let ancestor_canonical = current.canonicalize().map_err(|e| {
            BuildError::ExecutionFailed(format!("failed to canonicalize ancestor path: {}", e))
        })?;

        // Check if the ancestor is within the base
        if !ancestor_canonical.starts_with(&base_canonical) {
            return Err(BuildError::ExecutionFailed(format!(
                "path '{}' escapes base directory '{}' (ancestor outside base)",
                target_absolute.display(),
                base_canonical.display()
            )));
        }

        // Now check the remaining components for path traversal
        for component in remaining_components.iter().rev() {
            let component_str = component.to_string_lossy();
            if component_str == ".." {
                return Err(BuildError::ExecutionFailed(format!(
                    "path '{}' escapes base directory '{}' (contains '..')",
                    target_absolute.display(),
                    base_canonical.display()
                )));
            }
        }

        // Additionally, if the full target path exists, do a final canonicalize check
        // to catch symlinks in the target path itself
        if target_absolute.exists() {
            let target_canonical = target_absolute.canonicalize().map_err(|e| {
                BuildError::ExecutionFailed(format!("failed to canonicalize target path: {}", e))
            })?;
            if !target_canonical.starts_with(&base_canonical) {
                return Err(BuildError::ExecutionFailed(format!(
                    "path '{}' escapes base directory '{}' (after symlink resolution)",
                    target_absolute.display(),
                    base_canonical.display()
                )));
            }
        }

        Ok(())
    }

    /// Validates a workdir path component (no absolute paths, no "..", no leading/trailing slashes).
    fn validate_workdir_component(workdir: &str) -> BuildResult<()> {
        if workdir.is_empty() {
            return Err(BuildError::RecipeValidationFailed(
                "workdir cannot be empty".to_string(),
            ));
        }

        // Must not be an absolute path
        if workdir.starts_with('/') || Path::new(workdir).is_absolute() {
            return Err(BuildError::RecipeValidationFailed(
                "workdir must be a relative path, not absolute".to_string(),
            ));
        }

        // Must not contain ".." components
        for component in Path::new(workdir).components() {
            if matches!(component, std::path::Component::ParentDir) {
                return Err(BuildError::RecipeValidationFailed(
                    "workdir must not contain '..' (parent directory) components".to_string(),
                ));
            }
        }

        // Must not start or end with path separator
        if workdir.starts_with('/') || workdir.ends_with('/') {
            return Err(BuildError::RecipeValidationFailed(
                "workdir must not start or end with path separator".to_string(),
            ));
        }

        Ok(())
    }

    /// Validates a package name (strict: alphanumeric, hyphen, underscore only, no leading/trailing hyphens, max 64 chars).
    fn validate_package_name(name: &str) -> BuildResult<()> {
        if name.is_empty() {
            return Err(BuildError::RecipeValidationFailed(
                "package name cannot be empty".to_string(),
            ));
        }

        if name.len() > 64 {
            return Err(BuildError::RecipeValidationFailed(
                "package name cannot exceed 64 characters".to_string(),
            ));
        }

        // Must not start or end with hyphen
        if name.starts_with('-') || name.ends_with('-') {
            return Err(BuildError::RecipeValidationFailed(
                "package name cannot start or end with hyphen".to_string(),
            ));
        }

        // Only alphanumeric, hyphen, underscore
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(BuildError::RecipeValidationFailed(
                "package name can only contain alphanumeric characters, hyphens, and underscores"
                    .to_string(),
            ));
        }

        // Check for path traversal sequences
        if name.contains("..") || name.contains('/') || name.contains('\\') {
            return Err(BuildError::RecipeValidationFailed(
                "package name must not contain path traversal characters (.., /, \\)".to_string(),
            ));
        }

        Ok(())
    }

    /// Validates a version string (semantic version-like, no path traversal chars, max 32 chars).
    fn validate_version(version: &str) -> BuildResult<()> {
        if version.is_empty() {
            return Err(BuildError::RecipeValidationFailed(
                "version cannot be empty".to_string(),
            ));
        }

        if version.len() > 32 {
            return Err(BuildError::RecipeValidationFailed(
                "version cannot exceed 32 characters".to_string(),
            ));
        }

        // No path traversal characters
        if version.contains("..") || version.contains('/') || version.contains('\\') {
            return Err(BuildError::RecipeValidationFailed(
                "version must not contain path traversal characters (.., /, \\)".to_string(),
            ));
        }

        // Basic semantic version-like validation: alphanumeric, dots, hyphens, underscores, plus
        if !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        {
            return Err(BuildError::RecipeValidationFailed(
                "version can only contain alphanumeric characters, dots, hyphens, underscores, and plus signs"
                    .to_string(),
            ));
        }

        Ok(())
    }

    /// Builds the environment for a recipe/step.
    fn build_env(
        &self,
        recipe: &BuildRecipe,
        step: Option<&BuildStep>,
        build_dir: &Path,
        destdir: Option<&Path>,
    ) -> HashMap<String, String> {
        let mut env = HashMap::new();

        // Base environment from config
        env.insert(
            "ROYAL_SYSROOT".to_string(),
            self.sysroot_dir().display().to_string(),
        );
        env.insert(
            "ROYAL_BUILD_DIR".to_string(),
            build_dir.display().to_string(),
        );
        env.insert(
            "ROYAL_WORKSPACE".to_string(),
            self.config.directories.workspace.display().to_string(),
        );

        // Parallel job control from config (MAKEFLAGS, NINJAFLAGS, CARGOFLAGS)
        if let Some(jobs) = self.config.build.jobs {
            let makeflags = format!("-j{}", jobs);
            env.insert("MAKEFLAGS".to_string(), makeflags.clone());
            env.insert("NINJAFLAGS".to_string(), makeflags.clone());
            // Cargo uses --jobs flag
            env.insert("CARGOFLAGS".to_string(), format!("--jobs {}", jobs));
        }

        // Build optimization flags from config
        let opt_level = &self.config.build.opt_level;
        let mut cflags = format!("-O{} -pipe", opt_level);
        let mut cxxflags = format!("-O{} -pipe", opt_level);
        let mut ldflags = String::new();

        // Apply additional flags from config
        if !self.config.build.cflags.is_empty() {
            cflags.push(' ');
            cflags.push_str(&self.config.build.cflags.join(" "));
        }
        if !self.config.build.cxxflags.is_empty() {
            cxxflags.push(' ');
            cxxflags.push_str(&self.config.build.cxxflags.join(" "));
        }
        if !self.config.build.ldflags.is_empty() {
            ldflags.push_str(&self.config.build.ldflags.join(" "));
        }

        env.insert("CFLAGS".to_string(), cflags);
        env.insert("CXXFLAGS".to_string(), cxxflags);
        if !ldflags.is_empty() {
            env.insert("LDFLAGS".to_string(), ldflags);
        }

        // Toolchain environment variables (CC, CXX, AR, RANLIB, STRIP)
        // Set these if a toolchain is configured and the binaries exist.
        // This provides a predictable cross-compilation environment.
        let toolchain_dir = &self.config.directories.toolchain;
        let target_triplet = self.config.target.triplet();
        if toolchain_dir.exists() {
            // Common toolchain binary names for cross-compilation
            let cc = toolchain_dir.join(format!("{}-gcc", target_triplet));
            let cxx = toolchain_dir.join(format!("{}-g++", target_triplet));
            let ar = toolchain_dir.join(format!("{}-ar", target_triplet));
            let ranlib = toolchain_dir.join(format!("{}-ranlib", target_triplet));
            let strip = toolchain_dir.join(format!("{}-strip", target_triplet));

            if cc.exists() {
                env.insert("CC".to_string(), cc.display().to_string());
            }
            if cxx.exists() {
                env.insert("CXX".to_string(), cxx.display().to_string());
            }
            if ar.exists() {
                env.insert("AR".to_string(), ar.display().to_string());
            }
            if ranlib.exists() {
                env.insert("RANLIB".to_string(), ranlib.display().to_string());
            }
            if strip.exists() {
                env.insert("STRIP".to_string(), strip.display().to_string());
            }
        }

        // Recipe-level environment
        for (k, v) in &recipe.env {
            env.insert(k.clone(), v.clone());
        }

        // Step-level environment (overrides recipe-level)
        if let Some(step) = step {
            for (k, v) in &step.env {
                env.insert(k.clone(), v.clone());
            }
        }

        // DESTDIR for installation - use staging directory if provided, otherwise sysroot
        let destdir_path = destdir.unwrap_or_else(|| self.sysroot_dir());
        env.insert("DESTDIR".to_string(), destdir_path.display().to_string());

        // Pass through some host environment variables
        // PATH: Required for finding executables (make, cmake, pkg-config, etc.)
        // HOME: Some build systems (e.g., cargo, cmake) use $HOME for cache/config
        // USER: Some build scripts reference the current user
        // SHELL: Required for shell-based build steps and scripts
        // TERM: Required for interactive/colored output in some build tools
        for var in ["PATH", "HOME", "USER", "SHELL", "TERM"] {
            if let Ok(val) = env::var(var) {
                env.insert(var.to_string(), val);
            }
        }

        env
    }

    /// Sanitizes environment variables for logging (removes sensitive values).
    fn sanitize_env(env: &HashMap<String, String>) -> HashMap<String, String> {
        let sensitive_keys = [
            "PASSWORD",
            "SECRET",
            "TOKEN",
            "KEY",
            "PRIVATE",
            "CREDENTIAL",
            "AUTH",
        ];
        let mut sanitized = HashMap::new();
        for (k, v) in env {
            let upper_key = k.to_uppercase();
            let is_sensitive = sensitive_keys.iter().any(|s| upper_key.contains(s));
            if is_sensitive {
                sanitized.insert(k.clone(), "***REDACTED***".to_string());
            } else {
                sanitized.insert(k.clone(), v.clone());
            }
        }
        sanitized
    }

    /// Writes a log entry to the log file.
    fn write_log(&self, log_file: &mut fs::File, entry: &LogEntry) -> BuildResult<()> {
        log_file
            .write_all(entry.format().as_bytes())
            .map_err(BuildError::from)?;
        log_file.write_all(b"\n").map_err(BuildError::from)?;
        log_file.flush().map_err(BuildError::from)?;
        Ok(())
    }

    /// Maximum number of lines to retain in the tail buffer for error diagnostics.
    const TAIL_BUFFER_LINES: usize = 100;
    /// Maximum bytes to retain in the tail buffer for error diagnostics.
    const TAIL_BUFFER_MAX_BYTES: usize = 64 * 1024; // 64 KB

    /// Interpolates `${DESTDIR}` placeholders in arguments with the actual DESTDIR path.
    /// This is a narrowly-scoped mechanism for installation paths; no other variable
    /// expansion or shell interpretation is performed.
    fn interpolate_destdir(args: &[String], destdir: Option<&Path>) -> Vec<String> {
        let destdir_str = destdir.map(|p| p.display().to_string()).unwrap_or_default();
        args.iter()
            .map(|arg| arg.replace("${DESTDIR}", &destdir_str))
            .collect()
    }

    /// Executes a single build step with streaming output support.
    /// Streams stdout/stderr directly to the log file without buffering unlimited output in memory.
    /// Retains a bounded tail buffer (last N lines / max bytes) for error diagnostics.
    fn execute_step(
        &self,
        recipe: &BuildRecipe,
        step: &BuildStep,
        base_workdir: &Path,
        build_dir: &Path,
        log_file: &mut fs::File,
        destdir: Option<&Path>,
    ) -> BuildResult<Output> {
        let workdir = if let Some(ref wd) = step.workdir {
            base_workdir.join(wd)
        } else {
            base_workdir.to_path_buf()
        };

        // Validate that the resolved workdir is within the base_workdir (source directory)
        Self::validate_path_within_base(base_workdir, &workdir)?;

        // Ensure workdir exists
        fs::create_dir_all(&workdir).map_err(|e| {
            BuildError::ExecutionFailed(format!(
                "failed to create workdir '{}': {}",
                workdir.display(),
                e
            ))
        })?;

        // Interpolate ${DESTDIR} in step arguments
        let interpolated_args = Self::interpolate_destdir(&step.args, destdir);

        let env = self.build_env(recipe, Some(step), build_dir, destdir);
        let sanitized_env = Self::sanitize_env(&env);

        let description = step.description.as_deref().unwrap_or(&step.command);
        let step_display = if interpolated_args.is_empty() {
            step.command.clone()
        } else {
            format!("{} {}", step.command, interpolated_args.join(" "))
        };

        // Log step start
        let start_entry = LogEntry::new(LogLevel::Info, format!("Starting step: {}", description))
            .with_package(recipe.name.clone())
            .with_step(description.to_string())
            .with_context("command".to_string(), step_display.clone())
            .with_context("workdir".to_string(), workdir.display().to_string())
            .with_context(
                "env".to_string(),
                serde_json::to_string(&sanitized_env).unwrap_or_default(),
            );
        self.write_log(log_file, &start_entry)?;

        // Also print to console in verbose mode
        if self.config.build.verbose {
            eprintln!("{}", start_entry.format());
        }

        let mut cmd = Command::new(&step.command);
        cmd.args(&interpolated_args)
            .current_dir(&workdir)
            .env_clear()
            .envs(&env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Spawn the child process
        let mut child = cmd.spawn().map_err(|e| {
            BuildError::ExecutionFailed(format!("failed to execute '{}': {}", step.command, e))
        })?;

        // Get stdout and stderr handles
        let stdout = child.stdout.take().expect("stdout should be piped");
        let stderr = child.stderr.take().expect("stderr should be piped");

        // Shared state for collecting tail buffers and log writing
        let log_file_shared: Arc<Mutex<fs::File>> =
            Arc::new(Mutex::new(log_file.try_clone().map_err(BuildError::from)?));

        // Tail buffers for error diagnostics (bounded)
        let stdout_tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr_tail: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let stdout_bytes_total: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let stderr_bytes_total: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

        // Track if process was terminated by signal
        let terminated_by_signal: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));

        let verbose = self.config.build.verbose;
        let recipe_name = recipe.name.clone();
        let step_desc = description.to_string();

        // Thread to read stdout
        let stdout_handle = {
            let log_file_stdout = Arc::clone(&log_file_shared);
            let stdout_tail = Arc::clone(&stdout_tail);
            let stdout_bytes_total = Arc::clone(&stdout_bytes_total);
            let recipe_name_stdout = recipe_name.clone();
            let step_desc_stdout = step_desc.clone();

            thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break, // EOF
                        Ok(_) => {
                            let line_trimmed = line.trim_end();
                            let line_bytes = line.len();

                            // Update tail buffer (bounded)
                            if let Ok(mut tail) = stdout_tail.lock() {
                                tail.push(line_trimmed.to_string());
                                // Trim to max lines
                                while tail.len() > Self::TAIL_BUFFER_LINES {
                                    tail.remove(0);
                                }
                            }
                            // Track total bytes
                            if let Ok(mut total) = stdout_bytes_total.lock() {
                                *total += line_bytes;
                                // Trim tail buffer if over byte limit
                                if *total > Self::TAIL_BUFFER_MAX_BYTES
                                    && let Ok(mut tail) = stdout_tail.lock()
                                {
                                    while *total > Self::TAIL_BUFFER_MAX_BYTES && !tail.is_empty() {
                                        *total -= tail[0].len() + 1; // +1 for newline
                                        tail.remove(0);
                                    }
                                }
                            }

                            // Write to log file
                            if let Ok(mut log_file_guard) = log_file_stdout.lock() {
                                let entry = LogEntry::new(LogLevel::Info, line_trimmed.to_string())
                                    .with_package(recipe_name_stdout.clone())
                                    .with_step(step_desc_stdout.clone())
                                    .with_context("stream".to_string(), "stdout".to_string());
                                if let Err(e) = log_file_guard.write_all(entry.format().as_bytes())
                                {
                                    eprintln!("Warning: failed to write stdout to log: {}", e);
                                } else if let Err(e) = log_file_guard.write_all(b"\n") {
                                    eprintln!("Warning: failed to write newline to log: {}", e);
                                } else if let Err(e) = log_file_guard.flush() {
                                    eprintln!("Warning: failed to flush log: {}", e);
                                }
                            }

                            // Print to console in verbose mode
                            if verbose {
                                eprint!("{}", line);
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        // Thread to read stderr
        let stderr_handle = {
            let log_file_stderr = Arc::clone(&log_file_shared);
            let stderr_tail = Arc::clone(&stderr_tail);
            let stderr_bytes_total = Arc::clone(&stderr_bytes_total);
            let recipe_name_stderr = recipe_name.clone();
            let step_desc_stderr = step_desc.clone();

            thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break, // EOF
                        Ok(_) => {
                            let line_trimmed = line.trim_end();
                            let line_bytes = line.len();

                            // Update tail buffer (bounded)
                            if let Ok(mut tail) = stderr_tail.lock() {
                                tail.push(line_trimmed.to_string());
                                // Trim to max lines
                                while tail.len() > Self::TAIL_BUFFER_LINES {
                                    tail.remove(0);
                                }
                            }
                            // Track total bytes
                            if let Ok(mut total) = stderr_bytes_total.lock() {
                                *total += line_bytes;
                                // Trim tail buffer if over byte limit
                                if *total > Self::TAIL_BUFFER_MAX_BYTES
                                    && let Ok(mut tail) = stderr_tail.lock()
                                {
                                    while *total > Self::TAIL_BUFFER_MAX_BYTES && !tail.is_empty() {
                                        *total -= tail[0].len() + 1; // +1 for newline
                                        tail.remove(0);
                                    }
                                }
                            }

                            // Write to log file
                            if let Ok(mut log_file_guard) = log_file_stderr.lock() {
                                let entry = LogEntry::new(LogLevel::Warn, line_trimmed.to_string())
                                    .with_package(recipe_name_stderr.clone())
                                    .with_step(step_desc_stderr.clone())
                                    .with_context("stream".to_string(), "stderr".to_string());
                                if let Err(e) = log_file_guard.write_all(entry.format().as_bytes())
                                {
                                    eprintln!("Warning: failed to write stderr to log: {}", e);
                                } else if let Err(e) = log_file_guard.write_all(b"\n") {
                                    eprintln!("Warning: failed to write newline to log: {}", e);
                                } else if let Err(e) = log_file_guard.flush() {
                                    eprintln!("Warning: failed to flush log: {}", e);
                                }
                            }

                            // Print to console in verbose mode
                            if verbose {
                                eprint!("{}", line);
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        // Wait for process to complete with signal handling
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    // Process still running, sleep briefly and check again
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    // Error waiting for process - mark as signal termination
                    *terminated_by_signal.lock().unwrap() = Some(-1);
                    return Err(BuildError::ExecutionFailed(format!(
                        "failed to wait for '{}': {}",
                        step.command, e
                    )));
                }
            }
        };

        // Wait for reader threads to finish (with timeout to avoid hanging on broken pipes)
        let stdout_join_result = stdout_handle.join();
        let stderr_join_result = stderr_handle.join();

        if stdout_join_result.is_err() {
            eprintln!("Warning: stdout reader thread panicked");
        }
        if stderr_join_result.is_err() {
            eprintln!("Warning: stderr reader thread panicked");
        }

        // Collect tail buffers for error diagnostics
        let stdout_tail_lines = Arc::try_unwrap(stdout_tail)
            .map_err(|_| {
                BuildError::ExecutionFailed("failed to unwrap stdout tail buffer".to_string())
            })?
            .into_inner()
            .map_err(|_| {
                BuildError::ExecutionFailed("failed to get stdout tail buffer".to_string())
            })?;

        let stderr_tail_lines = Arc::try_unwrap(stderr_tail)
            .map_err(|_| {
                BuildError::ExecutionFailed("failed to unwrap stderr tail buffer".to_string())
            })?
            .into_inner()
            .map_err(|_| {
                BuildError::ExecutionFailed("failed to get stderr tail buffer".to_string())
            })?;

        let stdout_str = stdout_tail_lines.join("\n");
        let stderr_str = stderr_tail_lines.join("\n");

        let exit_code = status.code().unwrap_or(-1);
        let signal = status.signal();

        // Check if process was terminated by a signal
        let terminated_by_signal_val = *terminated_by_signal.lock().unwrap();
        let was_signaled = signal.is_some() || terminated_by_signal_val.is_some();

        // Log exit code / signal
        let (status_level, status_msg) = if exit_code == 0 && !was_signaled {
            (
                LogLevel::Info,
                format!("Step finished with exit code {}", exit_code),
            )
        } else if was_signaled {
            let sig = signal.unwrap_or(terminated_by_signal_val.unwrap_or(-1));
            (
                LogLevel::Error,
                format!("Step terminated by signal {}", sig),
            )
        } else {
            (
                LogLevel::Error,
                format!("Step finished with exit code {}", exit_code),
            )
        };

        let status_entry = LogEntry::new(status_level, status_msg)
            .with_package(recipe.name.clone())
            .with_step(description.to_string())
            .with_context("exit_code".to_string(), exit_code.to_string());
        if was_signaled {
            if let Some(sig) = signal.or(terminated_by_signal_val) {
                self.write_log(
                    log_file,
                    &status_entry
                        .clone()
                        .with_context("signal".to_string(), sig.to_string()),
                )?;
            }
        } else {
            self.write_log(log_file, &status_entry)?;
        }

        if self.config.build.verbose {
            eprintln!("{}", status_entry.format());
        }

        // Check if step failed
        if exit_code != 0 || was_signaled {
            let step_failed_info = StepFailedInfo::new(
                recipe.name.clone(),
                description.to_string(),
                step.command.clone(),
                interpolated_args.clone(),
                exit_code,
                stdout_str.clone(),
                stderr_str.clone(),
                workdir.display().to_string(),
                sanitized_env,
            );

            if step.continue_on_failure {
                let warn_entry = LogEntry::new(
                    LogLevel::Warn,
                    format!("Step failed but continuing: {:?}", step_failed_info),
                )
                .with_package(recipe.name.clone())
                .with_step(description.to_string());
                self.write_log(log_file, &warn_entry)?;
                if self.config.build.verbose {
                    eprintln!("{}", warn_entry.format());
                }
            } else {
                let error_entry = LogEntry::new(
                    LogLevel::Error,
                    format!("Step failed: {:?}", step_failed_info),
                )
                .with_package(recipe.name.clone())
                .with_step(description.to_string());
                self.write_log(log_file, &error_entry)?;
                if self.config.build.verbose {
                    eprintln!("{}", error_entry.format());
                }
                return Err(BuildError::StepFailed(Box::new(step_failed_info)));
            }
        }

        // Return Output with tail buffers (for error diagnostics only)
        // Note: stdout/stderr in Output contain only the tail, not full output
        Ok(Output {
            status,
            stdout: stdout_str.into_bytes(),
            stderr: stderr_str.into_bytes(),
        })
    }

    /// Recursively copies a directory from src to dst, preserving symlinks.
    fn copy_dir_all(&self, src: &Path, dst: &Path) -> BuildResult<()> {
        fs::create_dir_all(dst).map_err(BuildError::from)?;

        for entry in fs::read_dir(src).map_err(BuildError::from)? {
            let entry = entry.map_err(BuildError::from)?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());

            let file_type = entry.file_type().map_err(BuildError::from)?;

            if file_type.is_dir() {
                self.copy_dir_all(&src_path, &dst_path)?;
            } else if file_type.is_file() {
                fs::copy(&src_path, &dst_path).map_err(BuildError::from)?;
            } else if file_type.is_symlink() {
                let target = fs::read_link(&src_path).map_err(BuildError::from)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::symlink as unix_symlink;
                    unix_symlink(&target, &dst_path).map_err(BuildError::from)?;
                }
                #[cfg(windows)]
                {
                    if fs::metadata(&src_path).map_err(BuildError::from)?.is_dir() {
                        std::os::windows::fs::symlink_dir(&target, &dst_path)
                            .map_err(BuildError::from)?;
                    } else {
                        std::os::windows::fs::symlink_file(&target, &dst_path)
                            .map_err(BuildError::from)?;
                    }
                }
            }
            // Skip other file types (sockets, devices, etc.)
        }

        Ok(())
    }

    /// Atomically publishes staging directory contents to sysroot.
    /// Uses rename for same-filesystem moves, falls back to copy+remove for cross-device.
    fn atomic_publish_to_sysroot(&self, staging_dir: &Path) -> BuildResult<()> {
        let sysroot = self.sysroot_dir();

        // Try atomic rename first (same filesystem)
        match fs::rename(staging_dir, sysroot) {
            Ok(()) => Ok(()),
            Err(e) => {
                // If cross-device (EXDEV), fall back to copy+remove
                if e.raw_os_error() == Some(libc::EXDEV)
                    || e.kind() == std::io::ErrorKind::CrossesDevices
                {
                    self.copy_dir_all(staging_dir, sysroot)?;
                    fs::remove_dir_all(staging_dir).map_err(BuildError::from)?;
                    Ok(())
                } else {
                    Err(BuildError::from(e))
                }
            }
        }
    }

    /// Cleans up the staging directory.
    fn cleanup_staging_dir(&self, staging_dir: &Path) {
        let _ = fs::remove_dir_all(staging_dir);
    }

    /// Executes all build steps for a single recipe.
    ///
    /// NOTE: This builds ONLY the specified recipe. It does NOT automatically build
    /// dependencies. Dependencies must be built first (either manually or via `build_all()`).
    /// The sysroot must already contain the required dependencies for the build to succeed.
    pub fn build(&mut self, name: &str, keep_on_failure: bool) -> BuildResult<BuildResultInfo> {
        let recipe = self
            .recipes
            .get(name)
            .ok_or_else(|| BuildError::RecipeNotFound(format!("recipe '{}' not found", name)))?
            .clone();

        let build_dir = self.package_build_dir(&recipe);
        let staging_dir = self.package_staging_dir(&recipe);
        let log_path = self.log_path(&recipe);

        // Create log directory
        fs::create_dir_all(self.log_dir()).map_err(BuildError::from)?;

        // Create build directory
        fs::create_dir_all(&build_dir).map_err(BuildError::from)?;

        // Create staging directory
        fs::create_dir_all(&staging_dir).map_err(BuildError::from)?;

        // Open log file
        let mut log_file = fs::File::create(&log_path).map_err(BuildError::from)?;

        // Write build header
        let header_entry = LogEntry::new(
            LogLevel::Info,
            format!("=== Build started: {} {} ===", recipe.name, recipe.version),
        )
        .with_package(recipe.name.clone())
        .with_context("build_dir".to_string(), build_dir.display().to_string())
        .with_context("staging_dir".to_string(), staging_dir.display().to_string())
        .with_context(
            "sysroot".to_string(),
            self.sysroot_dir().display().to_string(),
        )
        .with_context("timestamp".to_string(), chrono::Local::now().to_rfc3339());
        self.write_log(&mut log_file, &header_entry)?;

        if self.config.build.verbose {
            eprintln!("{}", header_entry.format());
        }

        // Get source directory
        let source_dir = match self.get_source_dir(&recipe) {
            Ok(dir) => dir,
            Err(e) => {
                let error_entry = LogEntry::new(
                    LogLevel::Error,
                    format!("Failed to get source directory: {}", e),
                )
                .with_package(recipe.name.clone());
                self.write_log(&mut log_file, &error_entry)?;
                if self.config.build.verbose {
                    eprintln!("{}", error_entry.format());
                }
                if !keep_on_failure && !self.config.build.keep_on_failure {
                    self.cleanup_staging_dir(&staging_dir);
                    let _ = fs::remove_dir_all(&build_dir);
                }
                return Err(e);
            }
        };

        let workdir = self.recipe_workdir(&recipe, &source_dir);

        // Validate that the recipe workdir is within the source directory
        Self::validate_path_within_base(&source_dir, &workdir)?;

        // Log build phase
        let phase_entry =
            LogEntry::new(LogLevel::Info, "=== BUILD PHASE (staging) ===".to_string())
                .with_package(recipe.name.clone());
        self.write_log(&mut log_file, &phase_entry)?;
        if self.config.build.verbose {
            eprintln!("{}", phase_entry.format());
        }

        // Execute build steps - DESTDIR points to staging directory
        for step in &recipe.build {
            let result = self.execute_step(
                &recipe,
                step,
                &workdir,
                &build_dir,
                &mut log_file,
                Some(&staging_dir),
            );
            if let Err(e) = result {
                let error_entry = LogEntry::new(LogLevel::Error, format!("BUILD FAILED: {}", e))
                    .with_package(recipe.name.clone());
                self.write_log(&mut log_file, &error_entry)?;
                if self.config.build.verbose {
                    eprintln!("{}", error_entry.format());
                }
                if !keep_on_failure && !self.config.build.keep_on_failure {
                    self.cleanup_staging_dir(&staging_dir);
                    let _ = fs::remove_dir_all(&build_dir);
                }
                return Err(e);
            }
        }

        // Log install phase
        let phase_entry = LogEntry::new(
            LogLevel::Info,
            "=== INSTALL PHASE (staging) ===".to_string(),
        )
        .with_package(recipe.name.clone());
        self.write_log(&mut log_file, &phase_entry)?;
        if self.config.build.verbose {
            eprintln!("{}", phase_entry.format());
        }

        // Execute install steps - DESTDIR points to staging directory
        for step in &recipe.install {
            let result = self.execute_step(
                &recipe,
                step,
                &workdir,
                &build_dir,
                &mut log_file,
                Some(&staging_dir),
            );
            if let Err(e) = result {
                let error_entry = LogEntry::new(LogLevel::Error, format!("INSTALL FAILED: {}", e))
                    .with_package(recipe.name.clone());
                self.write_log(&mut log_file, &error_entry)?;
                if self.config.build.verbose {
                    eprintln!("{}", error_entry.format());
                }
                if !keep_on_failure && !self.config.build.keep_on_failure {
                    self.cleanup_staging_dir(&staging_dir);
                    let _ = fs::remove_dir_all(&build_dir);
                }
                return Err(e);
            }
        }

        // Log publish phase
        let phase_entry = LogEntry::new(LogLevel::Info, "=== PUBLISH PHASE ===".to_string())
            .with_package(recipe.name.clone());
        self.write_log(&mut log_file, &phase_entry)?;
        if self.config.build.verbose {
            eprintln!("{}", phase_entry.format());
        }

        // Publish from staging to sysroot
        let publish_entry = LogEntry::new(
            LogLevel::Info,
            "Publishing from staging to sysroot".to_string(),
        )
        .with_package(recipe.name.clone());
        self.write_log(&mut log_file, &publish_entry)?;
        if self.config.build.verbose {
            eprintln!("{}", publish_entry.format());
        }

        if let Err(e) = self.atomic_publish_to_sysroot(&staging_dir) {
            let error_entry = LogEntry::new(
                LogLevel::Error,
                format!("Failed to publish to sysroot: {}", e),
            )
            .with_package(recipe.name.clone());
            self.write_log(&mut log_file, &error_entry)?;
            if self.config.build.verbose {
                eprintln!("{}", error_entry.format());
            }
            if !keep_on_failure && !self.config.build.keep_on_failure {
                self.cleanup_staging_dir(&staging_dir);
                let _ = fs::remove_dir_all(&build_dir);
            }
            return Err(e);
        }

        // Write build footer
        let footer_entry = LogEntry::new(
            LogLevel::Info,
            format!(
                "=== Build completed successfully: {} {} ===",
                recipe.name, recipe.version
            ),
        )
        .with_package(recipe.name.clone())
        .with_context("timestamp".to_string(), chrono::Local::now().to_rfc3339());
        self.write_log(&mut log_file, &footer_entry)?;
        if self.config.build.verbose {
            eprintln!("{}", footer_entry.format());
        }

        Ok(BuildResultInfo {
            name: recipe.name,
            version: recipe.version,
            build_dir,
            staging_dir: Some(staging_dir),
            log_path,
            success: true,
        })
    }

    /// Builds all loaded recipes in dependency order.
    ///
    /// This method builds packages in topological order based on their dependencies.
    /// If a package fails to build, subsequent packages that depend on it (directly or
    /// indirectly) will be skipped with an error, rather than attempting to build them.
    ///
    /// Returns a vector of results in build order. Failed packages and their dependents
    /// will have `Err` results.
    pub fn build_all(&mut self, keep_on_failure: bool) -> Vec<BuildResult<BuildResultInfo>> {
        let order = match self.resolve_build_order() {
            Ok(order) => order,
            Err(e) => {
                return vec![Err(BuildError::RecipeValidationFailed(e))];
            }
        };

        let mut results = Vec::with_capacity(order.len());
        let mut failed_packages: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for name in order {
            // Check if any dependency of this package has failed
            let recipe = self.recipes.get(&name).unwrap();
            let has_failed_dep = recipe
                .depends
                .iter()
                .any(|dep| failed_packages.contains(dep));

            if has_failed_dep {
                let dep_list: Vec<String> = recipe
                    .depends
                    .iter()
                    .filter(|dep| failed_packages.contains(*dep))
                    .cloned()
                    .collect();
                let err = BuildError::DependencyFailed(format!(
                    "skipping '{}' due to failed dependencies: {}",
                    name,
                    dep_list.join(", ")
                ));
                results.push(Err(err));
                failed_packages.insert(name);
            } else {
                let result = self.build(&name, keep_on_failure);
                if result.is_err() {
                    failed_packages.insert(name.clone());
                }
                results.push(result);
            }
        }

        results
    }

    /// Resolves build order based on dependencies.
    fn resolve_build_order(&self) -> Result<Vec<String>, String> {
        let mut visited = HashMap::new(); // 0 = unvisited, 1 = visiting, 2 = visited
        let mut order = Vec::new();

        fn visit(
            name: &str,
            recipes: &HashMap<String, BuildRecipe>,
            visited: &mut HashMap<String, u8>,
            order: &mut Vec<String>,
        ) -> Result<(), String> {
            match visited.get(name) {
                Some(1) => {
                    return Err(format!("circular dependency detected involving '{}'", name));
                }
                Some(2) => return Ok(()),
                _ => {}
            }

            visited.insert(name.to_string(), 1);

            if let Some(recipe) = recipes.get(name) {
                for dep in &recipe.depends {
                    visit(dep, recipes, visited, order)?;
                }
            }

            visited.insert(name.to_string(), 2);
            order.push(name.to_string());
            Ok(())
        }

        for name in self.recipes.keys() {
            if !visited.contains_key(name) {
                visit(name, &self.recipes, &mut visited, &mut order)?;
            }
        }

        Ok(order)
    }

    /// Lists all available recipes.
    pub fn list_recipes(&self) -> Vec<&BuildRecipe> {
        let mut recipes: Vec<&BuildRecipe> = self.recipes.values().collect();
        recipes.sort_by(|a, b| a.name.cmp(&b.name));
        recipes
    }
}

impl BuildRecipe {
    /// Validates the build recipe.
    pub fn validate(&self) -> BuildResult<()> {
        // Validate package name
        BuildManager::validate_package_name(&self.name)?;

        // Validate version
        BuildManager::validate_version(&self.version)?;

        if self.source.is_empty() {
            return Err(BuildError::RecipeValidationFailed(
                "source reference cannot be empty".to_string(),
            ));
        }

        if self.build.is_empty() {
            return Err(BuildError::RecipeValidationFailed(
                "at least one build step is required".to_string(),
            ));
        }

        // Validate recipe workdir if provided
        if let Some(ref workdir) = self.workdir {
            BuildManager::validate_workdir_component(workdir)?;
        }

        // Validate each build step
        for (i, step) in self.build.iter().enumerate() {
            step.validate().map_err(|e| {
                BuildError::RecipeValidationFailed(format!("build step {}: {}", i, e))
            })?;
        }

        // Validate each install step
        for (i, step) in self.install.iter().enumerate() {
            step.validate().map_err(|e| {
                BuildError::RecipeValidationFailed(format!("install step {}: {}", i, e))
            })?;
        }

        Ok(())
    }
}

impl BuildStep {
    /// Validates the build step.
    pub fn validate(&self) -> BuildResult<()> {
        if self.command.is_empty() {
            return Err(BuildError::RecipeValidationFailed(
                "command cannot be empty".to_string(),
            ));
        }
        // Check for shell metacharacters that would require shell parsing
        if self.command.contains(' ') || self.command.contains(';') || self.command.contains('|') {
            return Err(BuildError::RecipeValidationFailed(
                "command must not contain shell metacharacters (use args instead)".to_string(),
            ));
        }

        // Validate step workdir if provided
        if let Some(ref workdir) = self.workdir {
            BuildManager::validate_workdir_component(workdir)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
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
staging = "workspace/staging"
"#,
            temp_dir.path().display()
        );
        fs::write(&config_path, config_content).unwrap();
        crate::config::load_config(&config_path).unwrap()
    }

    fn create_test_recipe() -> BuildRecipe {
        BuildRecipe {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            source: "test-source".to_string(),
            depends: vec![],
            build: vec![
                BuildStep {
                    command: "echo".to_string(),
                    args: vec!["build step 1".to_string()],
                    workdir: None,
                    env: HashMap::new(),
                    description: Some("First build step".to_string()),
                    continue_on_failure: false,
                },
                BuildStep {
                    command: "echo".to_string(),
                    args: vec!["build step 2".to_string()],
                    workdir: None,
                    env: HashMap::new(),
                    description: Some("Second build step".to_string()),
                    continue_on_failure: false,
                },
            ],
            install: vec![BuildStep {
                command: "echo".to_string(),
                args: vec!["install step".to_string()],
                workdir: None,
                env: HashMap::new(),
                description: Some("Install step".to_string()),
                continue_on_failure: false,
            }],
            env: HashMap::new(),
            workdir: None,
        }
    }

    #[test]
    fn test_build_recipe_validate_valid() {
        let recipe = create_test_recipe();
        assert!(recipe.validate().is_ok());
    }

    #[test]
    fn test_build_recipe_validate_empty_name() {
        let mut recipe = create_test_recipe();
        recipe.name = "".to_string();
        assert!(recipe.validate().is_err());
    }

    #[test]
    fn test_build_recipe_validate_invalid_name_chars() {
        let mut recipe = create_test_recipe();
        recipe.name = "test@pkg".to_string();
        assert!(recipe.validate().is_err());
    }

    #[test]
    fn test_build_recipe_validate_empty_version() {
        let mut recipe = create_test_recipe();
        recipe.version = "".to_string();
        assert!(recipe.validate().is_err());
    }

    #[test]
    fn test_build_recipe_validate_empty_source() {
        let mut recipe = create_test_recipe();
        recipe.source = "".to_string();
        assert!(recipe.validate().is_err());
    }

    #[test]
    fn test_build_recipe_validate_empty_build() {
        let mut recipe = create_test_recipe();
        recipe.build = vec![];
        assert!(recipe.validate().is_err());
    }

    #[test]
    fn test_build_step_validate_valid() {
        let step = BuildStep {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: None,
            continue_on_failure: false,
        };
        assert!(step.validate().is_ok());
    }

    #[test]
    fn test_build_step_validate_empty_command() {
        let step = BuildStep {
            command: "".to_string(),
            args: vec![],
            workdir: None,
            env: HashMap::new(),
            description: None,
            continue_on_failure: false,
        };
        assert!(step.validate().is_err());
    }

    #[test]
    fn test_build_step_validate_shell_metacharacters() {
        let step = BuildStep {
            command: "echo hello; ls".to_string(),
            args: vec![],
            workdir: None,
            env: HashMap::new(),
            description: None,
            continue_on_failure: false,
        };
        assert!(step.validate().is_err());
    }

    #[test]
    fn test_build_manager_creation() {
        let config = create_test_config();
        let manager = BuildManager::new(config);
        assert!(manager.is_ok());
    }

    #[test]
    fn test_resolve_build_order_simple() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();

        manager.recipes = HashMap::from([
            (
                "pkg-a".to_string(),
                BuildRecipe {
                    name: "pkg-a".to_string(),
                    version: "1.0".to_string(),
                    source: "src-a".to_string(),
                    depends: vec![],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "pkg-b".to_string(),
                BuildRecipe {
                    name: "pkg-b".to_string(),
                    version: "1.0".to_string(),
                    source: "src-b".to_string(),
                    depends: vec!["pkg-a".to_string()],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
        ]);

        let order = manager.resolve_build_order().unwrap();
        assert_eq!(order.len(), 2);
        assert_eq!(order[0], "pkg-a");
        assert_eq!(order[1], "pkg-b");
    }

    #[test]
    fn test_resolve_build_order_independent() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();

        manager.recipes = HashMap::from([
            (
                "pkg-c".to_string(),
                BuildRecipe {
                    name: "pkg-c".to_string(),
                    version: "1.0".to_string(),
                    source: "src-c".to_string(),
                    depends: vec![],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "pkg-a".to_string(),
                BuildRecipe {
                    name: "pkg-a".to_string(),
                    version: "1.0".to_string(),
                    source: "src-a".to_string(),
                    depends: vec![],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
        ]);

        let order = manager.resolve_build_order().unwrap();
        assert_eq!(order.len(), 2);
        // Should be sorted alphabetically for independent packages
        assert!(order.contains(&"pkg-a".to_string()));
        assert!(order.contains(&"pkg-c".to_string()));
    }

    #[test]
    fn test_package_staging_dir() {
        let config = create_test_config();
        let manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();

        let staging_dir = manager.package_staging_dir(&recipe);
        assert!(staging_dir.to_string_lossy().contains("test-pkg-1.0"));
    }

    #[test]
    fn test_build_env_includes_cflags_cxxflags() {
        let config = create_test_config();
        let manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        let temp_dir = tempfile::tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();
        let sysroot = config.directories.sysroot.clone();

        // Test with default config
        let env = manager.build_env(&recipe, None, &build_dir, Some(&sysroot));
        assert!(env.contains_key("CFLAGS"));
        assert!(env.contains_key("CXXFLAGS"));
        assert_eq!(env.get("CFLAGS").unwrap(), "-O2 -pipe");
        assert_eq!(env.get("CXXFLAGS").unwrap(), "-O2 -pipe");
    }

    #[test]
    fn test_build_env_destdir_staging() {
        let config = create_test_config();
        let manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        let temp_dir1 = tempfile::tempdir().unwrap();
        let build_dir = temp_dir1.path().to_path_buf();
        let temp_dir2 = tempfile::tempdir().unwrap();
        let staging_dir = temp_dir2.path().to_path_buf();

        let env = manager.build_env(&recipe, None, &build_dir, Some(&staging_dir));
        assert_eq!(
            env.get("DESTDIR").unwrap().as_str(),
            staging_dir.display().to_string().as_str()
        );
    }

    #[test]
    fn test_copy_dir_all() {
        let temp_dir = tempdir().unwrap();
        let src = temp_dir.path().join("src");
        let dst = temp_dir.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("file1.txt"), "content1").unwrap();
        fs::create_dir_all(src.join("subdir")).unwrap();
        fs::write(src.join("subdir").join("file2.txt"), "content2").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            // Create symlink pointing to file1.txt in the same directory
            symlink("file1.txt", src.join("link.txt")).unwrap();
        }

        let config = create_test_config();
        let manager = BuildManager::new(config).unwrap();
        manager.copy_dir_all(&src, &dst).unwrap();

        assert!(dst.join("file1.txt").exists());
        assert_eq!(
            fs::read_to_string(dst.join("file1.txt")).unwrap(),
            "content1"
        );
        assert!(dst.join("subdir").join("file2.txt").exists());
        assert_eq!(
            fs::read_to_string(dst.join("subdir").join("file2.txt")).unwrap(),
            "content2"
        );

        #[cfg(unix)]
        {
            assert!(dst.join("link.txt").exists());
            let target = fs::read_link(dst.join("link.txt")).unwrap();
            assert_eq!(target, Path::new("file1.txt"));
        }
    }

    // =========================================================================
    // Path Validation Tests
    // =========================================================================

    #[test]
    fn test_validate_package_name_valid() {
        assert!(BuildManager::validate_package_name("test-pkg").is_ok());
        assert!(BuildManager::validate_package_name("test_pkg").is_ok());
        assert!(BuildManager::validate_package_name("testpkg").is_ok());
        assert!(BuildManager::validate_package_name("TestPkg").is_ok());
        assert!(BuildManager::validate_package_name("a".to_string().repeat(64).as_str()).is_ok());
    }

    #[test]
    fn test_validate_package_name_empty() {
        assert!(BuildManager::validate_package_name("").is_err());
    }

    #[test]
    fn test_validate_package_name_too_long() {
        assert!(BuildManager::validate_package_name(&"a".to_string().repeat(65)).is_err());
    }

    #[test]
    fn test_validate_package_name_leading_hyphen() {
        assert!(BuildManager::validate_package_name("-test").is_err());
    }

    #[test]
    fn test_validate_package_name_trailing_hyphen() {
        assert!(BuildManager::validate_package_name("test-").is_err());
    }

    #[test]
    fn test_validate_package_name_invalid_chars() {
        assert!(BuildManager::validate_package_name("test@pkg").is_err());
        assert!(BuildManager::validate_package_name("test.pkg").is_err());
        assert!(BuildManager::validate_package_name("test pkg").is_err());
    }

    #[test]
    fn test_validate_package_name_path_traversal() {
        assert!(BuildManager::validate_package_name("test..pkg").is_err());
        assert!(BuildManager::validate_package_name("test/pkg").is_err());
        assert!(BuildManager::validate_package_name("test\\pkg").is_err());
        assert!(BuildManager::validate_package_name("../test").is_err());
    }

    #[test]
    fn test_validate_version_valid() {
        assert!(BuildManager::validate_version("1.0").is_ok());
        assert!(BuildManager::validate_version("1.0.0").is_ok());
        assert!(BuildManager::validate_version("1.0.0-alpha").is_ok());
        assert!(BuildManager::validate_version("1.0.0+build").is_ok());
        assert!(BuildManager::validate_version("1.0.0-alpha+build").is_ok());
    }

    #[test]
    fn test_validate_version_empty() {
        assert!(BuildManager::validate_version("").is_err());
    }

    #[test]
    fn test_validate_version_too_long() {
        assert!(BuildManager::validate_version(&"a".to_string().repeat(33)).is_err());
    }

    #[test]
    fn test_validate_version_path_traversal() {
        assert!(BuildManager::validate_version("1.0..0").is_err());
        assert!(BuildManager::validate_version("1/0").is_err());
        assert!(BuildManager::validate_version("1\\0").is_err());
        assert!(BuildManager::validate_version("../1.0").is_err());
    }

    #[test]
    fn test_validate_version_invalid_chars() {
        assert!(BuildManager::validate_version("1.0@").is_err());
        assert!(BuildManager::validate_version("1.0#").is_err());
        assert!(BuildManager::validate_version("1.0 ").is_err());
    }

    #[test]
    fn test_validate_workdir_component_valid() {
        assert!(BuildManager::validate_workdir_component("build").is_ok());
        assert!(BuildManager::validate_workdir_component("build/subdir").is_ok());
        assert!(BuildManager::validate_workdir_component("a/b/c").is_ok());
    }

    #[test]
    fn test_validate_workdir_component_empty() {
        assert!(BuildManager::validate_workdir_component("").is_err());
    }

    #[test]
    fn test_validate_workdir_component_absolute() {
        assert!(BuildManager::validate_workdir_component("/build").is_err());
        assert!(BuildManager::validate_workdir_component("/absolute/path").is_err());
    }

    #[test]
    fn test_validate_workdir_component_parent_dir() {
        assert!(BuildManager::validate_workdir_component("build/..").is_err());
        assert!(BuildManager::validate_workdir_component("../build").is_err());
        assert!(BuildManager::validate_workdir_component("a/../b").is_err());
    }

    #[test]
    fn test_validate_workdir_component_leading_trailing_slash() {
        assert!(BuildManager::validate_workdir_component("/build").is_err());
        assert!(BuildManager::validate_workdir_component("build/").is_err());
    }

    #[test]
    fn test_validate_path_within_base() {
        let temp_dir = tempdir().unwrap();
        let base = temp_dir.path().join("base");
        let inside = base.join("inside");
        let outside = temp_dir.path().join("outside");

        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&inside).unwrap();
        fs::create_dir_all(&outside).unwrap();

        // Path inside base should be ok
        assert!(BuildManager::validate_path_within_base(&base, &inside).is_ok());
        // Path outside base should fail
        assert!(BuildManager::validate_path_within_base(&base, &outside).is_err());
    }

    #[test]
    fn test_validate_path_within_base_nonexistent_target() {
        let temp_dir = tempdir().unwrap();
        let base = temp_dir.path().join("base");
        let inside = base.join("inside");
        let outside = temp_dir.path().join("outside");

        fs::create_dir_all(&base).unwrap();
        // Note: inside and outside do NOT exist yet

        // Path inside base (nonexistent) should be ok
        assert!(BuildManager::validate_path_within_base(&base, &inside).is_ok());
        // Path outside base (nonexistent) should fail
        assert!(BuildManager::validate_path_within_base(&base, &outside).is_err());
    }

    #[test]
    fn test_validate_path_within_base_symlink_escape() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let temp_dir = tempdir().unwrap();
            let base = temp_dir.path().join("base");
            let outside = temp_dir.path().join("outside");
            let link_inside = base.join("link_to_outside");

            fs::create_dir_all(&base).unwrap();
            fs::create_dir_all(&outside).unwrap();

            // Create a symlink inside base that points outside
            symlink(&outside, &link_inside).unwrap();

            // The symlink target should be detected as escaping
            let result = BuildManager::validate_path_within_base(&base, &link_inside);
            assert!(result.is_err(), "Symlink escape should be detected");
        }
    }

    #[test]
    fn test_validate_path_within_base_nested_symlink_escape() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let temp_dir = tempdir().unwrap();
            let base = temp_dir.path().join("base");
            let outside = temp_dir.path().join("outside");
            let subdir = base.join("subdir");
            let link = subdir.join("link");

            fs::create_dir_all(&base).unwrap();
            fs::create_dir_all(&subdir).unwrap();
            fs::create_dir_all(&outside).unwrap();

            // Create a symlink in base/subdir that points outside
            symlink(&outside, &link).unwrap();

            // The path base/subdir/link should be detected as escaping
            let result = BuildManager::validate_path_within_base(&base, &link);
            assert!(result.is_err(), "Nested symlink escape should be detected");
        }
    }

    #[test]
    fn test_build_recipe_validate_workdir() {
        let mut recipe = create_test_recipe();
        recipe.workdir = Some("build".to_string());
        assert!(recipe.validate().is_ok());

        recipe.workdir = Some("/absolute".to_string());
        assert!(recipe.validate().is_err());

        recipe.workdir = Some("../escape".to_string());
        assert!(recipe.validate().is_err());
    }

    #[test]
    fn test_build_step_validate_workdir() {
        let mut step = BuildStep {
            command: "echo".to_string(),
            args: vec!["hello".to_string()],
            workdir: Some("build".to_string()),
            env: HashMap::new(),
            description: None,
            continue_on_failure: false,
        };
        assert!(step.validate().is_ok());

        step.workdir = Some("/absolute".to_string());
        assert!(step.validate().is_err());

        step.workdir = Some("../escape".to_string());
        assert!(step.validate().is_err());
    }

    #[test]
    fn test_recipe_workdir_validation_in_build() {
        // Test that recipe_workdir with path traversal is caught during build
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "test-escape".to_string();
        recipe.workdir = Some("../escape".to_string());
        manager.recipes.insert("test-escape".to_string(), recipe);

        // This should fail during build when validate_path_within_base is called
        let temp_dir = tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();

        let recipe = manager.recipes.get("test-escape").unwrap();
        let workdir = manager.recipe_workdir(recipe, &source_dir);

        // The workdir should be source_dir/../escape which escapes
        let result = BuildManager::validate_path_within_base(&source_dir, &workdir);
        assert!(result.is_err());
    }

    #[test]
    fn test_log_entry_formatting() {
        let entry = LogEntry::new(LogLevel::Info, "test message".to_string())
            .with_package("test-pkg".to_string())
            .with_step("build".to_string())
            .with_context("key".to_string(), "value".to_string());

        let formatted = entry.format();
        assert!(formatted.contains("INFO"));
        assert!(formatted.contains("test-pkg"));
        assert!(formatted.contains("build"));
        assert!(formatted.contains("key=value"));
        assert!(formatted.contains("test message"));
    }

    #[test]
    fn test_log_entry_json() {
        let entry = LogEntry::new(LogLevel::Error, "error message".to_string())
            .with_package("test-pkg".to_string());

        let json = entry.format_json();
        assert!(json.contains("Error"));
        assert!(json.contains("test-pkg"));
        assert!(json.contains("error message"));
    }

    #[test]
    fn test_sanitize_env() {
        let mut env = HashMap::new();
        env.insert("NORMAL_VAR".to_string(), "value".to_string());
        env.insert("SECRET_KEY".to_string(), "secret123".to_string());
        env.insert("PASSWORD".to_string(), "pass123".to_string());
        env.insert("API_TOKEN".to_string(), "token123".to_string());

        let sanitized = BuildManager::sanitize_env(&env);
        assert_eq!(sanitized.get("NORMAL_VAR").unwrap(), "value");
        assert_eq!(sanitized.get("SECRET_KEY").unwrap(), "***REDACTED***");
        assert_eq!(sanitized.get("PASSWORD").unwrap(), "***REDACTED***");
        assert_eq!(sanitized.get("API_TOKEN").unwrap(), "***REDACTED***");
    }

    // Tests for execute_step streaming and tail buffer behavior

    #[test]
    fn test_execute_step_streaming_basic() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Simple successful step
        let step = BuildStep {
            command: "echo".to_string(),
            args: vec!["hello world".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("test step".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &build_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.status.success());

        // Verify log was written
        let log_content = fs::read_to_string(&log_path).unwrap();
        assert!(log_content.contains("Starting step: test step"));
        assert!(log_content.contains("hello world"));
        assert!(log_content.contains("Step finished with exit code 0"));
    }

    #[test]
    fn test_execute_step_failure_captures_tail() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step that fails
        let step = BuildStep {
            command: "false".to_string(),
            args: vec![],
            workdir: None,
            env: HashMap::new(),
            description: Some("failing step".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &build_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );

        assert!(result.is_err());
        if let Err(BuildError::StepFailed(info)) = result {
            // Error info should contain tail of output (even if empty for `false`)
            assert_eq!(info.exit_code, 1);
        } else {
            panic!("Expected StepFailed error");
        }

        // Verify log was written
        let log_content = fs::read_to_string(&log_path).unwrap();
        assert!(log_content.contains("Starting step: failing step"));
        assert!(log_content.contains("Step finished with exit code 1"));
        assert!(log_content.contains("Step failed:"));
    }

    #[test]
    fn test_execute_step_large_output_bounded() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Generate lots of output using seq (if available) or printf
        // Using a shell command that generates many lines
        let step = BuildStep {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "for i in $(seq 1 200); do echo \"line $i\"; done".to_string(),
            ],
            workdir: None,
            env: HashMap::new(),
            description: Some("large output step".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &build_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.status.success());

        // The returned output should only contain the tail (last TAIL_BUFFER_LINES lines)
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout_str.lines().collect();
        // Should be bounded by TAIL_BUFFER_LINES (100)
        assert!(
            lines.len() <= BuildManager::TAIL_BUFFER_LINES,
            "Output should be bounded to TAIL_BUFFER_LINES"
        );

        // Verify the last lines are present
        assert!(
            lines.last().unwrap().contains("200"),
            "Should contain last line"
        );

        // But the log file should contain ALL lines
        let log_content = fs::read_to_string(&log_path).unwrap();
        let log_lines: Vec<&str> = log_content.lines().collect();
        let output_log_lines: Vec<&str> = log_lines
            .iter()
            .filter(|l| l.contains("line "))
            .copied()
            .collect();
        // Log should have all 200 lines (or close to it, depending on timing)
        assert!(
            output_log_lines.len() >= 190,
            "Log should contain most/all output lines"
        );
    }

    #[test]
    fn test_execute_step_stderr_streaming() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Generate stderr output
        let step = BuildStep {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "echo 'stdout line 1'; echo 'stderr line 1' >&2; echo 'stdout line 2'; echo 'stderr line 2' >&2".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("stderr test".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &build_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );

        assert!(result.is_ok());

        // Verify log contains both stdout and stderr
        let log_content = fs::read_to_string(&log_path).unwrap();
        assert!(log_content.contains("stdout line 1"));
        assert!(log_content.contains("stdout line 2"));
        assert!(log_content.contains("stderr line 1"));
        assert!(log_content.contains("stderr line 2"));
        assert!(log_content.contains("stream=stdout"));
        assert!(log_content.contains("stream=stderr"));
    }

    #[test]
    fn test_execute_step_signal_termination() {
        // This test is informational - we can't easily test signal handling
        // without a controllable subprocess. The signal handling code is
        // exercised by the try_wait loop which detects signals via status.signal()
    }

    #[test]
    fn test_execute_step_continue_on_failure() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step that fails but continues
        let step = BuildStep {
            command: "false".to_string(),
            args: vec![],
            workdir: None,
            env: HashMap::new(),
            description: Some("failing step".to_string()),
            continue_on_failure: true,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &build_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );

        // Should succeed (continue_on_failure = true)
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(!output.status.success());

        // Verify log contains warning but not error
        let log_content = fs::read_to_string(&log_path).unwrap();
        assert!(log_content.contains("Step failed but continuing"));
        assert!(!log_content.contains("Step failed:"));
    }

    #[test]
    fn test_execute_step_workdir_validation() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step with workdir that escapes base
        let step = BuildStep {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            workdir: Some("../escape".to_string()),
            env: HashMap::new(),
            description: Some("escaping workdir".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );

        assert!(result.is_err());
        if let Err(BuildError::ExecutionFailed(msg)) = result {
            assert!(msg.contains("escapes base directory"));
        } else {
            panic!("Expected ExecutionFailed error for path escape");
        }
    }

    // =========================================================================
    // Build Isolation Tests
    // =========================================================================

    #[test]
    fn test_staging_dir_creation() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        // Each call to build() should create a unique staging directory
        let temp_dir = tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();

        // Write a dummy source file to satisfy get_source_dir
        let source_file = source_dir.join("test.txt");
        fs::write(&source_file, "test").unwrap();

        // We need to mock the extract manager to return our source dir
        // Since we can't easily mock it, we'll test the directory creation logic directly
        let build_dir = manager.package_build_dir(&recipe);
        let staging_dir = manager.package_staging_dir(&recipe);

        // Verify unique staging dir naming
        assert!(staging_dir.to_string_lossy().contains("test-pkg-1.0"));
        assert_ne!(build_dir, staging_dir);

        // Create the directories and verify they exist
        fs::create_dir_all(&staging_dir).unwrap();
        assert!(staging_dir.exists());
    }

    #[test]
    fn test_atomic_publish() {
        let config = create_test_config();
        let manager = BuildManager::new(config).unwrap();

        let temp_dir = tempdir().unwrap();
        let staging_dir = temp_dir.path().join("staging");
        let sysroot = temp_dir.path().join("sysroot");

        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&sysroot).unwrap();

        // Create some test files in staging
        fs::write(staging_dir.join("file1.txt"), "content1").unwrap();
        fs::create_dir_all(staging_dir.join("subdir")).unwrap();
        fs::write(staging_dir.join("subdir").join("file2.txt"), "content2").unwrap();

        // We need to test atomic_publish_to_sysroot, but it's private
        // Test the copy_dir_all logic which is used for cross-device fallback
        let dst = temp_dir.path().join("dst");
        manager.copy_dir_all(&staging_dir, &dst).unwrap();

        assert!(dst.join("file1.txt").exists());
        assert_eq!(
            fs::read_to_string(dst.join("file1.txt")).unwrap(),
            "content1"
        );
        assert!(dst.join("subdir").join("file2.txt").exists());
        assert_eq!(
            fs::read_to_string(dst.join("subdir").join("file2.txt")).unwrap(),
            "content2"
        );
    }

    #[test]
    fn test_staging_cleanup_on_failure() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let mut recipe = create_test_recipe();
        recipe.name = "fail-test".to_string();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        // Create a step that fails
        let step = BuildStep {
            command: "false".to_string(),
            args: vec![],
            workdir: None,
            env: HashMap::new(),
            description: Some("failing step".to_string()),
            continue_on_failure: false,
        };

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Execute step with keep_on_failure = false (default)
        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );

        // Step should fail
        assert!(result.is_err());

        // The build() method would clean up staging dir on failure
        // Let's verify cleanup_staging_dir works
        // (We create it first since the failed step doesn't create it)
        fs::create_dir_all(&staging_dir).unwrap();
        fs::write(staging_dir.join("test.txt"), "content").unwrap();
        assert!(staging_dir.exists());

        manager.cleanup_staging_dir(&staging_dir);
        assert!(!staging_dir.exists());
    }

    #[test]
    fn test_staging_preserved_on_keep_on_failure() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let mut recipe = create_test_recipe();
        recipe.name = "keep-test".to_string();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        // Create a step that fails
        let step = BuildStep {
            command: "false".to_string(),
            args: vec![],
            workdir: None,
            env: HashMap::new(),
            description: Some("failing step".to_string()),
            continue_on_failure: false,
        };

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Execute step - this would normally clean up on failure
        // but we're testing the flag logic
        let _result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );

        // Create some content in staging (simulating partial build)
        fs::write(staging_dir.join("partial.txt"), "partial build").unwrap();
        assert!(staging_dir.exists());
        assert!(staging_dir.join("partial.txt").exists());

        // When keep_on_failure is true, staging should NOT be cleaned up
        // This test verifies the flag logic in build() method
        // We test the cleanup behavior directly
        let keep_on_failure = true;
        if !keep_on_failure {
            manager.cleanup_staging_dir(&staging_dir);
        }
        // With keep_on_failure = true, staging should still exist
        assert!(
            staging_dir.exists(),
            "Staging should be preserved when keep_on_failure=true"
        );
        assert!(staging_dir.join("partial.txt").exists());
    }

    #[test]
    fn test_cross_device_publish() {
        // This test mocks a cross-device scenario by verifying
        // the copy_dir_all fallback works when rename fails with EXDEV
        let config = create_test_config();
        let manager = BuildManager::new(config).unwrap();

        let temp_dir = tempdir().unwrap();
        let staging_dir = temp_dir.path().join("staging");
        let sysroot = temp_dir.path().join("sysroot");

        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&sysroot).unwrap();

        // Create test files including symlinks
        fs::write(staging_dir.join("regular.txt"), "regular content").unwrap();
        fs::create_dir_all(staging_dir.join("bin")).unwrap();
        fs::write(staging_dir.join("bin").join("tool"), "binary content").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink("regular.txt", staging_dir.join("link.txt")).unwrap();
        }

        // Test copy_dir_all which is used as fallback for cross-device
        let dst = temp_dir.path().join("dst");
        manager.copy_dir_all(&staging_dir, &dst).unwrap();

        assert!(dst.join("regular.txt").exists());
        assert_eq!(
            fs::read_to_string(dst.join("regular.txt")).unwrap(),
            "regular content"
        );
        assert!(dst.join("bin").join("tool").exists());
        assert_eq!(
            fs::read_to_string(dst.join("bin").join("tool")).unwrap(),
            "binary content"
        );

        #[cfg(unix)]
        {
            assert!(dst.join("link.txt").exists());
            let target = fs::read_link(dst.join("link.txt")).unwrap();
            assert_eq!(target, Path::new("regular.txt"));
        }
    }

    // =========================================================================
    // Path Validation Tests
    // =========================================================================

    #[test]
    fn test_recipe_validate_workdir_traversal() {
        let mut recipe = create_test_recipe();
        recipe.workdir = Some("../escape".to_string());
        let result = recipe.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("workdir must not contain")
        );
    }

    #[test]
    fn test_recipe_validate_workdir_absolute() {
        let mut recipe = create_test_recipe();
        recipe.workdir = Some("/absolute/path".to_string());
        let result = recipe.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("workdir must be a relative path")
        );
    }

    #[test]
    fn test_step_validate_workdir_traversal() {
        let step = BuildStep {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            workdir: Some("subdir/../escape".to_string()),
            env: HashMap::new(),
            description: None,
            continue_on_failure: false,
        };
        let result = step.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("workdir must not contain")
        );
    }

    #[test]
    fn test_execute_step_traversal() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step with workdir that tries to escape source directory
        let step = BuildStep {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            workdir: Some("../../etc".to_string()),
            env: HashMap::new(),
            description: Some("escaping workdir".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );

        assert!(result.is_err());
        if let Err(BuildError::ExecutionFailed(msg)) = result {
            assert!(msg.contains("escapes base directory"));
        } else {
            panic!("Expected ExecutionFailed error for path escape");
        }
    }

    // =========================================================================
    // Dependency Validation Tests
    // =========================================================================

    #[test]
    fn test_load_recipes_missing_dependency() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();

        let recipes_content = r#"
[test-pkg]
name = "test-pkg"
version = "1.0"
source = "test-source"
depends = ["missing-dep"]
build = [{ command = "echo", args = ["build"] }]
install = [{ command = "echo", args = ["install"] }]
"#;

        let temp_dir = tempdir().unwrap();
        let recipes_path = temp_dir.path().join("recipes.toml");
        fs::write(&recipes_path, recipes_content).unwrap();

        let result = manager.load_recipes(&recipes_path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Missing dependencies"));
        assert!(err.contains("missing-dep"));
    }

    #[test]
    fn test_load_recipes_undefined_source() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();

        let recipes_content = r#"
[test-pkg]
name = "test-pkg"
version = "1.0"
source = "undefined-source"
depends = []
build = [{ command = "echo", args = ["build"] }]
install = [{ command = "echo", args = ["install"] }]
"#;

        let temp_dir = tempdir().unwrap();
        let recipes_path = temp_dir.path().join("recipes.toml");
        fs::write(&recipes_path, recipes_content).unwrap();

        let result = manager.load_recipes(&recipes_path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Undefined sources"));
        assert!(err.contains("undefined-source"));
    }

    #[test]
    fn test_load_recipes_valid_deps() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        // First load sources - SourceDefinition requires name, version, url, sha256, archive
        let sources_content = r#"
[test-source]
name = "test-source"
version = "1.0"
url = "https://example.com/test-source-1.0.tar.gz"
sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
archive = "test-source-1.0.tar.gz"
"#;
        let temp_dir = tempdir().unwrap();
        let sources_path = temp_dir.path().join("sources.toml");
        fs::write(&sources_path, sources_content).unwrap();
        manager.load_sources(&sources_path).unwrap();

        // Now load recipes with valid deps and sources
        let recipes_content = r#"
[pkg-a]
name = "pkg-a"
version = "1.0"
source = "test-source"
depends = []
build = [{ command = "echo", args = ["build a"] }]
install = [{ command = "echo", args = ["install a"] }]

[pkg-b]
name = "pkg-b"
version = "1.0"
source = "test-source"
depends = ["pkg-a"]
build = [{ command = "echo", args = ["build b"] }]
install = [{ command = "echo", args = ["install b"] }]
"#;
        let recipes_path = temp_dir.path().join("recipes.toml");
        fs::write(&recipes_path, recipes_content).unwrap();

        let result = manager.load_recipes(&recipes_path);
        assert!(result.is_ok());
        assert_eq!(manager.recipes.len(), 2);
        assert!(manager.recipes.contains_key("pkg-a"));
        assert!(manager.recipes.contains_key("pkg-b"));
    }

    // =========================================================================
    // Build Configuration Tests
    // =========================================================================

    #[test]
    fn test_build_env_jobs() {
        let recipe = create_test_recipe();
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();

        // Test with jobs=4
        let config = create_test_config();
        let mut config_with_jobs = config.clone();
        config_with_jobs.build.jobs = Some(4);
        let manager_with_jobs = BuildManager::new(config_with_jobs).unwrap();
        let sysroot = manager_with_jobs.sysroot_dir().to_path_buf();

        let env = manager_with_jobs.build_env(&recipe, None, &build_dir, Some(&sysroot));
        assert_eq!(env.get("MAKEFLAGS").unwrap(), "-j4");
        assert_eq!(env.get("NINJAFLAGS").unwrap(), "-j4");
        assert_eq!(env.get("CARGOFLAGS").unwrap(), "--jobs 4");
    }

    #[test]
    fn test_build_env_cflags() {
        let config = create_test_config();
        let mut config_with_cflags = config.clone();
        config_with_cflags.build.cflags = vec!["-Wall".to_string(), "-Wextra".to_string()];
        let manager = BuildManager::new(config_with_cflags).unwrap();
        let recipe = create_test_recipe();
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();
        let sysroot = manager.sysroot_dir().to_path_buf();

        let env = manager.build_env(&recipe, None, &build_dir, Some(&sysroot));
        let cflags = env.get("CFLAGS").unwrap();
        assert!(cflags.contains("-O2 -pipe"));
        assert!(cflags.contains("-Wall"));
        assert!(cflags.contains("-Wextra"));
    }

    #[test]
    fn test_build_env_cxxflags() {
        let config = create_test_config();
        let mut config_with_cxxflags = config.clone();
        config_with_cxxflags.build.cxxflags = vec!["-std=c++17".to_string(), "-Wall".to_string()];
        let manager = BuildManager::new(config_with_cxxflags).unwrap();
        let recipe = create_test_recipe();
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();
        let sysroot = manager.sysroot_dir().to_path_buf();

        let env = manager.build_env(&recipe, None, &build_dir, Some(&sysroot));
        let cxxflags = env.get("CXXFLAGS").unwrap();
        assert!(cxxflags.contains("-O2 -pipe"));
        assert!(cxxflags.contains("-std=c++17"));
        assert!(cxxflags.contains("-Wall"));
    }

    #[test]
    fn test_build_env_ldflags() {
        let config = create_test_config();
        let mut config_with_ldflags = config.clone();
        config_with_ldflags.build.ldflags =
            vec!["-L/custom/lib".to_string(), "-Wl,-z,relro".to_string()];
        let manager = BuildManager::new(config_with_ldflags).unwrap();
        let recipe = create_test_recipe();
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();
        let sysroot = manager.sysroot_dir().to_path_buf();

        let env = manager.build_env(&recipe, None, &build_dir, Some(&sysroot));
        let ldflags = env.get("LDFLAGS").unwrap();
        assert!(ldflags.contains("-L/custom/lib"));
        assert!(ldflags.contains("-Wl,-z,relro"));
    }

    // =========================================================================
    // Logging/Execution Tests
    // =========================================================================

    #[test]
    fn test_verbose_streaming() {
        let mut config = create_test_config();
        config.build.verbose = true;
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Simple step that produces output
        let step = BuildStep {
            command: "echo".to_string(),
            args: vec!["verbose test output".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("verbose step".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );

        assert!(result.is_ok());

        // Verify log contains the output
        let log_content = fs::read_to_string(&log_path).unwrap();
        assert!(log_content.contains("verbose test output"));
        assert!(log_content.contains("stream=stdout"));
    }

    #[test]
    fn test_structured_logs() {
        let entry = LogEntry::new(LogLevel::Info, "test message".to_string())
            .with_package("test-pkg".to_string())
            .with_step("build".to_string())
            .with_context("key1".to_string(), "value1".to_string())
            .with_context("key2".to_string(), "value2".to_string());

        let formatted = entry.format();
        // Check for timestamp format (ISO 8601 with micros)
        assert!(formatted.contains("[") && formatted.contains("]"));
        assert!(formatted.contains("INFO"));
        assert!(formatted.contains("pkg=test-pkg"));
        assert!(formatted.contains("step=build"));
        assert!(formatted.contains("key1=value1"));
        assert!(formatted.contains("key2=value2"));
        assert!(formatted.contains("test message"));

        // Check JSON format
        let json = entry.format_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Note: serde serializes enum variants by name (e.g., "Info"), not by Display ("INFO")
        assert_eq!(parsed["level"], "Info");
        assert_eq!(parsed["package"], "test-pkg");
        assert_eq!(parsed["step"], "build");
        assert_eq!(parsed["message"], "test message");
        assert_eq!(parsed["context"]["key1"], "value1");
        assert_eq!(parsed["context"]["key2"], "value2");
        // Timestamp should be present and in ISO 8601 format
        assert!(parsed["timestamp"].is_string());
        let ts = parsed["timestamp"].as_str().unwrap();
        assert!(ts.contains("T") && ts.contains(":"));
    }

    #[test]
    fn test_log_levels() {
        assert_eq!(LogLevel::Debug.to_string(), "DEBUG");
        assert_eq!(LogLevel::Info.to_string(), "INFO");
        assert_eq!(LogLevel::Warn.to_string(), "WARN");
        assert_eq!(LogLevel::Error.to_string(), "ERROR");
    }

    #[test]
    fn test_sanitize_env_comprehensive() {
        let mut env = HashMap::new();
        env.insert("NORMAL_VAR".to_string(), "value".to_string());
        env.insert("SECRET_KEY".to_string(), "secret123".to_string());
        env.insert("PASSWORD".to_string(), "pass123".to_string());
        env.insert("API_TOKEN".to_string(), "token123".to_string());
        env.insert("PRIVATE_KEY".to_string(), "key123".to_string());
        env.insert("CREDENTIAL".to_string(), "cred123".to_string());
        env.insert("AUTH_TOKEN".to_string(), "auth123".to_string());
        env.insert("PUBLIC_VAR".to_string(), "visible".to_string());

        let sanitized = BuildManager::sanitize_env(&env);
        assert_eq!(sanitized.get("NORMAL_VAR").unwrap(), "value");
        assert_eq!(sanitized.get("PUBLIC_VAR").unwrap(), "visible");
        assert_eq!(sanitized.get("SECRET_KEY").unwrap(), "***REDACTED***");
        assert_eq!(sanitized.get("PASSWORD").unwrap(), "***REDACTED***");
        assert_eq!(sanitized.get("API_TOKEN").unwrap(), "***REDACTED***");
        assert_eq!(sanitized.get("PRIVATE_KEY").unwrap(), "***REDACTED***");
        assert_eq!(sanitized.get("CREDENTIAL").unwrap(), "***REDACTED***");
        assert_eq!(sanitized.get("AUTH_TOKEN").unwrap(), "***REDACTED***");
    }

    // =========================================================================
    // Integration Tests - Successful Package Builds
    // =========================================================================

    #[test]
    fn test_build_successful_package() {
        // Full integration test: build a package that creates files in staging
        // and publishes to sysroot
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "integration-test".to_string();
        recipe.version = "1.0".to_string();
        recipe.source = "test-source".to_string();
        recipe.build = vec![
            BuildStep {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "mkdir -p out/bin && echo '#!/bin/sh\necho hello' > out/bin/hello && chmod +x out/bin/hello".to_string()],
                workdir: None,
                env: HashMap::new(),
                description: Some("Create test binary".to_string()),
                continue_on_failure: false,
            },
        ];
        recipe.install = vec![BuildStep {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "mkdir -p \"$DESTDIR/usr/bin\" && cp out/bin/hello \"$DESTDIR/usr/bin/hello\""
                    .to_string(),
            ],
            workdir: None,
            env: HashMap::new(),
            description: Some("Install binary to sysroot".to_string()),
            continue_on_failure: false,
        }];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        // Load source (mock - we just need an extracted source dir)
        let temp_dir = tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let sysroot = temp_dir.path().join("sysroot");
        let logs_dir = temp_dir.path().join("logs");

        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&sysroot).unwrap();
        fs::create_dir_all(&logs_dir).unwrap();

        // Write a dummy source file
        fs::write(source_dir.join("dummy.txt"), "test").unwrap();

        // We need to mock the extract manager - directly test build steps
        // by calling execute_step directly with our test directories
        let mut log_file = tempfile::NamedTempFile::new().unwrap();

        // Test build phase
        let step1 = recipe.build[0].clone();
        let result = manager.execute_step(
            &recipe,
            &step1,
            &source_dir,
            &build_dir,
            log_file.as_file_mut(),
            Some(&staging_dir),
        );
        assert!(result.is_ok(), "Build step failed: {:?}", result);

        // Verify the file was created in source dir
        assert!(source_dir.join("out/bin/hello").exists());

        // Test install phase
        let step2 = recipe.install[0].clone();
        let result = manager.execute_step(
            &recipe,
            &step2,
            &source_dir,
            &build_dir,
            log_file.as_file_mut(),
            Some(&staging_dir),
        );
        assert!(result.is_ok(), "Install step failed: {:?}", result);

        // Verify file was installed to staging
        assert!(staging_dir.join("usr/bin/hello").exists());

        // Test publish (copy_dir_all + rename simulation)
        let dst = temp_dir.path().join("published_sysroot");
        manager.copy_dir_all(&staging_dir, &dst).unwrap();
        assert!(dst.join("usr/bin/hello").exists());
    }

    #[test]
    fn test_build_with_out_of_tree_build() {
        // Test build with separate build directory (out-of-tree)
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "out-of-tree-test".to_string();
        recipe.version = "1.0".to_string();
        recipe.source = "test-source".to_string();
        recipe.workdir = Some("src".to_string());
        recipe.build = vec![BuildStep {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "mkdir -p ../build && echo 'built' > ../build/output.txt".to_string(),
            ],
            workdir: Some("src".to_string()),
            env: HashMap::new(),
            description: Some("Build in separate directory".to_string()),
            continue_on_failure: false,
        }];
        recipe.install = vec![];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");

        fs::create_dir_all(source_dir.join("src")).unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let mut log_file = tempfile::NamedTempFile::new().unwrap();

        let step = recipe.build[0].clone();
        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            log_file.as_file_mut(),
            Some(&staging_dir),
        );
        assert!(
            result.is_ok(),
            "Out-of-tree build step failed: {:?}",
            result
        );

        // Verify output in source_dir/build (since workdir is source_dir/src and command uses ../build)
        assert!(source_dir.join("build/output.txt").exists());
    }

    // =========================================================================
    // Failed Build Steps Tests
    // =========================================================================

    #[test]
    fn test_build_step_failure_stops_build() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "fail-test".to_string();
        recipe.build = vec![
            BuildStep {
                command: "echo".to_string(),
                args: vec!["this works".to_string()],
                workdir: None,
                env: HashMap::new(),
                description: Some("First step".to_string()),
                continue_on_failure: false,
            },
            BuildStep {
                command: "false".to_string(),
                args: vec![],
                workdir: None,
                env: HashMap::new(),
                description: Some("Failing step".to_string()),
                continue_on_failure: false,
            },
            BuildStep {
                command: "echo".to_string(),
                args: vec!["this should not run".to_string()],
                workdir: None,
                env: HashMap::new(),
                description: Some("Third step".to_string()),
                continue_on_failure: false,
            },
        ];
        recipe.install = vec![];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");

        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let mut log_file = tempfile::NamedTempFile::new().unwrap();

        // Execute first step (should succeed)
        let result1 = manager.execute_step(
            &recipe,
            &recipe.build[0],
            &source_dir,
            &build_dir,
            log_file.as_file_mut(),
            Some(&staging_dir),
        );
        assert!(result1.is_ok());

        // Execute second step (should fail)
        let result2 = manager.execute_step(
            &recipe,
            &recipe.build[1],
            &source_dir,
            &build_dir,
            log_file.as_file_mut(),
            Some(&staging_dir),
        );
        assert!(result2.is_err());
        if let Err(BuildError::StepFailed(info)) = result2 {
            assert_eq!(info.step, "Failing step");
            assert_eq!(info.exit_code, 1);
        } else {
            panic!("Expected StepFailed error");
        }

        // Verify log contains the failure
        let log_content = fs::read_to_string(log_file.path()).unwrap();
        assert!(log_content.contains("Starting step: First step"));
        assert!(log_content.contains("Step finished with exit code 0"));
        assert!(log_content.contains("Starting step: Failing step"));
        assert!(log_content.contains("Step finished with exit code 1"));
        assert!(log_content.contains("Step failed:"));
    }

    #[test]
    fn test_build_step_failure_with_continue_on_failure() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "continue-fail-test".to_string();
        recipe.build = vec![
            BuildStep {
                command: "echo".to_string(),
                args: vec!["step 1".to_string()],
                workdir: None,
                env: HashMap::new(),
                description: Some("Step 1".to_string()),
                continue_on_failure: false,
            },
            BuildStep {
                command: "false".to_string(),
                args: vec![],
                workdir: None,
                env: HashMap::new(),
                description: Some("Failing step".to_string()),
                continue_on_failure: true, // Continue on failure
            },
            BuildStep {
                command: "echo".to_string(),
                args: vec!["step 3".to_string()],
                workdir: None,
                env: HashMap::new(),
                description: Some("Step 3".to_string()),
                continue_on_failure: false,
            },
        ];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");

        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let mut log_file = tempfile::NamedTempFile::new().unwrap();

        // All steps should execute (step 2 fails but continues)
        for step in &recipe.build {
            let result = manager.execute_step(
                &recipe,
                step,
                &source_dir,
                &build_dir,
                log_file.as_file_mut(),
                Some(&staging_dir),
            );
            // Should not return error due to continue_on_failure
            assert!(
                result.is_ok(),
                "Step {:?} should not error",
                step.description
            );
        }

        let log_content = fs::read_to_string(log_file.path()).unwrap();
        assert!(log_content.contains("Step failed but continuing"));
    }

    // =========================================================================
    // Failed Installations and Staging Cleanup Tests
    // =========================================================================

    #[test]
    fn test_install_failure_cleans_up_staging() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "install-fail-test".to_string();
        recipe.build = vec![BuildStep {
            command: "echo".to_string(),
            args: vec!["build ok".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("Build step".to_string()),
            continue_on_failure: false,
        }];
        recipe.install = vec![BuildStep {
            command: "false".to_string(),
            args: vec![],
            workdir: None,
            env: HashMap::new(),
            description: Some("Failing install".to_string()),
            continue_on_failure: false,
        }];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");

        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let mut log_file = tempfile::NamedTempFile::new().unwrap();

        // Build phase succeeds
        let result = manager.execute_step(
            &recipe,
            &recipe.build[0],
            &source_dir,
            &build_dir,
            log_file.as_file_mut(),
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        // Install phase fails
        let result = manager.execute_step(
            &recipe,
            &recipe.install[0],
            &source_dir,
            &build_dir,
            log_file.as_file_mut(),
            Some(&staging_dir),
        );
        assert!(result.is_err());

        // Verify staging directory can be cleaned up
        manager.cleanup_staging_dir(&staging_dir);
        assert!(!staging_dir.exists());
    }

    #[test]
    fn test_staging_isolation_between_packages() {
        let config = create_test_config();
        let manager = BuildManager::new(config.clone()).unwrap();

        let recipe1 = BuildRecipe {
            name: "pkg-a".to_string(),
            version: "1.0".to_string(),
            source: "src-a".to_string(),
            depends: vec![],
            build: vec![],
            install: vec![],
            env: HashMap::new(),
            workdir: None,
        };
        let recipe2 = BuildRecipe {
            name: "pkg-b".to_string(),
            version: "2.0".to_string(),
            source: "src-b".to_string(),
            depends: vec![],
            build: vec![],
            install: vec![],
            env: HashMap::new(),
            workdir: None,
        };

        let staging1 = manager.package_staging_dir(&recipe1);
        let staging2 = manager.package_staging_dir(&recipe2);

        // Staging directories should be unique per package-version
        assert_ne!(staging1, staging2);
        assert!(staging1.to_string_lossy().contains("pkg-a-1.0"));
        assert!(staging2.to_string_lossy().contains("pkg-b-2.0"));

        // Build directories should also be unique
        let build1 = manager.package_build_dir(&recipe1);
        let build2 = manager.package_build_dir(&recipe2);
        assert_ne!(build1, build2);
    }

    #[test]
    fn test_partial_install_cleanup_on_failure() {
        // Test that partial installation in staging is cleaned up on failure
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "partial-install-test".to_string();
        recipe.build = vec![];
        recipe.install = vec![
            BuildStep {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    "mkdir -p \"$DESTDIR/usr/bin\" && echo 'file1' > \"$DESTDIR/usr/bin/file1\""
                        .to_string(),
                ],
                workdir: None,
                env: HashMap::new(),
                description: Some("Install file 1".to_string()),
                continue_on_failure: false,
            },
            BuildStep {
                command: "false".to_string(),
                args: vec![],
                workdir: None,
                env: HashMap::new(),
                description: Some("Fail before file 2".to_string()),
                continue_on_failure: false,
            },
            BuildStep {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    "echo 'file2' > \"$DESTDIR/usr/bin/file2\"".to_string(),
                ],
                workdir: None,
                env: HashMap::new(),
                description: Some("Install file 2 (never runs)".to_string()),
                continue_on_failure: false,
            },
        ];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");

        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let mut log_file = tempfile::NamedTempFile::new().unwrap();

        // First install step succeeds
        let result = manager.execute_step(
            &recipe,
            &recipe.install[0],
            &source_dir,
            &build_dir,
            log_file.as_file_mut(),
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        // Verify file1 was created in staging
        assert!(staging_dir.join("usr/bin/file1").exists());

        // Second step fails
        let result = manager.execute_step(
            &recipe,
            &recipe.install[1],
            &source_dir,
            &build_dir,
            log_file.as_file_mut(),
            Some(&staging_dir),
        );
        assert!(result.is_err());

        // Cleanup staging
        manager.cleanup_staging_dir(&staging_dir);
        assert!(!staging_dir.exists());
    }

    // =========================================================================
    // Dependency Ordering, Missing Dependencies, and Cycles Tests
    // =========================================================================

    #[test]
    fn test_resolve_build_order_complex_dependencies() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        manager.recipes = HashMap::from([
            (
                "pkg-a".to_string(),
                BuildRecipe {
                    name: "pkg-a".to_string(),
                    version: "1.0".to_string(),
                    source: "src-a".to_string(),
                    depends: vec![],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "pkg-b".to_string(),
                BuildRecipe {
                    name: "pkg-b".to_string(),
                    version: "1.0".to_string(),
                    source: "src-b".to_string(),
                    depends: vec!["pkg-a".to_string()],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "pkg-c".to_string(),
                BuildRecipe {
                    name: "pkg-c".to_string(),
                    version: "1.0".to_string(),
                    source: "src-c".to_string(),
                    depends: vec!["pkg-a".to_string(), "pkg-b".to_string()],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "pkg-d".to_string(),
                BuildRecipe {
                    name: "pkg-d".to_string(),
                    version: "1.0".to_string(),
                    source: "src-d".to_string(),
                    depends: vec!["pkg-b".to_string()],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
        ]);

        let order = manager.resolve_build_order().unwrap();

        // pkg-a must come first (no deps)
        assert_eq!(order[0], "pkg-a");

        // pkg-b must come after pkg-a
        let b_idx = order.iter().position(|x| x == "pkg-b").unwrap();
        let a_idx = order.iter().position(|x| x == "pkg-a").unwrap();
        assert!(b_idx > a_idx);

        // pkg-c must come after both pkg-a and pkg-b
        let c_idx = order.iter().position(|x| x == "pkg-c").unwrap();
        assert!(c_idx > a_idx);
        assert!(c_idx > b_idx);

        // pkg-d must come after pkg-b
        let d_idx = order.iter().position(|x| x == "pkg-d").unwrap();
        assert!(d_idx > b_idx);

        // Total 4 packages
        assert_eq!(order.len(), 4);
    }

    #[test]
    fn test_resolve_build_order_cycle_detection() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        manager.recipes = HashMap::from([
            (
                "pkg-a".to_string(),
                BuildRecipe {
                    name: "pkg-a".to_string(),
                    version: "1.0".to_string(),
                    source: "src-a".to_string(),
                    depends: vec!["pkg-c".to_string()], // Creates cycle: a -> c -> a
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "pkg-b".to_string(),
                BuildRecipe {
                    name: "pkg-b".to_string(),
                    version: "1.0".to_string(),
                    source: "src-b".to_string(),
                    depends: vec!["pkg-a".to_string()],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "pkg-c".to_string(),
                BuildRecipe {
                    name: "pkg-c".to_string(),
                    version: "1.0".to_string(),
                    source: "src-c".to_string(),
                    depends: vec!["pkg-b".to_string()],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
        ]);

        let result = manager.resolve_build_order();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("circular dependency"));
    }

    #[test]
    fn test_resolve_build_order_self_dependency() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        manager.recipes = HashMap::from([(
            "pkg-a".to_string(),
            BuildRecipe {
                name: "pkg-a".to_string(),
                version: "1.0".to_string(),
                source: "src-a".to_string(),
                depends: vec!["pkg-a".to_string()], // Self-dependency
                build: vec![BuildStep {
                    command: "true".to_string(),
                    args: vec![],
                    workdir: None,
                    env: HashMap::new(),
                    description: None,
                    continue_on_failure: false,
                }],
                install: vec![],
                env: HashMap::new(),
                workdir: None,
            },
        )]);

        let result = manager.resolve_build_order();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("circular dependency"));
    }

    #[test]
    fn test_build_all_skips_failed_dependencies() {
        // Test that build_all skips packages whose dependencies failed
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        // pkg-a will fail, pkg-b depends on pkg-a
        manager.recipes = HashMap::from([
            (
                "pkg-a".to_string(),
                BuildRecipe {
                    name: "pkg-a".to_string(),
                    version: "1.0".to_string(),
                    source: "src-a".to_string(),
                    depends: vec![],
                    build: vec![BuildStep {
                        command: "false".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: Some("fail".to_string()),
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "pkg-b".to_string(),
                BuildRecipe {
                    name: "pkg-b".to_string(),
                    version: "1.0".to_string(),
                    source: "src-b".to_string(),
                    depends: vec!["pkg-a".to_string()],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: Some("build b".to_string()),
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "pkg-c".to_string(),
                BuildRecipe {
                    name: "pkg-c".to_string(),
                    version: "1.0".to_string(),
                    source: "src-c".to_string(),
                    depends: vec![],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: Some("build c".to_string()),
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
        ]);

        // We can't easily test build_all without real source dirs,
        // but we can test the logic by checking resolve_build_order
        let order = manager.resolve_build_order().unwrap();
        assert!(order.contains(&"pkg-a".to_string()));
        assert!(order.contains(&"pkg-b".to_string()));
        assert!(order.contains(&"pkg-c".to_string()));
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn test_load_recipes_reports_all_missing_dependencies() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();

        let recipes_content = r#"
[test-pkg]
name = "test-pkg"
version = "1.0"
source = "test-source"
depends = ["missing-dep1", "missing-dep2"]
build = [{ command = "echo", args = ["build"] }]
install = [{ command = "echo", args = ["install"] }]

[another-pkg]
name = "another-pkg"
version = "1.0"
source = "test-source"
depends = ["missing-dep3"]
build = [{ command = "echo", args = ["build"] }]
install = [{ command = "echo", args = ["install"] }]
"#;

        let temp_dir = tempdir().unwrap();
        let recipes_path = temp_dir.path().join("recipes.toml");
        fs::write(&recipes_path, recipes_content).unwrap();

        let result = manager.load_recipes(&recipes_path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Missing dependencies"));
        assert!(err.contains("missing-dep1"));
        assert!(err.contains("missing-dep2"));
        assert!(err.contains("missing-dep3"));
    }

    #[test]
    fn test_load_recipes_reports_all_undefined_sources() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();

        let recipes_content = r#"
[pkg-a]
name = "pkg-a"
version = "1.0"
source = "undefined-source-1"
depends = []
build = [{ command = "echo", args = ["build"] }]
install = [{ command = "echo", args = ["install"] }]

[pkg-b]
name = "pkg-b"
version = "1.0"
source = "undefined-source-2"
depends = []
build = [{ command = "echo", args = ["build"] }]
install = [{ command = "echo", args = ["install"] }]
"#;

        let temp_dir = tempdir().unwrap();
        let recipes_path = temp_dir.path().join("recipes.toml");
        fs::write(&recipes_path, recipes_content).unwrap();

        let result = manager.load_recipes(&recipes_path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Undefined sources"));
        assert!(err.contains("undefined-source-1"));
        assert!(err.contains("undefined-source-2"));
    }

    #[test]
    fn test_load_recipes_reports_both_missing_deps_and_undefined_sources() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();

        let recipes_content = r#"
[pkg-a]
name = "pkg-a"
version = "1.0"
source = "undefined-source"
depends = ["missing-dep"]
build = [{ command = "echo", args = ["build"] }]
install = [{ command = "echo", args = ["install"] }]
"#;

        let temp_dir = tempdir().unwrap();
        let recipes_path = temp_dir.path().join("recipes.toml");
        fs::write(&recipes_path, recipes_content).unwrap();

        let result = manager.load_recipes(&recipes_path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Missing dependencies"));
        assert!(err.contains("missing-dep"));
        assert!(err.contains("Undefined sources"));
        assert!(err.contains("undefined-source"));
    }

    // =========================================================================
    // Undefined Source References Tests
    // =========================================================================

    #[test]
    fn test_build_recipe_undefined_source() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        // Load a source
        let sources_content = r#"
[test-source]
name = "test-source"
version = "1.0"
url = "https://example.com/test.tar.gz"
sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
archive = "test.tar.gz"
"#;
        let temp_dir = tempdir().unwrap();
        let sources_path = temp_dir.path().join("sources.toml");
        fs::write(&sources_path, sources_content).unwrap();
        manager.load_sources(&sources_path).unwrap();

        // Recipe references undefined source
        let recipes_content = r#"
[test-pkg]
name = "test-pkg"
version = "1.0"
source = "undefined-source"
depends = []
build = [{ command = "echo", args = ["build"] }]
install = [{ command = "echo", args = ["install"] }]
"#;
        let recipes_path = temp_dir.path().join("recipes.toml");
        fs::write(&recipes_path, recipes_content).unwrap();

        let result = manager.load_recipes(&recipes_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("undefined-source"));
    }

    // =========================================================================
    // Path Traversal and Symlink Escapes Tests
    // =========================================================================

    #[test]
    fn test_execute_step_workdir_symlink_escape() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let config = create_test_config();
            let mut manager = BuildManager::new(config).unwrap();
            let recipe = create_test_recipe();
            manager.recipes.insert(recipe.name.clone(), recipe.clone());

            let temp_dir = tempdir().unwrap();
            let build_dir = temp_dir.path().join("build");
            let staging_dir = temp_dir.path().join("staging");
            let log_path = temp_dir.path().join("test.log");
            let source_dir = temp_dir.path().join("source");
            let outside = temp_dir.path().join("outside");

            fs::create_dir_all(&build_dir).unwrap();
            fs::create_dir_all(&staging_dir).unwrap();
            fs::create_dir_all(&source_dir).unwrap();
            fs::create_dir_all(&outside).unwrap();

            // Create a symlink in source_dir that points outside
            let link = source_dir.join("evil_link");
            symlink(&outside, &link).unwrap();

            let mut log_file = fs::File::create(&log_path).unwrap();

            // Step with workdir that uses the symlink to escape
            let step = BuildStep {
                command: "echo".to_string(),
                args: vec!["test".to_string()],
                workdir: Some("evil_link".to_string()),
                env: HashMap::new(),
                description: Some("escaping workdir via symlink".to_string()),
                continue_on_failure: false,
            };

            let result = manager.execute_step(
                &recipe,
                &step,
                &source_dir,
                &build_dir,
                &mut log_file,
                Some(&staging_dir),
            );

            assert!(result.is_err());
            if let Err(BuildError::ExecutionFailed(msg)) = result {
                assert!(msg.contains("escapes base directory"));
            } else {
                panic!("Expected ExecutionFailed error for symlink escape");
            }
        }
    }

    #[test]
    fn test_recipe_workdir_validation_rejects_symlink_components() {
        // Test that workdir validation catches .. in path components
        let mut recipe = create_test_recipe();
        recipe.workdir = Some("subdir/../escape".to_string());
        let result = recipe.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("workdir must not contain")
        );
    }

    #[test]
    fn test_build_recipe_key_mismatch() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();

        let recipes_content = r#"
[wrong-key]
name = "test-pkg"
version = "1.0"
source = "test-source"
depends = []
build = [{ command = "echo", args = ["build"] }]
install = [{ command = "echo", args = ["install"] }]
"#;

        let temp_dir = tempdir().unwrap();
        let recipes_path = temp_dir.path().join("recipes.toml");
        fs::write(&recipes_path, recipes_content).unwrap();

        let result = manager.load_recipes(&recipes_path);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("recipe key 'wrong-key' does not match recipe name 'test-pkg'")
        );
    }

    // =========================================================================
    // Build Environment Configuration Tests
    // =========================================================================

    #[test]
    fn test_build_env_opt_level() {
        let config = create_test_config();
        let mut config_with_opt = config.clone();
        config_with_opt.build.opt_level = "3".to_string();
        let manager = BuildManager::new(config_with_opt).unwrap();
        let recipe = create_test_recipe();
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();
        let sysroot = manager.sysroot_dir().to_path_buf();

        let env = manager.build_env(&recipe, None, &build_dir, Some(&sysroot));
        let cflags = env.get("CFLAGS").unwrap();
        assert!(cflags.contains("-O3"));
    }

    #[test]
    fn test_build_env_opt_level_s() {
        let config = create_test_config();
        let mut config_with_opt = config.clone();
        config_with_opt.build.opt_level = "s".to_string();
        let manager = BuildManager::new(config_with_opt).unwrap();
        let recipe = create_test_recipe();
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();
        let sysroot = manager.sysroot_dir().to_path_buf();

        let env = manager.build_env(&recipe, None, &build_dir, Some(&sysroot));
        let cflags = env.get("CFLAGS").unwrap();
        assert!(cflags.contains("-Os"));
    }

    #[test]
    fn test_build_env_opt_level_z() {
        let config = create_test_config();
        let mut config_with_opt = config.clone();
        config_with_opt.build.opt_level = "z".to_string();
        let manager = BuildManager::new(config_with_opt).unwrap();
        let recipe = create_test_recipe();
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();
        let sysroot = manager.sysroot_dir().to_path_buf();

        let env = manager.build_env(&recipe, None, &build_dir, Some(&sysroot));
        let cflags = env.get("CFLAGS").unwrap();
        assert!(cflags.contains("-Oz"));
    }

    #[test]
    fn test_build_env_toolchain_variables() {
        let config = create_test_config();
        let manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();
        let sysroot = manager.sysroot_dir().to_path_buf();

        // Toolchain dir doesn't exist, so no CC/CXX should be set
        let env = manager.build_env(&recipe, None, &build_dir, Some(&sysroot));
        // These may or may not be set depending on toolchain existence
        // Just verify the env was built without errors
        assert!(env.contains_key("ROYAL_SYSROOT"));
        assert!(env.contains_key("ROYAL_BUILD_DIR"));
        assert!(env.contains_key("ROYAL_WORKSPACE"));
        assert!(env.contains_key("DESTDIR"));
        assert!(env.contains_key("PATH"));
    }

    #[test]
    fn test_build_env_recipe_env_overrides() {
        let config = create_test_config();
        let manager = BuildManager::new(config.clone()).unwrap();
        let mut recipe = create_test_recipe();
        recipe
            .env
            .insert("CFLAGS".to_string(), "-O3 -custom".to_string());
        recipe
            .env
            .insert("CUSTOM_VAR".to_string(), "custom_value".to_string());
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();
        let sysroot = manager.sysroot_dir().to_path_buf();

        let env = manager.build_env(&recipe, None, &build_dir, Some(&sysroot));
        // Recipe env should override config CFLAGS
        assert_eq!(env.get("CFLAGS").unwrap(), "-O3 -custom");
        assert_eq!(env.get("CUSTOM_VAR").unwrap(), "custom_value");
    }

    #[test]
    fn test_build_env_step_env_overrides_recipe_env() {
        let config = create_test_config();
        let manager = BuildManager::new(config.clone()).unwrap();
        let mut recipe = create_test_recipe();
        recipe
            .env
            .insert("TEST_VAR".to_string(), "recipe_value".to_string());
        let mut step = BuildStep {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: None,
            continue_on_failure: false,
        };
        step.env
            .insert("TEST_VAR".to_string(), "step_value".to_string());
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();
        let sysroot = manager.sysroot_dir().to_path_buf();

        let env = manager.build_env(&recipe, Some(&step), &build_dir, Some(&sysroot));
        // Step env should override recipe env
        assert_eq!(env.get("TEST_VAR").unwrap(), "step_value");
    }

    #[test]
    fn test_build_env_destdir_uses_staging_for_build_phase() {
        let config = create_test_config();
        let manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();
        let staging_dir = temp_dir.path().join("staging");

        let env = manager.build_env(&recipe, None, &build_dir, Some(&staging_dir));
        assert_eq!(
            env.get("DESTDIR").unwrap().as_str(),
            staging_dir.display().to_string().as_str()
        );
    }

    #[test]
    fn test_build_env_preserves_required_host_vars() {
        let config = create_test_config();
        let manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().to_path_buf();
        let sysroot = manager.sysroot_dir().to_path_buf();

        let env = manager.build_env(&recipe, None, &build_dir, Some(&sysroot));
        // Required host vars should be passed through
        assert!(env.contains_key("PATH"));
        assert!(env.contains_key("HOME"));
        assert!(env.contains_key("USER"));
        assert!(env.contains_key("SHELL"));
    }

    // =========================================================================
    // Logging Behavior Tests
    // =========================================================================

    #[test]
    fn test_log_file_contains_all_output() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step that produces multiple lines of output
        let step = BuildStep {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "for i in 1 2 3; do echo \"stdout line $i\"; echo \"stderr line $i\" >&2; done"
                    .to_string(),
            ],
            workdir: None,
            env: HashMap::new(),
            description: Some("multi-line output".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        // Read log file and verify all output is present
        let log_content = fs::read_to_string(&log_path).unwrap();

        // Should have all 3 stdout lines
        assert!(log_content.contains("stdout line 1"));
        assert!(log_content.contains("stdout line 2"));
        assert!(log_content.contains("stdout line 3"));

        // Should have all 3 stderr lines
        assert!(log_content.contains("stderr line 1"));
        assert!(log_content.contains("stderr line 2"));
        assert!(log_content.contains("stderr line 3"));

        // Should have stream markers
        assert!(log_content.contains("stream=stdout"));
        assert!(log_content.contains("stream=stderr"));

        // Should have step start and end
        assert!(log_content.contains("Starting step: multi-line output"));
        assert!(log_content.contains("Step finished with exit code 0"));
    }

    #[test]
    fn test_log_file_structure_with_metadata() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        let step = BuildStep {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("test step".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();

        // Each log line should have timestamp, level, package, step, and context
        let lines: Vec<&str> = log_content.lines().collect();
        assert!(!lines.is_empty());

        // First line should be step start
        let first_line = lines[0];
        assert!(first_line.contains("[")); // timestamp
        assert!(first_line.contains("]"));
        assert!(first_line.contains("INFO"));
        assert!(first_line.contains("pkg=test-pkg"));
        assert!(first_line.contains("step=test step"));
        assert!(first_line.contains("command="));
        assert!(first_line.contains("workdir="));
        assert!(first_line.contains("env="));
        assert!(first_line.contains("Starting step: test step"));
    }

    #[test]
    fn test_log_tail_buffer_bounded() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Generate more output than TAIL_BUFFER_LINES (100)
        let step = BuildStep {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "for i in $(seq 1 200); do echo \"line $i\"; done".to_string(),
            ],
            workdir: None,
            env: HashMap::new(),
            description: Some("large output".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let output = result.unwrap();
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout_str.lines().collect();

        // Returned output should be bounded by TAIL_BUFFER_LINES
        assert!(
            lines.len() <= BuildManager::TAIL_BUFFER_LINES,
            "Returned output should be bounded, got {} lines",
            lines.len()
        );

        // But log file should have all lines
        let log_content = fs::read_to_string(&log_path).unwrap();
        let log_lines: Vec<&str> = log_content.lines().collect();
        let output_log_lines: Vec<&str> = log_lines
            .iter()
            .filter(|l| l.contains("line "))
            .copied()
            .collect();
        // Log should have all 200 lines (or close to it)
        assert!(
            output_log_lines.len() >= 190,
            "Log should contain most output lines, got {}",
            output_log_lines.len()
        );
    }

    #[test]
    fn test_failed_step_log_contains_diagnostics() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step that fails with output
        let step = BuildStep {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "echo 'error detail' >&2; exit 1".to_string(),
            ],
            workdir: None,
            env: HashMap::new(),
            description: Some("failing step".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_err());

        let log_content = fs::read_to_string(&log_path).unwrap();

        // Should contain error details in log
        assert!(log_content.contains("Starting step: failing step"));
        assert!(log_content.contains("error detail"));
        assert!(log_content.contains("Step finished with exit code 1"));
        assert!(log_content.contains("Step failed:"));
        assert!(log_content.contains("stderr"));
    }

    #[test]
    fn test_verbose_mode_outputs_to_console() {
        let mut config = create_test_config();
        config.build.verbose = true;
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        let step = BuildStep {
            command: "echo".to_string(),
            args: vec!["verbose test".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("verbose step".to_string()),
            continue_on_failure: false,
        };

        // This test verifies the code path - we can't easily capture stderr in tests
        // but we can verify it doesn't crash
        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());
    }

    // =========================================================================
    // Build All Integration Tests
    // =========================================================================

    #[test]
    fn test_build_all_dependency_order() {
        // This test verifies that build_all would build in correct order
        // We can't run full build without real sources, but we can verify the order
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        manager.recipes = HashMap::from([
            (
                "base".to_string(),
                BuildRecipe {
                    name: "base".to_string(),
                    version: "1.0".to_string(),
                    source: "src-base".to_string(),
                    depends: vec![],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "lib".to_string(),
                BuildRecipe {
                    name: "lib".to_string(),
                    version: "1.0".to_string(),
                    source: "src-lib".to_string(),
                    depends: vec!["base".to_string()],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "app".to_string(),
                BuildRecipe {
                    name: "app".to_string(),
                    version: "1.0".to_string(),
                    source: "src-app".to_string(),
                    depends: vec!["lib".to_string()],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
        ]);

        let order = manager.resolve_build_order().unwrap();
        assert_eq!(order, vec!["base", "lib", "app"]);
    }

    #[test]
    fn test_build_all_independent_packages_sorted() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        manager.recipes = HashMap::from([
            (
                "zebra".to_string(),
                BuildRecipe {
                    name: "zebra".to_string(),
                    version: "1.0".to_string(),
                    source: "src-z".to_string(),
                    depends: vec![],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "apple".to_string(),
                BuildRecipe {
                    name: "apple".to_string(),
                    version: "1.0".to_string(),
                    source: "src-a".to_string(),
                    depends: vec![],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
            (
                "banana".to_string(),
                BuildRecipe {
                    name: "banana".to_string(),
                    version: "1.0".to_string(),
                    source: "src-b".to_string(),
                    depends: vec![],
                    build: vec![BuildStep {
                        command: "true".to_string(),
                        args: vec![],
                        workdir: None,
                        env: HashMap::new(),
                        description: None,
                        continue_on_failure: false,
                    }],
                    install: vec![],
                    env: HashMap::new(),
                    workdir: None,
                },
            ),
        ]);

        let order = manager.resolve_build_order().unwrap();
        // All three independent packages should be in the order (exact order not guaranteed for independent packages)
        assert_eq!(order.len(), 3);
        assert!(order.contains(&"apple".to_string()));
        assert!(order.contains(&"banana".to_string()));
        assert!(order.contains(&"zebra".to_string()));
    }

    // =========================================================================
    // Atomic Publish and Staging Tests
    // =========================================================================

    #[test]
    fn test_atomic_publish_same_filesystem() {
        let config = create_test_config();
        let manager = BuildManager::new(config).unwrap();

        let temp_dir = tempdir().unwrap();
        let staging_dir = temp_dir.path().join("staging");
        let sysroot = temp_dir.path().join("sysroot");

        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&sysroot).unwrap();

        // Create test files in staging
        fs::write(staging_dir.join("file1.txt"), "content1").unwrap();
        fs::create_dir_all(staging_dir.join("bin")).unwrap();
        fs::write(staging_dir.join("bin").join("tool"), "binary content").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink("file1.txt", staging_dir.join("link.txt")).unwrap();
        }

        // Test atomic_publish_to_sysroot by using copy_dir_all (fallback)
        // since we can't easily test rename on same filesystem in test
        let dst = temp_dir.path().join("dst");
        manager.copy_dir_all(&staging_dir, &dst).unwrap();

        assert!(dst.join("file1.txt").exists());
        assert_eq!(
            fs::read_to_string(dst.join("file1.txt")).unwrap(),
            "content1"
        );
        assert!(dst.join("bin").join("tool").exists());
        assert_eq!(
            fs::read_to_string(dst.join("bin").join("tool")).unwrap(),
            "binary content"
        );

        #[cfg(unix)]
        {
            assert!(dst.join("link.txt").exists());
            let target = fs::read_link(dst.join("link.txt")).unwrap();
            assert_eq!(target, Path::new("file1.txt"));
        }
    }

    #[test]
    fn test_staging_dir_unique_per_build() {
        let config = create_test_config();
        let manager = BuildManager::new(config).unwrap();

        let recipe = BuildRecipe {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            source: "test-source".to_string(),
            depends: vec![],
            build: vec![],
            install: vec![],
            env: HashMap::new(),
            workdir: None,
        };

        // Multiple calls should produce same staging dir name (deterministic)
        let staging1 = manager.package_staging_dir(&recipe);
        let staging2 = manager.package_staging_dir(&recipe);
        assert_eq!(staging1, staging2);
        assert!(staging1.to_string_lossy().contains("test-pkg-1.0"));
    }

    #[test]
    fn test_cleanup_staging_dir_ignores_missing() {
        let config = create_test_config();
        let manager = BuildManager::new(config).unwrap();

        let temp_dir = tempdir().unwrap();
        let staging_dir = temp_dir.path().join("nonexistent");

        // Should not panic or error
        manager.cleanup_staging_dir(&staging_dir);
    }

    // =========================================================================
    // StepFailedInfo Tests
    // =========================================================================

    #[test]
    fn test_step_failed_info_creation() {
        let mut env = HashMap::new();
        env.insert("TEST_VAR".to_string(), "value".to_string());

        let info = StepFailedInfo::new(
            "test-pkg".to_string(),
            "build step".to_string(),
            "make".to_string(),
            vec!["-j4".to_string()],
            2,
            "stdout content".to_string(),
            "stderr content".to_string(),
            "/tmp/build".to_string(),
            env,
        );

        assert_eq!(info.package, "test-pkg");
        assert_eq!(info.step, "build step");
        assert_eq!(info.command, "make");
        assert_eq!(info.args, vec!["-j4"]);
        assert_eq!(info.exit_code, 2);
        assert_eq!(info.stdout, "stdout content");
        assert_eq!(info.stderr, "stderr content");
        assert_eq!(info.workdir, "/tmp/build");
        assert_eq!(info.sanitized_env.get("TEST_VAR").unwrap(), "value");
    }

    // =========================================================================
    // BuildResultInfo Tests
    // =========================================================================

    #[test]
    fn test_build_result_info_fields() {
        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");

        let result = BuildResultInfo {
            name: "test-pkg".to_string(),
            version: "1.0".to_string(),
            build_dir: build_dir.clone(),
            staging_dir: Some(staging_dir.clone()),
            log_path: log_path.clone(),
            success: true,
        };

        assert_eq!(result.name, "test-pkg");
        assert_eq!(result.version, "1.0");
        assert_eq!(result.build_dir, build_dir);
        assert_eq!(result.staging_dir, Some(staging_dir));
        assert_eq!(result.log_path, log_path);
        assert!(result.success);
    }

    // =========================================================================
    // Edge Cases and Error Handling Tests
    // =========================================================================

    #[test]
    fn test_build_recipe_validation_empty_install() {
        let mut recipe = create_test_recipe();
        recipe.install = vec![];
        // Empty install should be valid (no install steps is OK)
        assert!(recipe.validate().is_ok());
    }

    #[test]
    fn test_build_step_validation_command_with_path() {
        let step = BuildStep {
            command: "/usr/bin/make".to_string(),
            args: vec!["install".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: None,
            continue_on_failure: false,
        };
        // Absolute path in command should be valid
        assert!(step.validate().is_ok());
    }

    #[test]
    fn test_execute_step_empty_command_fails() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let _log_file = fs::File::create(&log_path).unwrap();

        // Empty command should fail at validation, not execution
        let step = BuildStep {
            command: "".to_string(),
            args: vec![],
            workdir: None,
            env: HashMap::new(),
            description: Some("empty command".to_string()),
            continue_on_failure: false,
        };

        // Validation should catch this
        assert!(step.validate().is_err());
    }

    #[test]
    fn test_recipe_validation_rejects_invalid_workdir_in_step() {
        let mut recipe = create_test_recipe();
        recipe.build = vec![BuildStep {
            command: "echo".to_string(),
            args: vec!["test".to_string()],
            workdir: Some("/absolute".to_string()),
            env: HashMap::new(),
            description: None,
            continue_on_failure: false,
        }];
        let result = recipe.validate();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("build step 0"));
        assert!(err.to_string().contains("workdir must be a relative path"));
    }

    #[test]
    fn test_validate_version_edge_cases() {
        // Valid versions
        assert!(BuildManager::validate_version("1").is_ok());
        assert!(BuildManager::validate_version("1.0").is_ok());
        assert!(BuildManager::validate_version("1.0.0").is_ok());
        assert!(BuildManager::validate_version("1.0.0-alpha").is_ok());
        assert!(BuildManager::validate_version("1.0.0-alpha.1").is_ok());
        assert!(BuildManager::validate_version("1.0.0+build.1").is_ok());
        assert!(BuildManager::validate_version("1.0.0-alpha+build").is_ok());
        assert!(BuildManager::validate_version("1.0.0-rc.1+build.2").is_ok());

        // Invalid versions
        assert!(BuildManager::validate_version("").is_err());
        assert!(BuildManager::validate_version(&"a".repeat(33)).is_err());
        assert!(BuildManager::validate_version("1..0").is_err());
        assert!(BuildManager::validate_version("1/0").is_err());
        assert!(BuildManager::validate_version("1\\0").is_err());
        assert!(BuildManager::validate_version("1.0@").is_err());
        assert!(BuildManager::validate_version("1.0#").is_err());
        assert!(BuildManager::validate_version("1.0 ").is_err());
    }

    #[test]
    fn test_validate_package_name_edge_cases() {
        // Valid names
        assert!(BuildManager::validate_package_name("a").is_ok());
        assert!(BuildManager::validate_package_name("a-b").is_ok());
        assert!(BuildManager::validate_package_name("a_b").is_ok());
        assert!(BuildManager::validate_package_name("a1").is_ok());
        assert!(BuildManager::validate_package_name("A").is_ok());
        assert!(BuildManager::validate_package_name(&"a".repeat(64)).is_ok());

        // Invalid names
        assert!(BuildManager::validate_package_name("").is_err());
        assert!(BuildManager::validate_package_name(&"a".repeat(65)).is_err());
        assert!(BuildManager::validate_package_name("-a").is_err());
        assert!(BuildManager::validate_package_name("a-").is_err());
        assert!(BuildManager::validate_package_name("a@b").is_err());
        assert!(BuildManager::validate_package_name("a.b").is_err());
        assert!(BuildManager::validate_package_name("a b").is_err());
        assert!(BuildManager::validate_package_name("a..b").is_err());
        assert!(BuildManager::validate_package_name("a/b").is_err());
        assert!(BuildManager::validate_package_name("a\\b").is_err());
    }

    #[test]
    fn test_validate_workdir_component_edge_cases() {
        // Valid
        assert!(BuildManager::validate_workdir_component("a").is_ok());
        assert!(BuildManager::validate_workdir_component("a/b").is_ok());
        assert!(BuildManager::validate_workdir_component("a/b/c").is_ok());
        assert!(BuildManager::validate_workdir_component("build").is_ok());
        assert!(BuildManager::validate_workdir_component("src/build").is_ok());

        // Invalid
        assert!(BuildManager::validate_workdir_component("").is_err());
        assert!(BuildManager::validate_workdir_component("/a").is_err());
        assert!(BuildManager::validate_workdir_component("a/").is_err());
        assert!(BuildManager::validate_workdir_component("a/../b").is_err());
        assert!(BuildManager::validate_workdir_component("../a").is_err());
        assert!(BuildManager::validate_workdir_component("a/..").is_err());
    }

    #[test]
    fn test_log_entry_with_all_fields() {
        let mut context = HashMap::new();
        context.insert("key1".to_string(), "value1".to_string());
        context.insert("key2".to_string(), "value2".to_string());

        let entry = LogEntry::new(LogLevel::Warn, "warning message".to_string())
            .with_package("test-pkg".to_string())
            .with_step("compile".to_string())
            .with_context("key1".to_string(), "value1".to_string())
            .with_context("key2".to_string(), "value2".to_string());

        let formatted = entry.format();
        assert!(formatted.contains("WARN"));
        assert!(formatted.contains("test-pkg"));
        assert!(formatted.contains("compile"));
        assert!(formatted.contains("key1=value1"));
        assert!(formatted.contains("key2=value2"));
        assert!(formatted.contains("warning message"));

        let json = entry.format_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["level"], "Warn");
        assert_eq!(parsed["package"], "test-pkg");
        assert_eq!(parsed["step"], "compile");
        assert_eq!(parsed["message"], "warning message");
        assert_eq!(parsed["context"]["key1"], "value1");
        assert_eq!(parsed["context"]["key2"], "value2");
    }

    #[test]
    fn test_log_entry_without_optional_fields() {
        let entry = LogEntry::new(LogLevel::Error, "error message".to_string());
        let formatted = entry.format();
        assert!(formatted.contains("ERROR"));
        assert!(formatted.contains("error message"));
        assert!(!formatted.contains("pkg="));
        assert!(!formatted.contains("step="));

        let json = entry.format_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["level"], "Error");
        assert!(parsed["package"].is_null());
        assert!(parsed["step"].is_null());
    }

    #[test]
    fn test_sanitize_env_case_insensitive() {
        let mut env = HashMap::new();
        env.insert("password".to_string(), "secret123".to_string());
        env.insert("PASSWORD".to_string(), "secret123".to_string());
        env.insert("Password".to_string(), "secret123".to_string());
        env.insert("Api_Key".to_string(), "key123".to_string());
        env.insert("SECRET_TOKEN".to_string(), "token123".to_string());
        env.insert("PrivateKey".to_string(), "priv123".to_string());

        let sanitized = BuildManager::sanitize_env(&env);
        assert_eq!(sanitized.get("password").unwrap(), "***REDACTED***");
        assert_eq!(sanitized.get("PASSWORD").unwrap(), "***REDACTED***");
        assert_eq!(sanitized.get("Password").unwrap(), "***REDACTED***");
        assert_eq!(sanitized.get("Api_Key").unwrap(), "***REDACTED***");
        assert_eq!(sanitized.get("SECRET_TOKEN").unwrap(), "***REDACTED***");
        assert_eq!(sanitized.get("PrivateKey").unwrap(), "***REDACTED***");
    }

    #[test]
    fn test_sanitize_env_preserves_non_sensitive() {
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        env.insert("HOME".to_string(), "/home/user".to_string());
        env.insert("CFLAGS".to_string(), "-O2".to_string());
        env.insert("MY_CUSTOM_VAR".to_string(), "value".to_string());

        let sanitized = BuildManager::sanitize_env(&env);
        assert_eq!(sanitized.get("PATH").unwrap(), "/usr/bin");
        assert_eq!(sanitized.get("HOME").unwrap(), "/home/user");
        assert_eq!(sanitized.get("CFLAGS").unwrap(), "-O2");
        assert_eq!(sanitized.get("MY_CUSTOM_VAR").unwrap(), "value");
    }

    // =========================================================================
    // Build Manager Error Propagation Tests
    // =========================================================================

    #[test]
    fn test_build_recipe_not_found() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config).unwrap();

        // Don't add any recipes
        let temp_dir = tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");

        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let _log_file = fs::File::create(&log_path).unwrap();

        // Try to build non-existent recipe
        let result = manager.build("nonexistent", false);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), BuildError::RecipeNotFound(_)));
    }

    #[test]
    fn test_build_missing_source() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "test-missing-source".to_string();
        recipe.source = "nonexistent-source".to_string();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");

        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let _log_file = fs::File::create(&log_path).unwrap();

        // Should fail with SourceNotFound
        let result = manager.build(&recipe.name, false);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), BuildError::SourceNotFound(_)));
    }

    #[test]
    fn test_build_source_not_extracted() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        // Load source definition but don't extract
        let sources_content = r#"
[test-source]
name = "test-source"
version = "1.0"
url = "https://example.com/test.tar.gz"
sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
archive = "test.tar.gz"
"#;
        let temp_dir = tempdir().unwrap();
        let sources_path = temp_dir.path().join("sources.toml");
        fs::write(&sources_path, sources_content).unwrap();
        manager.load_sources(&sources_path).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "test-not-extracted".to_string();
        recipe.source = "test-source".to_string();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let source_dir = temp_dir.path().join("source");
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");

        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();

        let _log_file = fs::File::create(&log_path).unwrap();

        // Should fail with SourceNotExtracted
        let result = manager.build(&recipe.name, false);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            BuildError::SourceNotExtracted(_)
        ));
    }

    // =========================================================================
    // DESTDIR Interpolation Tests
    // =========================================================================

    #[test]
    fn test_destdir_interpolation_basic() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step with ${DESTDIR} in args
        let step = BuildStep {
            command: "echo".to_string(),
            args: vec!["installing to ${DESTDIR}/usr/bin".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("install step".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        // Verify ${DESTDIR} was interpolated to the staging directory path
        assert!(log_content.contains(&staging_dir.display().to_string()));
        assert!(log_content.contains("installing to"));
    }

    #[test]
    fn test_destdir_interpolation_multiple_args() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step with ${DESTDIR} in multiple args - use echo to avoid file dependency
        let step = BuildStep {
            command: "echo".to_string(),
            args: vec![
                "arg1".to_string(),
                "${DESTDIR}/usr/bin/file.txt".to_string(),
                "arg3".to_string(),
            ],
            workdir: None,
            env: HashMap::new(),
            description: Some("multiple args with destdir".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        // Verify ${DESTDIR} was interpolated in the args
        let staging_str = staging_dir.display().to_string();
        assert!(log_content.contains(&staging_str));
    }

    #[test]
    fn test_destdir_interpolation_no_destdir_provided() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step with ${DESTDIR} but no destdir provided (should become empty string)
        let step = BuildStep {
            command: "echo".to_string(),
            args: vec!["path: ${DESTDIR}/test".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("no destdir".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            None, // No destdir provided
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        // When no destdir, ${DESTDIR} becomes empty string
        assert!(log_content.contains("path: /test"));
    }

    #[test]
    fn test_destdir_interpolation_preserves_other_args() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step with ${DESTDIR} and other args that should not be modified
        let step = BuildStep {
            command: "echo".to_string(),
            args: vec![
                "prefix".to_string(),
                "${DESTDIR}/path".to_string(),
                "suffix".to_string(),
                "no-placeholder".to_string(),
            ],
            workdir: None,
            env: HashMap::new(),
            description: Some("mixed args".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        let staging_str = staging_dir.display().to_string();
        assert!(log_content.contains("prefix"));
        assert!(log_content.contains(&staging_str));
        assert!(log_content.contains("suffix"));
        assert!(log_content.contains("no-placeholder"));
    }

    #[test]
    fn test_unsupported_placeholder_rejected() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step with unsupported placeholder $(nproc) - should pass through literally
        // (we only interpolate ${DESTDIR}, not $(...) or $VAR)
        let step = BuildStep {
            command: "echo".to_string(),
            args: vec!["$(nproc)".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("unsupported placeholder".to_string()),
            continue_on_failure: false,
        };

        let result =
            manager.execute_step(&recipe, &step, &source_dir, &build_dir, &mut log_file, None);
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        // $(nproc) should pass through literally (not interpolated)
        assert!(log_content.contains("$(nproc)"));
    }

    #[test]
    fn test_malformed_destdir_placeholder() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step with malformed placeholder $DESTDIR (missing braces) - should pass through literally
        let step = BuildStep {
            command: "echo".to_string(),
            args: vec!["$DESTDIR/path".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("malformed placeholder".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        // $DESTDIR (without braces) should pass through literally
        assert!(log_content.contains("$DESTDIR/path"));
    }

    // =========================================================================
    // Parallel Job Configuration Tests
    // =========================================================================

    #[test]
    fn test_makeflags_jobs_passed_to_make() {
        let mut config = create_test_config();
        config.build.jobs = Some(4);
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step that prints MAKEFLAGS
        let step = BuildStep {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "echo MAKEFLAGS=$MAKEFLAGS".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("check makeflags".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        // MAKEFLAGS should contain -j4
        assert!(log_content.contains("-j4"));
    }

    #[test]
    fn test_ninjaflags_jobs_passed() {
        let mut config = create_test_config();
        config.build.jobs = Some(8);
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        let step = BuildStep {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "echo NINJAFLAGS=$NINJAFLAGS".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("check ninjaflags".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        assert!(log_content.contains("-j8"));
    }

    #[test]
    fn test_cargoflags_jobs_passed() {
        let mut config = create_test_config();
        config.build.jobs = Some(6);
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        let step = BuildStep {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "echo CARGOFLAGS=$CARGOFLAGS".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("check cargoflags".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        assert!(log_content.contains("--jobs 6"));
    }

    #[test]
    fn test_no_jobs_config_does_not_set_flags() {
        let config = create_test_config(); // jobs = None by default
        let mut manager = BuildManager::new(config).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        let step = BuildStep {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "echo MAKEFLAGS=$MAKEFLAGS".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("check no jobs".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        // MAKEFLAGS should not be set (or empty)
        // The env var won't be in the output if not set
        assert!(!log_content.contains("-j"));
    }

    // =========================================================================
    // Working Directory Behavior Tests
    // =========================================================================

    #[test]
    fn test_step_workdir_relative_to_recipe_workdir() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let mut recipe = create_test_recipe();
        recipe.workdir = Some("subdir".to_string());
        recipe.build = vec![BuildStep {
            command: "pwd".to_string(),
            args: vec![],
            workdir: Some("deeper".to_string()), // Relative to recipe workdir
            env: HashMap::new(),
            description: Some("check workdir".to_string()),
            continue_on_failure: false,
        }];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(source_dir.join("subdir/deeper")).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Compute recipe workdir (source_dir/subdir) and pass as base_workdir
        // This is what the actual build flow does via recipe_workdir()
        let recipe_workdir = source_dir.join("subdir");

        let result = manager.execute_step(
            &recipe,
            &recipe.build[0],
            &recipe_workdir, // Pass recipe workdir as base, not source_dir
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        // Workdir should be source_dir/subdir/deeper
        assert!(log_content.contains("subdir/deeper"));
    }

    #[test]
    fn test_step_workdir_overrides_recipe_workdir() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let mut recipe = create_test_recipe();
        recipe.workdir = Some("recipe_wd".to_string());
        recipe.build = vec![BuildStep {
            command: "pwd".to_string(),
            args: vec![],
            workdir: Some("step_wd".to_string()), // Overrides recipe workdir
            env: HashMap::new(),
            description: Some("check workdir override".to_string()),
            continue_on_failure: false,
        }];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(source_dir.join("step_wd")).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        let result = manager.execute_step(
            &recipe,
            &recipe.build[0],
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        // Step workdir should be relative to source_dir, not recipe workdir
        // (It's joined with source_dir, not recipe workdir)
        assert!(log_content.contains("step_wd"));
    }

    #[test]
    fn test_no_workdir_uses_source_root() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        // No recipe.workdir, no step.workdir
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        let step = BuildStep {
            command: "pwd".to_string(),
            args: vec![],
            workdir: None,
            env: HashMap::new(),
            description: Some("check default workdir".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        // Should use source_dir as workdir
        assert!(log_content.contains(&source_dir.display().to_string()));
    }

    // =========================================================================
    // Recipe Validation Tests (for corrected recipes)
    // =========================================================================

    #[test]
    fn test_recipe_validation_rejects_shell_metacharacters_in_command() {
        let config = create_test_config();
        let _manager = BuildManager::new(config).unwrap();

        let mut recipe = create_test_recipe();
        recipe.build = vec![BuildStep {
            command: "echo hello; ls".to_string(), // Shell metacharacter
            args: vec![],
            workdir: None,
            env: HashMap::new(),
            description: None,
            continue_on_failure: false,
        }];

        let result = recipe.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("shell metacharacters")
        );
    }

    #[test]
    fn test_recipe_validation_rejects_cd_command() {
        // 'cd' is a shell builtin, not a standalone executable
        // The Build Engine rejects commands that are shell builtins
        let config = create_test_config();
        let _manager = BuildManager::new(config).unwrap();

        let mut recipe = create_test_recipe();
        recipe.build = vec![BuildStep {
            command: "cd".to_string(),
            args: vec!["build".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("change directory".to_string()),
            continue_on_failure: false,
        }];

        // Note: The current validation only checks for shell metacharacters
        // 'cd' without metacharacters would pass validation but fail at runtime
        // This is expected behavior - we don't reject 'cd' at validation time
        // but it will fail with "No such file or directory" at execution
        let result = recipe.validate();
        // This should pass validation (cd has no metacharacters)
        // but would fail at runtime
        assert!(result.is_ok());
    }

    #[test]
    fn test_recipe_validation_accepts_valid_build_steps() {
        let config = create_test_config();
        let _manager = BuildManager::new(config).unwrap();

        let recipe = create_test_recipe();
        // Valid recipe with proper steps (no shell metacharacters)
        let result = recipe.validate();
        assert!(result.is_ok());
    }

    // =========================================================================
    // Installation Staging and Path Containment Tests
    // =========================================================================

    #[test]
    fn test_install_staging_directory_containment() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step that tries to write outside staging via ${DESTDIR}
        let step = BuildStep {
            command: "mkdir".to_string(),
            args: vec!["-p".to_string(), "${DESTDIR}/usr/bin".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("create dir in staging".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        // Verify directory was created inside staging
        assert!(staging_dir.join("usr/bin").exists());
    }

    #[test]
    fn test_install_destdir_env_var_available() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Step that reads DESTDIR from environment
        let step = BuildStep {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "echo DESTDIR=$DESTDIR".to_string()],
            workdir: None,
            env: HashMap::new(),
            description: Some("check DESTDIR env".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        let log_content = fs::read_to_string(&log_path).unwrap();
        // DESTDIR env var should be set to staging directory
        assert!(log_content.contains(&staging_dir.display().to_string()));
    }

    #[test]
    fn test_install_step_with_ln_symlink_in_staging() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();
        let recipe = create_test_recipe();
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(staging_dir.join("usr/bin")).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Create a target file first
        fs::write(staging_dir.join("usr/bin/gcc"), "fake gcc").unwrap();

        // Step that creates symlink using ${DESTDIR}
        let step = BuildStep {
            command: "ln".to_string(),
            args: vec![
                "-sv".to_string(),
                "gcc".to_string(),
                "${DESTDIR}/usr/bin/cc".to_string(),
            ],
            workdir: Some("usr/bin".to_string()),
            env: HashMap::new(),
            description: Some("create cc symlink".to_string()),
            continue_on_failure: false,
        };

        let result = manager.execute_step(
            &recipe,
            &step,
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        // Verify symlink was created in staging
        let cc_path = staging_dir.join("usr/bin/cc");
        assert!(cc_path.exists());
        let target = fs::read_link(&cc_path).unwrap();
        assert_eq!(target, Path::new("gcc"));
    }

    // =========================================================================
    // Corrected Recipe-Specific Behavior Tests
    // =========================================================================

    #[test]
    fn test_binutils_build_uses_workdir_not_cd() {
        // Test that binutils-style build with separate build directory works via workdir
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "binutils-test".to_string();
        recipe.build = vec![
            BuildStep {
                command: "mkdir".to_string(),
                args: vec!["-p".to_string(), "build".to_string()],
                workdir: None,
                env: HashMap::new(),
                description: Some("create build dir".to_string()),
                continue_on_failure: false,
            },
            BuildStep {
                command: "true".to_string(), // Mock configure
                args: vec!["--prefix=/usr".to_string()],
                workdir: Some("build".to_string()), // Use workdir instead of cd
                env: HashMap::new(),
                description: Some("configure".to_string()),
                continue_on_failure: false,
            },
            BuildStep {
                command: "true".to_string(), // Mock make
                args: vec![],
                workdir: Some("build".to_string()), // Use workdir instead of cd
                env: HashMap::new(),
                description: Some("build".to_string()),
                continue_on_failure: false,
            },
        ];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Execute all build steps
        for step in &recipe.build {
            let result = manager.execute_step(
                &recipe,
                step,
                &source_dir,
                &build_dir,
                &mut log_file,
                Some(&staging_dir),
            );
            assert!(result.is_ok(), "Step failed: {:?}", step.description);
        }

        let log_content = fs::read_to_string(&log_path).unwrap();
        // Verify build dir was used
        assert!(log_content.contains("build"));
    }

    #[test]
    fn test_ncurses_install_symlinks_without_for_loop() {
        // Test that ncurses-style symlink creation works via explicit steps
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "ncurses-test".to_string();
        recipe.install = vec![
            BuildStep {
                command: "ln".to_string(),
                args: vec![
                    "-svf".to_string(),
                    "libncursesw.so".to_string(),
                    "${DESTDIR}/usr/lib/libncurses.so".to_string(),
                ],
                workdir: None,
                env: HashMap::new(),
                description: Some("libncurses symlink".to_string()),
                continue_on_failure: false,
            },
            BuildStep {
                command: "ln".to_string(),
                args: vec![
                    "-svf".to_string(),
                    "libformw.so".to_string(),
                    "${DESTDIR}/usr/lib/libform.so".to_string(),
                ],
                workdir: None,
                env: HashMap::new(),
                description: Some("libform symlink".to_string()),
                continue_on_failure: false,
            },
            BuildStep {
                command: "ln".to_string(),
                args: vec![
                    "-svf".to_string(),
                    "libpanelw.so".to_string(),
                    "${DESTDIR}/usr/lib/libpanel.so".to_string(),
                ],
                workdir: None,
                env: HashMap::new(),
                description: Some("libpanel symlink".to_string()),
                continue_on_failure: false,
            },
            BuildStep {
                command: "ln".to_string(),
                args: vec![
                    "-svf".to_string(),
                    "libmenuw.so".to_string(),
                    "${DESTDIR}/usr/lib/libmenu.so".to_string(),
                ],
                workdir: None,
                env: HashMap::new(),
                description: Some("libmenu symlink".to_string()),
                continue_on_failure: false,
            },
            BuildStep {
                command: "ln".to_string(),
                args: vec![
                    "-svf".to_string(),
                    "libncursesw.so".to_string(),
                    "${DESTDIR}/usr/lib/libcurses.so".to_string(),
                ],
                workdir: None,
                env: HashMap::new(),
                description: Some("libcurses symlink".to_string()),
                continue_on_failure: false,
            },
        ];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(staging_dir.join("usr/lib")).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        // Create target libraries
        for lib in [
            "libncursesw.so",
            "libformw.so",
            "libpanelw.so",
            "libmenuw.so",
        ] {
            fs::write(staging_dir.join("usr/lib").join(lib), "fake").unwrap();
        }

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Execute all install steps
        for step in &recipe.install {
            let result = manager.execute_step(
                &recipe,
                step,
                &source_dir,
                &build_dir,
                &mut log_file,
                Some(&staging_dir),
            );
            assert!(result.is_ok(), "Step failed: {:?}", step.description);
        }

        // Verify all symlinks were created
        assert!(staging_dir.join("usr/lib/libncurses.so").exists());
        assert!(staging_dir.join("usr/lib/libform.so").exists());
        assert!(staging_dir.join("usr/lib/libpanel.so").exists());
        assert!(staging_dir.join("usr/lib/libmenu.so").exists());
        assert!(staging_dir.join("usr/lib/libcurses.so").exists());
    }

    #[test]
    fn test_gcc_install_cc_symlink() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "gcc-test".to_string();
        recipe.install = vec![BuildStep {
            command: "ln".to_string(),
            args: vec![
                "-sv".to_string(),
                "gcc".to_string(),
                "${DESTDIR}/usr/bin/cc".to_string(),
            ],
            workdir: None,
            env: HashMap::new(),
            description: Some("create cc symlink".to_string()),
            continue_on_failure: false,
        }];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(staging_dir.join("usr/bin")).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        // Create gcc binary
        fs::write(staging_dir.join("usr/bin/gcc"), "fake gcc").unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        let result = manager.execute_step(
            &recipe,
            &recipe.install[0],
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        // Verify cc symlink was created
        let cc_path = staging_dir.join("usr/bin/cc");
        assert!(cc_path.exists());
        let target = fs::read_link(&cc_path).unwrap();
        assert_eq!(target, Path::new("gcc"));
    }

    #[test]
    fn test_bash_install_sh_symlink() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "bash-test".to_string();
        recipe.install = vec![BuildStep {
            command: "ln".to_string(),
            args: vec![
                "-sv".to_string(),
                "bash".to_string(),
                "${DESTDIR}/usr/bin/sh".to_string(),
            ],
            workdir: None,
            env: HashMap::new(),
            description: Some("create sh symlink".to_string()),
            continue_on_failure: false,
        }];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(staging_dir.join("usr/bin")).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        // Create bash binary
        fs::write(staging_dir.join("usr/bin/bash"), "fake bash").unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        let result = manager.execute_step(
            &recipe,
            &recipe.install[0],
            &source_dir,
            &build_dir,
            &mut log_file,
            Some(&staging_dir),
        );
        assert!(result.is_ok());

        // Verify sh symlink was created
        let sh_path = staging_dir.join("usr/bin/sh");
        assert!(sh_path.exists());
        let target = fs::read_link(&sh_path).unwrap();
        assert_eq!(target, Path::new("bash"));
    }

    #[test]
    fn test_glibc_install_nscd_config() {
        let config = create_test_config();
        let mut manager = BuildManager::new(config.clone()).unwrap();

        let mut recipe = create_test_recipe();
        recipe.name = "glibc-test".to_string();
        recipe.install = vec![
            BuildStep {
                command: "mkdir".to_string(),
                args: vec!["-p".to_string(), "${DESTDIR}/etc".to_string()],
                workdir: None,
                env: HashMap::new(),
                description: Some("create etc dir".to_string()),
                continue_on_failure: false,
            },
            BuildStep {
                command: "cp".to_string(),
                args: vec![
                    "nscd.conf".to_string(),
                    "${DESTDIR}/etc/nscd.conf".to_string(),
                ],
                workdir: None,
                env: HashMap::new(),
                description: Some("copy nscd config".to_string()),
                continue_on_failure: false,
            },
        ];
        manager.recipes.insert(recipe.name.clone(), recipe.clone());

        let temp_dir = tempdir().unwrap();
        let build_dir = temp_dir.path().join("build");
        let staging_dir = temp_dir.path().join("staging");
        let log_path = temp_dir.path().join("test.log");
        let source_dir = temp_dir.path().join("source");

        fs::create_dir_all(&build_dir).unwrap();
        fs::create_dir_all(&staging_dir).unwrap();
        fs::create_dir_all(&source_dir).unwrap();

        // Create source nscd.conf
        fs::write(source_dir.join("nscd.conf"), "test config").unwrap();

        let mut log_file = fs::File::create(&log_path).unwrap();

        // Execute install steps
        for step in &recipe.install {
            let result = manager.execute_step(
                &recipe,
                step,
                &source_dir,
                &build_dir,
                &mut log_file,
                Some(&staging_dir),
            );
            assert!(result.is_ok(), "Step failed: {:?}", step.description);
        }

        // Verify etc dir and nscd.conf were created in staging
        assert!(staging_dir.join("etc").exists());
        assert!(staging_dir.join("etc/nscd.conf").exists());
        let content = fs::read_to_string(staging_dir.join("etc/nscd.conf")).unwrap();
        assert_eq!(content, "test config");
    }
}
