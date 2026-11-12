use std::env;
use std::path::PathBuf;
use std::process::Command;

mod config;
mod error;
mod extract;
mod source;
use config::load_config;
use error::BootstrapError;
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
    eprintln!("  help        Show this help message");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -c, --config <PATH>    Path to configuration file (default: bootstrap.toml)");
    eprintln!("  -s, --sources <PATH>   Path to sources file (default: sources.toml)");
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
    eprintln!("Examples:");
    eprintln!("  bootstrap fetch zlib");
    eprintln!("  bootstrap fetch --all");
    eprintln!("  bootstrap fetch --list");
    eprintln!("  bootstrap fetch zlib --force");
    eprintln!("  bootstrap extract zlib");
    eprintln!("  bootstrap extract --all");
    eprintln!("  bootstrap extract --list");
    eprintln!("  bootstrap extract zlib --force");
}

fn run_prerequisite_check() {
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
        std::process::exit(1);
    }
}

fn show_config(config_path: &PathBuf) {
    match load_config(config_path) {
        Ok(config) => {
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
        }
        Err(e) => {
            eprintln!("Error loading configuration: {}", e);
            std::process::exit(1);
        }
    }
}

fn run_fetch(
    config_path: &PathBuf,
    sources_path: &PathBuf,
    args: &[String],
) -> Result<(), BootstrapError> {
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
                    eprintln!("Error: duplicate option --list");
                    print_usage();
                    std::process::exit(1);
                }
                list = true;
                i += 1;
            }
            "--all" => {
                if all {
                    eprintln!("Error: duplicate option --all");
                    print_usage();
                    std::process::exit(1);
                }
                all = true;
                i += 1;
            }
            "--force" => {
                if force {
                    eprintln!("Error: duplicate option --force");
                    print_usage();
                    std::process::exit(1);
                }
                force = true;
                i += 1;
            }
            arg if !arg.starts_with('-') => {
                if package_name.is_some() {
                    eprintln!("Error: only one package name allowed");
                    print_usage();
                    std::process::exit(1);
                }
                package_name = Some(arg.to_string());
                i += 1;
            }
            _ => {
                eprintln!("Unknown fetch option: {}", args[i]);
                print_usage();
                std::process::exit(1);
            }
        }
    }

    // Reject conflicting combinations
    if list && all {
        eprintln!("Error: --list and --all cannot be used together");
        print_usage();
        std::process::exit(1);
    }
    if list && force {
        eprintln!("Error: --list and --force cannot be used together");
        print_usage();
        std::process::exit(1);
    }
    if list && package_name.is_some() {
        eprintln!("Error: --list cannot be used with a package name");
        print_usage();
        std::process::exit(1);
    }
    if all && package_name.is_some() {
        eprintln!("Error: --all cannot be used with a package name");
        print_usage();
        std::process::exit(1);
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
            std::process::exit(1);
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
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // No package specified and no --all/--list
    eprintln!("Error: package name required (or use --all or --list)");
    print_usage();
    std::process::exit(1);
}

fn run_extract(
    config_path: &PathBuf,
    sources_path: &PathBuf,
    args: &[String],
) -> Result<(), BootstrapError> {
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
                    eprintln!("Error: duplicate option --list");
                    print_usage();
                    std::process::exit(1);
                }
                list = true;
                i += 1;
            }
            "--all" => {
                if all {
                    eprintln!("Error: duplicate option --all");
                    print_usage();
                    std::process::exit(1);
                }
                all = true;
                i += 1;
            }
            "--force" => {
                if force {
                    eprintln!("Error: duplicate option --force");
                    print_usage();
                    std::process::exit(1);
                }
                force = true;
                i += 1;
            }
            arg if !arg.starts_with('-') => {
                if package_name.is_some() {
                    eprintln!("Error: only one package name allowed");
                    print_usage();
                    std::process::exit(1);
                }
                package_name = Some(arg.to_string());
                i += 1;
            }
            _ => {
                eprintln!("Unknown extract option: {}", args[i]);
                print_usage();
                std::process::exit(1);
            }
        }
    }

    // Reject conflicting combinations
    if list && all {
        eprintln!("Error: --list and --all cannot be used together");
        print_usage();
        std::process::exit(1);
    }
    if list && force {
        eprintln!("Error: --list and --force cannot be used together");
        print_usage();
        std::process::exit(1);
    }
    if list && package_name.is_some() {
        eprintln!("Error: --list cannot be used with a package name");
        print_usage();
        std::process::exit(1);
    }
    if all && package_name.is_some() {
        eprintln!("Error: --all cannot be used with a package name");
        print_usage();
        std::process::exit(1);
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
            std::process::exit(1);
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
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // No package specified and no --all/--list
    eprintln!("Error: package name required (or use --all or --list)");
    print_usage();
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = env::args().collect();

    // Default config paths
    let mut config_path = PathBuf::from("bootstrap.toml");
    let mut sources_path = PathBuf::from("sources.toml");
    let mut command = "check";
    let mut subcommand_args: Vec<String> = Vec::new();

    // Parse arguments: global options first, then command, then subcommand args
    let mut i = 1;
    let mut command_found = false;

    while i < args.len() {
        let arg = &args[i];

        if !command_found {
            // Still parsing global options / command
            match arg.as_str() {
                "-c" | "--config" => {
                    if i + 1 < args.len() {
                        config_path = PathBuf::from(&args[i + 1]);
                        i += 2;
                    } else {
                        eprintln!("Error: --config requires a path argument");
                        std::process::exit(1);
                    }
                }
                "-s" | "--sources" => {
                    if i + 1 < args.len() {
                        sources_path = PathBuf::from(&args[i + 1]);
                        i += 2;
                    } else {
                        eprintln!("Error: --sources requires a path argument");
                        std::process::exit(1);
                    }
                }
                "-h" | "--help" => {
                    print_usage();
                    return;
                }
                arg if !arg.starts_with('-') => {
                    // This is the command
                    command = arg;
                    command_found = true;
                    i += 1;
                }
                _ => {
                    eprintln!("Unknown option: {}", arg);
                    print_usage();
                    std::process::exit(1);
                }
            }
        } else {
            // Already found command, rest are subcommand args
            subcommand_args.push(arg.clone());
            i += 1;
        }
    }

    let result = match command {
        "check" => {
            if !subcommand_args.is_empty() {
                eprintln!("Error: check command does not accept arguments");
                print_usage();
                std::process::exit(1);
            }
            run_prerequisite_check();
            Ok(())
        }
        "config" => {
            if !subcommand_args.is_empty() {
                eprintln!("Error: config command does not accept arguments");
                print_usage();
                std::process::exit(1);
            }
            show_config(&config_path);
            Ok(())
        }
        "fetch" => run_fetch(&config_path, &sources_path, &subcommand_args),
        "extract" => run_extract(&config_path, &sources_path, &subcommand_args),
        "help" => {
            if !subcommand_args.is_empty() {
                eprintln!("Error: help command does not accept arguments");
                print_usage();
                std::process::exit(1);
            }
            print_usage();
            Ok(())
        }
        _ => {
            eprintln!("Unknown command: {}", command);
            print_usage();
            std::process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
