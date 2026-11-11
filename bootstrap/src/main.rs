use std::env;
use std::path::PathBuf;
use std::process::Command;

mod config;
use config::load_config;

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
    eprintln!("  help        Show this help message");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -c, --config <PATH>    Path to configuration file (default: bootstrap.toml)");
    eprintln!("  -h, --help             Show this help message");
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

fn main() {
    let args: Vec<String> = env::args().collect();

    // Default config path
    let mut config_path = PathBuf::from("bootstrap.toml");
    let mut command = "check";

    // Parse arguments
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-c" | "--config" => {
                if i + 1 < args.len() {
                    config_path = PathBuf::from(&args[i + 1]);
                    i += 2;
                } else {
                    eprintln!("Error: --config requires a path argument");
                    std::process::exit(1);
                }
            }
            "-h" | "--help" => {
                print_usage();
                return;
            }
            arg if !arg.starts_with('-') => {
                command = arg;
                i += 1;
            }
            _ => {
                eprintln!("Unknown option: {}", args[i]);
                print_usage();
                std::process::exit(1);
            }
        }
    }

    match command {
        "check" => run_prerequisite_check(),
        "config" => show_config(&config_path),
        "help" => print_usage(),
        _ => {
            eprintln!("Unknown command: {}", command);
            print_usage();
            std::process::exit(1);
        }
    }
}
