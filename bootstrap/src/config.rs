use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Supported target architectures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Architecture {
    X86_64,
    Aarch64,
}

impl Architecture {
    /// Returns the architecture as a string suitable for use in paths and toolchain names.
    pub fn as_str(&self) -> &'static str {
        match self {
            Architecture::X86_64 => "x86_64",
            Architecture::Aarch64 => "aarch64",
        }
    }

    /// Detects the host architecture from the current runtime.
    pub fn detect_host() -> Self {
        match std::env::consts::ARCH {
            "x86_64" => Architecture::X86_64,
            "aarch64" => Architecture::Aarch64,
            other => {
                eprintln!(
                    "Warning: Unknown host architecture '{}', defaulting to x86_64",
                    other
                );
                Architecture::X86_64
            }
        }
    }
}

impl std::str::FromStr for Architecture {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "x86_64" | "x86-64" | "amd64" => Ok(Architecture::X86_64),
            "aarch64" | "arm64" => Ok(Architecture::Aarch64),
            _ => Err(ConfigError::UnsupportedArchitecture(s.to_string())),
        }
    }
}

impl std::fmt::Display for Architecture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Host system information (configured by user; detection runs at load time if not specified).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    /// Host architecture (detected at load time if not specified).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<Architecture>,
    /// Host operating system (detected at load time if not specified).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Host vendor (detected at load time if not specified).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            arch: None,
            os: None,
            vendor: None,
        }
    }
}

impl HostConfig {
    /// Fills in any unset fields with detected values.
    pub fn apply_detection(&mut self) {
        if self.arch.is_none() {
            self.arch = Some(Architecture::detect_host());
        }
        if self.os.is_none() {
            self.os = Some("linux".to_string());
        }
        if self.vendor.is_none() {
            self.vendor = Some("unknown".to_string());
        }
    }
}

/// Target system configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetConfig {
    /// Target architecture (x86_64 or aarch64).
    #[serde(deserialize_with = "deserialize_arch")]
    pub arch: Architecture,
    /// Target vendor (default: "unknown").
    #[serde(default = "default_vendor")]
    pub vendor: String,
    /// Target operating system (default: "linux").
    #[serde(default = "default_os")]
    pub os: String,
    /// Target ABI (default: "gnu").
    #[serde(default = "default_abi")]
    pub abi: String,
}

fn deserialize_arch<'de, D>(deserializer: D) -> Result<Architecture, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    s.parse::<Architecture>().map_err(serde::de::Error::custom)
}

fn default_vendor() -> String {
    "unknown".to_string()
}

fn default_os() -> String {
    "linux".to_string()
}

fn default_abi() -> String {
    "gnu".to_string()
}

impl TargetConfig {
    /// Returns the full target triplet (e.g., "x86_64-unknown-linux-gnu").
    pub fn triplet(&self) -> String {
        format!(
            "{}-{}-{}-{}",
            self.arch.as_str(),
            self.vendor,
            self.os,
            self.abi
        )
    }
}

impl Default for TargetConfig {
    fn default() -> Self {
        Self {
            arch: Architecture::X86_64,
            vendor: default_vendor(),
            os: default_os(),
            abi: default_abi(),
        }
    }
}

/// Build directory configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryConfig {
    /// Workspace root directory (contains sources, build, output).
    #[serde(default = "default_workspace_dir")]
    pub workspace: PathBuf,
    /// Source directory (where source code is fetched/extracted).
    #[serde(default = "default_source_dir")]
    pub source: PathBuf,
    /// Build directory (where compilation happens).
    #[serde(default = "default_build_dir")]
    pub build: PathBuf,
    /// Output directory (where artifacts are placed).
    #[serde(default = "default_output_dir")]
    pub output: PathBuf,
}

fn default_workspace_dir() -> PathBuf {
    PathBuf::from("workspace")
}

fn default_source_dir() -> PathBuf {
    PathBuf::from("workspace/sources")
}

fn default_build_dir() -> PathBuf {
    PathBuf::from("workspace/build")
}

fn default_output_dir() -> PathBuf {
    PathBuf::from("workspace/output")
}

impl Default for DirectoryConfig {
    fn default() -> Self {
        Self {
            workspace: default_workspace_dir(),
            source: default_source_dir(),
            build: default_build_dir(),
            output: default_output_dir(),
        }
    }
}

impl DirectoryConfig {
    /// Resolves all paths relative to the given configuration file directory.
    pub fn resolve_paths(&mut self, config_dir: &Path) {
        self.workspace = resolve_relative_path(config_dir, &self.workspace);
        self.source = resolve_relative_path(config_dir, &self.source);
        self.build = resolve_relative_path(config_dir, &self.build);
        self.output = resolve_relative_path(config_dir, &self.output);
    }
}

/// Build configuration options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildConfig {
    /// Number of parallel jobs (default: number of CPU cores).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jobs: Option<usize>,
    /// Enable verbose output.
    #[serde(default)]
    pub verbose: bool,
    /// Keep build artifacts on failure.
    #[serde(default)]
    pub keep_on_failure: bool,
    /// Build optimization level (default: "2").
    #[serde(default = "default_opt_level")]
    pub opt_level: String,
    /// Additional CFLAGS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cflags: Vec<String>,
    /// Additional CXXFLAGS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cxxflags: Vec<String>,
    /// Additional LDFLAGS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ldflags: Vec<String>,
}

fn default_opt_level() -> String {
    "2".to_string()
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            jobs: None,
            verbose: false,
            keep_on_failure: false,
            opt_level: default_opt_level(),
            cflags: Vec::new(),
            cxxflags: Vec::new(),
            ldflags: Vec::new(),
        }
    }
}

/// Complete bootstrap configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Host system information.
    #[serde(default)]
    pub host: HostConfig,
    /// Target system configuration.
    #[serde(default)]
    pub target: TargetConfig,
    /// Directory configuration.
    #[serde(default)]
    pub directories: DirectoryConfig,
    /// Build configuration.
    #[serde(default)]
    pub build: BuildConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: HostConfig::default(),
            target: TargetConfig::default(),
            directories: DirectoryConfig::default(),
            build: BuildConfig::default(),
        }
    }
}

/// Configuration errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read configuration file: {0}")]
    Io(#[from] std::io::Error),

    #[error("Failed to parse TOML: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("Unsupported architecture: {0}. Supported: x86_64, aarch64")]
    UnsupportedArchitecture(String),

    #[error("Configuration validation failed: {0}")]
    Validation(String),
}

/// Loads configuration from a TOML file.
pub fn load_config<P: AsRef<Path>>(path: P) -> Result<Config, ConfigError> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path)?;
    let mut config: Config = toml::from_str(&content)?;

    // Resolve config file directory to absolute path
    let config_file = std::fs::canonicalize(path)?;
    let config_dir = config_file.parent().unwrap_or_else(|| Path::new("."));

    // Resolve paths relative to the config file's directory (always absolute)
    config.directories.resolve_paths(config_dir);

    // Apply host detection for any unset fields
    config.host.apply_detection();

    // Validate the configuration
    validate_config(&config)?;

    Ok(config)
}

/// Validates the configuration.
fn validate_config(config: &Config) -> Result<(), ConfigError> {
    // Validate workspace directory parent exists
    if let Some(parent) = config.directories.workspace.parent() {
        if !parent.exists() {
            return Err(ConfigError::Validation(format!(
                "Workspace directory parent does not exist: {}",
                parent.display()
            )));
        }
    }

    // Validate jobs setting
    if let Some(jobs) = config.build.jobs {
        if jobs == 0 {
            return Err(ConfigError::Validation(
                "jobs must be greater than 0".to_string(),
            ));
        }
    }

    // Validate opt_level
    if config.build.opt_level != "0"
        && config.build.opt_level != "1"
        && config.build.opt_level != "2"
        && config.build.opt_level != "3"
        && config.build.opt_level != "s"
        && config.build.opt_level != "z"
    {
        return Err(ConfigError::Validation(format!(
            "Invalid opt_level: {}. Supported: 0, 1, 2, 3, s, z",
            config.build.opt_level
        )));
    }

    Ok(())
}

/// Resolves a path relative to a base directory.
fn resolve_relative_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_architecture_from_str() {
        assert_eq!(
            "x86_64".parse::<Architecture>().unwrap(),
            Architecture::X86_64
        );
        assert_eq!(
            "x86-64".parse::<Architecture>().unwrap(),
            Architecture::X86_64
        );
        assert_eq!(
            "amd64".parse::<Architecture>().unwrap(),
            Architecture::X86_64
        );
        assert_eq!(
            "aarch64".parse::<Architecture>().unwrap(),
            Architecture::Aarch64
        );
        assert_eq!(
            "arm64".parse::<Architecture>().unwrap(),
            Architecture::Aarch64
        );

        assert!("invalid".parse::<Architecture>().is_err());
        assert!("riscv64".parse::<Architecture>().is_err());
    }

    #[test]
    fn test_architecture_display() {
        assert_eq!(Architecture::X86_64.to_string(), "x86_64");
        assert_eq!(Architecture::Aarch64.to_string(), "aarch64");
    }

    #[test]
    fn test_architecture_detect_host() {
        // On this system, should be x86_64
        let detected = Architecture::detect_host();
        assert!(matches!(
            detected,
            Architecture::X86_64 | Architecture::Aarch64
        ));
    }

    #[test]
    fn test_target_config_triplet() {
        let target = TargetConfig::default();
        assert_eq!(target.triplet(), "x86_64-unknown-linux-gnu");

        let target = TargetConfig {
            arch: Architecture::Aarch64,
            vendor: "custom".to_string(),
            os: "linux".to_string(),
            abi: "gnu".to_string(),
        };
        assert_eq!(target.triplet(), "aarch64-custom-linux-gnu");
    }

    #[test]
    fn test_directory_config_resolve_paths() {
        let temp_dir = tempdir().unwrap();
        let config_dir = temp_dir.path();

        let mut dirs = DirectoryConfig {
            workspace: PathBuf::from("workspace"),
            source: PathBuf::from("workspace/sources"),
            build: PathBuf::from("workspace/build"),
            output: PathBuf::from("workspace/output"),
        };

        dirs.resolve_paths(config_dir);

        assert_eq!(dirs.workspace, config_dir.join("workspace"));
        assert_eq!(dirs.source, config_dir.join("workspace/sources"));
        assert_eq!(dirs.build, config_dir.join("workspace/build"));
        assert_eq!(dirs.output, config_dir.join("workspace/output"));
    }

    #[test]
    fn test_directory_config_absolute_paths() {
        let temp_dir = tempdir().unwrap();
        let config_dir = temp_dir.path();

        let mut dirs = DirectoryConfig {
            workspace: PathBuf::from("/absolute/workspace"),
            source: PathBuf::from("/absolute/sources"),
            build: PathBuf::from("/absolute/build"),
            output: PathBuf::from("/absolute/output"),
        };

        dirs.resolve_paths(config_dir);

        assert_eq!(dirs.workspace, PathBuf::from("/absolute/workspace"));
        assert_eq!(dirs.source, PathBuf::from("/absolute/sources"));
        assert_eq!(dirs.build, PathBuf::from("/absolute/build"));
        assert_eq!(dirs.output, PathBuf::from("/absolute/output"));
    }

    #[test]
    fn test_load_valid_config() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        let config_content = r#"
[target]
arch = "x86_64"

[directories]
workspace = "my_workspace"
source = "my_workspace/sources"
build = "my_workspace/build"
output = "my_workspace/output"

[build]
jobs = 4
verbose = true
opt_level = "2"
"#;

        fs::write(&config_path, config_content).unwrap();

        let config = load_config(&config_path).unwrap();

        assert_eq!(config.target.arch, Architecture::X86_64);
        assert_eq!(
            config.directories.workspace,
            temp_dir.path().join("my_workspace")
        );
        assert_eq!(config.build.jobs, Some(4));
        assert!(config.build.verbose);
        assert_eq!(config.build.opt_level, "2");
    }

    #[test]
    fn test_load_config_with_aarch64() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        let config_content = r#"
[target]
arch = "aarch64"
"#;

        fs::write(&config_path, config_content).unwrap();

        let config = load_config(&config_path).unwrap();

        assert_eq!(config.target.arch, Architecture::Aarch64);
        assert_eq!(config.target.triplet(), "aarch64-unknown-linux-gnu");
    }

    #[test]
    fn test_load_config_invalid_toml() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        let config_content = r#"
[target
arch = "x86_64"
"#;

        fs::write(&config_path, config_content).unwrap();

        let result = load_config(&config_path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::TomlParse(_)));
    }

    #[test]
    fn test_load_config_unsupported_architecture() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        let config_content = r#"
[target]
arch = "riscv64"
"#;

        fs::write(&config_path, config_content).unwrap();

        let result = load_config(&config_path);
        eprintln!("Error: {:?}", result);
        assert!(result.is_err());
        // The error is wrapped in TomlParse due to serde deserialization
        assert!(matches!(result.unwrap_err(), ConfigError::TomlParse(_)));
    }

    #[test]
    fn test_load_config_missing_file() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("nonexistent.toml");

        let result = load_config(&config_path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::Io(_)));
    }

    #[test]
    fn test_validate_config_invalid_jobs() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        let config_content = r#"
[build]
jobs = 0
"#;

        fs::write(&config_path, config_content).unwrap();

        let result = load_config(&config_path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::Validation(_)));
    }

    #[test]
    fn test_validate_config_invalid_opt_level() {
        let temp_dir = tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        let config_content = r#"
[build]
opt_level = "5"
"#;

        fs::write(&config_path, config_content).unwrap();

        let result = load_config(&config_path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::Validation(_)));
    }

    #[test]
    fn test_validate_config_valid_opt_levels() {
        let temp_dir = tempdir().unwrap();

        for opt in ["0", "1", "2", "3", "s", "z"] {
            let config_path = temp_dir.path().join("config.toml");
            let config_content = format!("[build]\nopt_level = \"{}\"", opt);
            fs::write(&config_path, config_content).unwrap();

            let config = load_config(&config_path).unwrap();
            assert_eq!(config.build.opt_level, opt);
        }
    }

    #[test]
    fn test_default_config() {
        let config = Config::default();

        assert_eq!(config.target.arch, Architecture::X86_64);
        assert_eq!(config.target.triplet(), "x86_64-unknown-linux-gnu");
        assert_eq!(config.directories.workspace, PathBuf::from("workspace"));
        assert_eq!(config.build.opt_level, "2");
        assert!(!config.build.verbose);
    }

    #[test]
    fn test_config_serialization() {
        let config = Config::default();
        let toml_str = toml::to_string(&config).unwrap();
        assert!(toml_str.contains("x86_64"));
        assert!(toml_str.contains("workspace"));
    }

    #[test]
    fn test_path_resolution_relative_to_config_file() {
        let temp_dir = tempdir().unwrap();
        let config_dir = temp_dir.path().join("subdir");
        fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");

        let config_content = r#"
[directories]
workspace = "workspace"
"#;

        fs::write(&config_path, config_content).unwrap();

        let config = load_config(&config_path).unwrap();

        assert_eq!(config.directories.workspace, config_dir.join("workspace"));
    }
}
