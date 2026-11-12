use std::env;
use std::path::PathBuf;
use std::process::Command;

mod build;
mod config;
mod error;
mod extract;
mod source;
use build::BuildManager;
use config::load_config;
use error::{BootstrapError, BuildError, CliError, ExtractError, SourceError};
use extract::ExtractManager;
use source::SourceManager;

struct Requirement {
    name: &'static str,
    command: &'static str,
    version_flag: &'static str,
}

enum CheckStatus {
    Found(String),
    Missing,
    Failed,
}

fn check_requirement(requirement: &Requirement) -> CheckStatus {
    let result = Command::new(requirement.command)
        .arg(requirement.version_flag)
        .output();

    match result {
        Ok(output) if output.status.success() => {
            let version_output = if output.stdout.is_empty() {
                &output.stderr
            } else {
                &output.stdout
            };

            let version = String::from_utf8_lossy(version_output)
                .lines()
                .next()
                .unwrap_or("Version unknown")
                .to_string();

            CheckStatus::Found(version)
        }

        Ok(_) => CheckStatus::Failed,

        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CheckStatus::Missing,

        Err(_) => CheckStatus::Failed,
    }
}

fn print_usage() {
    eprintln!("Usage: bootstrap [OPTIONS] [COMMAND]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  check       Check host prerequisites (default)");
    eprintln!("  config      Show resolved configuration");
    eprintln!("  fetch       Fetch source packages");
    eprintln!("  extract     Extract source archives");
    eprintln!("  build       Build packages from recipes");
    eprintln!("  help        Show this help message");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -c, --config <PATH>    Path to configuration file (default: bootstrap.toml)");
    eprintln!("  -s, --sources <PATH>   Path to sources file (default: sources.toml)");
    eprintln!("  -r, --recipes <PATH>   Path to recipes file (default: recipes.toml)");
    eprintln!("  -h, --help             Show this help message");
    eprintln!();
    eprintln!("Fetch options:");
    eprintln!("  --list                 List available sources");
    eprintln!("  --all                  Fetch all sources");
    eprintln!("  --force                Force re-download even if cached");
    eprintln!();
    eprintln!("Extract options:");
    eprintln!("  --list                 List available sources");
    eprintln!("  --all                  Extract all sources");
    eprintln!("  --force                Force re-extract even if already extracted");
    eprintln!();
    eprintln!("Build options:");
    eprintln!("  --list                 List available recipes");
    eprintln!("  --all                  Build all recipes");
    eprintln!("  --keep-on-failure      Keep build directory on failure");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  bootstrap fetch zlib");
    eprintln!("  bootstrap fetch --all");
    eprintln!("  bootstrap fetch --list");
    eprintln!("  bootstrap fetch zlib --force");
    eprintln!("  bootstrap extract zlib");
    eprintln!("  bootstrap extract --all");
    eprintln!("  bootstrap extract --list");
    eprintln!("  bootstrap extract zlib --force");
    eprintln!("  bootstrap build zlib");
    eprintln!("  bootstrap build --all");
    eprintln!("  bootstrap build --list");
    eprintln!("  bootstrap build zlib --keep-on-failure");
}

fn run_prerequisite_check() -> Result<(), CliError> {
    let requirements = [
        Requirement {
            name: "GCC",
            command: "gcc",
            version_flag: "--version",
        },
        Requirement {
            name: "GNU Binutils",
            command: "ld",
            version_flag: "--version",
        },
        Requirement {
            name: "Make",
            command: "make",
            version_flag: "--version",
        },
        Requirement {
            name: "Bash",
            command: "bash",
            version_flag: "--version",
        },
        Requirement {
            name: "Bison",
            command: "bison",
            version_flag: "--version",
        },
        Requirement {
            name: "Gawk",
            command: "gawk",
            version_flag: "--version",
        },
        Requirement {
            name: "Python",
            command: "python3",
            version_flag: "--version",
        },
    ];

    let mut found = 0;
    let mut missing = 0;
    let mut failed = 0;

    println!("Royal Linux Bootstrap");
    println!("Host prerequisite check\n");

    for requirement in &requirements {
        match check_requirement(requirement) {
            CheckStatus::Found(version) => {
                found += 1;
                println!("{:<16} FOUND ({})", requirement.name, version);
            }

            CheckStatus::Missing => {
                missing += 1;
                println!("{:<16} MISSING", requirement.name);
            }

            CheckStatus::Failed => {
                failed += 1;
                println!("{:<16} VERSION CHECK FAILED", requirement.name);
            }
        }
    }

    println!("\nSummary");
    println!("Found:   {}", found);
    println!("Missing: {}", missing);
    println!("Failed:  {}", failed);
    println!("Total:   {}", requirements.len());

    if missing > 0 || failed > 0 {
        return Err(CliError::PrerequisiteCheckFailed);
    }
    Ok(())
}

fn show_config(config_path: &PathBuf) -> Result<(), CliError> {
    let config = load_config(config_path).map_err(|e| CliError::ConfigLoadFailed(e.to_string()))?;

    println!("Resolved Configuration");
    println!("======================\n");

    println!("[host]");
    if let Some(arch) = &config.host.arch {
        println!("arch = \"{}\"", arch);
    }
    if let Some(os) = &config.host.os {
        println!("os = \"{}\"", os);
    }
    if let Some(vendor) = &config.host.vendor {
        println!("vendor = \"{}\"", vendor);
    }
    println!();

    println!("[target]");
    println!("arch = \"{}\"", config.target.arch);
    println!("vendor = \"{}\"", config.target.vendor);
    println!("os = \"{}\"", config.target.os);
    println!("abi = \"{}\"", config.target.abi);
    println!("triplet = \"{}\"", config.target.triplet());
    println!();

    println!("[directories]");
    println!("workspace = \"{}\"", config.directories.workspace.display());
    println!("source = \"{}\"", config.directories.source.display());
    println!("build = \"{}\"", config.directories.build.display());
    println!("output = \"{}\"", config.directories.output.display());
    println!("toolchain = \"{}\"", config.directories.toolchain.display());
    println!("sysroot = \"{}\"", config.directories.sysroot.display());
    println!("logs = \"{}\"", config.directories.logs.display());
    println!();

    println!("[build]");
    if let Some(jobs) = config.build.jobs {
        println!("jobs = {}", jobs);
    }
    println!("verbose = {}", config.build.verbose);
    println!("keep_on_failure = {}", config.build.keep_on_failure);
    println!("opt_level = \"{}\"", config.build.opt_level);
    if !config.build.cflags.is_empty() {
        println!("cflags = {:?}", config.build.cflags);
    }
    if !config.build.cxxflags.is_empty() {
        println!("cxxflags = {:?}", config.build.cxxflags);
    }
    if !config.build.ldflags.is_empty() {
        println!("ldflags = {:?}", config.build.ldflags);
    }
    Ok(())
}

fn run_fetch(
    config_path: &PathBuf,
    sources_path: &PathBuf,
    args: &[String],
) -> Result<(), CliError> {
    let config = load_config(config_path)?;

    let mut manager = SourceManager::new(config)?;
    manager.load_sources(sources_path)?;

    // Parse fetch subcommands
    let mut list = false;
    let mut all = false;
    let mut force = false;
    let mut package_name = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => {
                if list {
                    return Err(CliError::InvalidArgument(
                        "duplicate option --list".to_string(),
                    ));
                }
                list = true;
                i += 1;
            }
            "--all" => {
                if all {
                    return Err(CliError::InvalidArgument(
                        "duplicate option --all".to_string(),
                    ));
                }
                all = true;
                i += 1;
            }
            "--force" => {
                if force {
                    return Err(CliError::InvalidArgument(
                        "duplicate option --force".to_string(),
                    ));
                }
                force = true;
                i += 1;
            }
            arg if !arg.starts_with('-') => {
                if package_name.is_some() {
                    return Err(CliError::InvalidArgument(
                        "only one package name allowed".to_string(),
                    ));
                }
                package_name = Some(arg.to_string());
                i += 1;
            }
            _ => {
                return Err(CliError::UnknownOption(args[i].clone()));
            }
        }
    } // Close while loop

    // Reject conflicting combinations
    if list && all {
        return Err(CliError::ConflictingOptions(
            "--list and --all cannot be used together".to_string(),
        ));
    }
    if list && force {
        return Err(CliError::ConflictingOptions(
            "--list and --force cannot be used together".to_string(),
        ));
    }
    if list && package_name.is_some() {
        return Err(CliError::ConflictingOptions(
            "--list cannot be used with a package name".to_string(),
        ));
    }
    if all && package_name.is_some() {
        return Err(CliError::ConflictingOptions(
            "--all cannot be used with a package name".to_string(),
        ));
    }

    if list {
        let sources = manager.list_sources();
        if sources.is_empty() {
            println!("No sources defined.");
        } else {
            println!("Available sources:");
            for source in sources {
                println!("  {} {} ({})", source.name, source.version, source.archive);
            }
        }
        return Ok(());
    }

    if all {
        let results = manager.fetch_all(force);
        let mut success = 0;
        let mut failed = 0;

        for result in results {
            match result {
                Ok(fetch_result) => {
                    success += 1;
                    let status = if fetch_result.from_cache {
                        "cached"
                    } else {
                        "downloaded"
                    };
                    println!(
                        "{} {} {} [{}]",
                        fetch_result.definition.name,
                        fetch_result.definition.version,
                        fetch_result.definition.archive,
                        status
                    );
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("Error: {}", e);
                }
            }
        }

        println!("\nSummary: {} succeeded, {} failed", success, failed);
        if failed > 0 {
            return Err(CliError::SourceError(SourceError::DownloadFailed(
                "some downloads failed".to_string(),
            )));
        }
        return Ok(());
    }

    if let Some(name) = package_name {
        match manager.fetch(&name, force) {
            Ok(result) => {
                let status = if result.from_cache {
                    "cached"
                } else {
                    "downloaded"
                };
                println!(
                    "{} {} {} [{}]",
                    result.definition.name,
                    result.definition.version,
                    result.definition.archive,
                    status
                );
                println!("Saved to: {}", result.path.display());
            }
            Err(e) => {
                return Err(CliError::SourceError(e));
            }
        }
        return Ok(());
    }

    // No package specified and no --all/--list
    Err(CliError::MissingArgument(
        "package name required (or use --all or --list)".to_string(),
    ))
}

fn run_extract(
    config_path: &PathBuf,
    sources_path: &PathBuf,
    args: &[String],
) -> Result<(), CliError> {
    let config = load_config(config_path)?;

    let mut manager = ExtractManager::new(config);
    manager.load_sources(sources_path)?;

    // Parse extract subcommands
    let mut list = false;
    let mut all = false;
    let mut force = false;
    let mut package_name = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => {
                if list {
                    return Err(CliError::InvalidArgument(
                        "duplicate option --list".to_string(),
                    ));
                }
                list = true;
                i += 1;
            }
            "--all" => {
                if all {
                    return Err(CliError::InvalidArgument(
                        "duplicate option --all".to_string(),
                    ));
                }
                all = true;
                i += 1;
            }
            "--force" => {
                if force {
                    return Err(CliError::InvalidArgument(
                        "duplicate option --force".to_string(),
                    ));
                }
                force = true;
                i += 1;
            }
            arg if !arg.starts_with('-') => {
                if package_name.is_some() {
                    return Err(CliError::InvalidArgument(
                        "only one package name allowed".to_string(),
                    ));
                }
                package_name = Some(arg.to_string());
                i += 1;
            }
            _ => {
                return Err(CliError::UnknownOption(args[i].clone()));
            }
        }
    } // Close while loop

    // Reject conflicting combinations
    if list && all {
        return Err(CliError::ConflictingOptions(
            "--list and --all cannot be used together".to_string(),
        ));
    }
    if list && force {
        return Err(CliError::ConflictingOptions(
            "--list and --force cannot be used together".to_string(),
        ));
    }
    if list && package_name.is_some() {
        return Err(CliError::ConflictingOptions(
            "--list cannot be used with a package name".to_string(),
        ));
    }
    if all && package_name.is_some() {
        return Err(CliError::ConflictingOptions(
            "--all cannot be used with a package name".to_string(),
        ));
    }

    if list {
        let sources = manager.list_sources();
        if sources.is_empty() {
            println!("No sources defined.");
        } else {
            println!("Available sources:");
            for source in sources {
                println!("  {} {} ({})", source.name, source.version, source.archive);
            }
        }
        return Ok(());
    }

    if all {
        let results = manager.extract_all(force);
        let mut success = 0;
        let mut failed = 0;

        for result in results {
            match result {
                Ok(extract_result) => {
                    success += 1;
                    let status = if extract_result.from_cache {
                        "cached"
                    } else {
                        "extracted"
                    };
                    println!(
                        "{} {} {} [{}]",
                        extract_result.definition.name,
                        extract_result.definition.version,
                        extract_result.definition.archive,
                        status
                    );
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("Error: {}", e);
                }
            }
        }

        println!("\nSummary: {} succeeded, {} failed", success, failed);
        if failed > 0 {
            return Err(CliError::ExtractError(ExtractError::ExtractionFailed(
                "some extractions failed".to_string(),
            )));
        }
        return Ok(());
    }

    if let Some(name) = package_name {
        match manager.extract(&name, force) {
            Ok(result) => {
                let status = if result.from_cache {
                    "cached"
                } else {
                    "extracted"
                };
                println!(
                    "{} {} {} [{}]",
                    result.definition.name,
                    result.definition.version,
                    result.definition.archive,
                    status
                );
                println!("Extracted to: {}", result.path.display());
            }
            Err(e) => {
                return Err(CliError::ExtractError(e));
            }
        }
        return Ok(());
    }

    // No package specified and no --all/--list
    Err(CliError::MissingArgument(
        "package name required (or use --all or --list)".to_string(),
    ))
}

fn run_build(
    config_path: &PathBuf,
    sources_path: &PathBuf,
    recipes_path: &PathBuf,
    args: &[String],
) -> Result<(), CliError> {
    let config = load_config(config_path)?;

    let mut manager = BuildManager::new(config)?;
    manager.load_sources(sources_path)?;
    manager.load_recipes(recipes_path)?;

    // Parse build subcommands
    let mut list = false;
    let mut all = false;
    let mut keep_on_failure = false;
    let mut package_name = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => {
                if list {
                    return Err(CliError::InvalidArgument(
                        "duplicate option --list".to_string(),
                    ));
                }
                list = true;
                i += 1;
            }
            "--all" => {
                if all {
                    return Err(CliError::InvalidArgument(
                        "duplicate option --all".to_string(),
                    ));
                }
                all = true;
                i += 1;
            }
            "--keep-on-failure" => {
                if keep_on_failure {
                    return Err(CliError::InvalidArgument(
                        "duplicate option --keep-on-failure".to_string(),
                    ));
                }
                keep_on_failure = true;
                i += 1;
            }
            arg if !arg.starts_with('-') => {
                if package_name.is_some() {
                    return Err(CliError::InvalidArgument(
                        "only one package name allowed".to_string(),
                    ));
                }
                package_name = Some(arg.to_string());
                i += 1;
            }
            _ => {
                return Err(CliError::UnknownOption(args[i].clone()));
            }
        }
    } // Close while loop

    // Reject conflicting combinations
    if list && all {
        return Err(CliError::ConflictingOptions(
            "--list and --all cannot be used together".to_string(),
        ));
    }
    if list && keep_on_failure {
        return Err(CliError::ConflictingOptions(
            "--list and --keep-on-failure cannot be used together".to_string(),
        ));
    }
    if list && package_name.is_some() {
        return Err(CliError::ConflictingOptions(
            "--list cannot be used with a package name".to_string(),
        ));
    }
    if all && package_name.is_some() {
        return Err(CliError::ConflictingOptions(
            "--all cannot be used with a package name".to_string(),
        ));
    }

    if list {
        let recipes = manager.list_recipes();
        if recipes.is_empty() {
            println!("No recipes defined.");
        } else {
            println!("Available recipes:");
            for recipe in recipes {
                println!(
                    "  {} {} (source: {})",
                    recipe.name, recipe.version, recipe.source
                );
            }
        }
        return Ok(());
    }

    if all {
        let results = manager.build_all(keep_on_failure);
        let mut success = 0;
        let mut failed = 0;

        for result in results {
            match result {
                Ok(build_result) => {
                    success += 1;
                    println!("{} {} [built]", build_result.name, build_result.version);
                    println!("  Build dir: {}", build_result.build_dir.display());
                    println!("  Log: {}", build_result.log_path.display());
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("Error: {}", e);
                }
            }
        }

        println!("\nSummary: {} succeeded, {} failed", success, failed);
        if failed > 0 {
            return Err(CliError::BootstrapError(BootstrapError::Build(Box::new(
                BuildError::ExecutionFailed("some builds failed".to_string()),
            ))));
        }
        return Ok(());
    }

    if let Some(name) = package_name {
        match manager.build(&name, keep_on_failure) {
            Ok(result) => {
                println!("{} {} [built]", result.name, result.version);
                println!("Build dir: {}", result.build_dir.display());
                println!("Log: {}", result.log_path.display());
            }
            Err(e) => {
                return Err(CliError::BuildError(e));
            }
        }
        return Ok(());
    }

    // No package specified and no --all/--list
    Err(CliError::MissingArgument(
        "package name required (or use --all or --list)".to_string(),
    ))
}

/// Parsed global options and command
struct ParsedArgs {
    config_path: PathBuf,
    sources_path: PathBuf,
    recipes_path: PathBuf,
    command: String,
    subcommand_args: Vec<String>,
}

/// Parse global arguments, returning the parsed structure or an error.
fn parse_global_args(args: &[String]) -> Result<ParsedArgs, CliError> {
    let mut config_path = PathBuf::from("bootstrap.toml");
    let mut sources_path = PathBuf::from("sources.toml");
    let mut recipes_path = PathBuf::from("recipes.toml");
    let mut command = "check".to_string();
    let mut subcommand_args: Vec<String> = Vec::new();

    // Track which global options have been seen to reject duplicates
    let mut config_seen = false;
    let mut sources_seen = false;
    let mut recipes_seen = false;

    let mut i = 1;
    let mut command_found = false;

    while i < args.len() {
        let arg = &args[i];

        if !command_found {
            // Still parsing global options / command
            match arg.as_str() {
                "-c" | "--config" => {
                    if config_seen {
                        return Err(CliError::InvalidArgument(
                            "duplicate option --config".to_string(),
                        ));
                    }
                    config_seen = true;
                    if i + 1 < args.len() {
                        config_path = PathBuf::from(&args[i + 1]);
                        // Validate the path argument is not another option
                        if args[i + 1].starts_with('-') {
                            return Err(CliError::InvalidArgument(
                                "--config requires a path argument".to_string(),
                            ));
                        }
                        i += 2;
                    } else {
                        return Err(CliError::InvalidArgument(
                            "--config requires a path argument".to_string(),
                        ));
                    }
                }
                "-s" | "--sources" => {
                    if sources_seen {
                        return Err(CliError::InvalidArgument(
                            "duplicate option --sources".to_string(),
                        ));
                    }
                    sources_seen = true;
                    if i + 1 < args.len() {
                        sources_path = PathBuf::from(&args[i + 1]);
                        if args[i + 1].starts_with('-') {
                            return Err(CliError::InvalidArgument(
                                "--sources requires a path argument".to_string(),
                            ));
                        }
                        i += 2;
                    } else {
                        return Err(CliError::InvalidArgument(
                            "--sources requires a path argument".to_string(),
                        ));
                    }
                }
                "-r" | "--recipes" => {
                    if recipes_seen {
                        return Err(CliError::InvalidArgument(
                            "duplicate option --recipes".to_string(),
                        ));
                    }
                    recipes_seen = true;
                    if i + 1 < args.len() {
                        recipes_path = PathBuf::from(&args[i + 1]);
                        if args[i + 1].starts_with('-') {
                            return Err(CliError::InvalidArgument(
                                "--recipes requires a path argument".to_string(),
                            ));
                        }
                        i += 2;
                    } else {
                        return Err(CliError::InvalidArgument(
                            "--recipes requires a path argument".to_string(),
                        ));
                    }
                }
                "-h" | "--help" => {
                    print_usage();
                    return Err(CliError::HelpRequested);
                }
                arg if !arg.starts_with('-') => {
                    // This is the command
                    command = arg.to_string();
                    command_found = true;
                    i += 1;
                }
                _ => {
                    return Err(CliError::UnknownOption(arg.to_string()));
                }
            }
        } else {
            // Already found command, rest are subcommand args
            subcommand_args.push(arg.clone());
            i += 1;
        }
    }

    Ok(ParsedArgs {
        config_path,
        sources_path,
        recipes_path,
        command,
        subcommand_args,
    })
}

fn main() {
    let args: Vec<String> = env::args().collect();

    // Parse global arguments
    let parsed = match parse_global_args(&args) {
        Ok(p) => p,
        Err(CliError::HelpRequested) => {
            // Help was requested, exit successfully
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            print_usage();
            std::process::exit(1);
        }
    };

    let result = match parsed.command.as_str() {
        "check" => {
            if !parsed.subcommand_args.is_empty() {
                Err(CliError::InvalidArgument(
                    "check command does not accept arguments".to_string(),
                ))
            } else {
                run_prerequisite_check()
            }
        }
        "config" => {
            if !parsed.subcommand_args.is_empty() {
                Err(CliError::InvalidArgument(
                    "config command does not accept arguments".to_string(),
                ))
            } else {
                show_config(&parsed.config_path)
            }
        }
        "fetch" => run_fetch(
            &parsed.config_path,
            &parsed.sources_path,
            &parsed.subcommand_args,
        ),
        "extract" => run_extract(
            &parsed.config_path,
            &parsed.sources_path,
            &parsed.subcommand_args,
        ),
        "build" => run_build(
            &parsed.config_path,
            &parsed.sources_path,
            &parsed.recipes_path,
            &parsed.subcommand_args,
        ),
        "help" => {
            if !parsed.subcommand_args.is_empty() {
                Err(CliError::InvalidArgument(
                    "help command does not accept arguments".to_string(),
                ))
            } else {
                print_usage();
                Ok(())
            }
        }
        _ => Err(CliError::UnknownCommand(parsed.command)),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        print_usage();
        std::process::exit(1);
    }
}

#[cfg(test)]
mod test_cli_error_display {
    use super::error::CliError;

    #[test]
    fn test_invalid_argument_display() {
        let e = CliError::InvalidArgument("test message".to_string());
        let display = format!("{}", e);
        println!("Display output: {}", display);
        assert_eq!(display, "Invalid argument: test message");
    }
}
