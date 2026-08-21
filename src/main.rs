use std::path::PathBuf;
use std::process;

use clap::{Args, Parser, Subcommand};
use license_check_and_add::{
    AppError, Mode, execute, execute_without_config, print_missing, print_report,
};

#[derive(Debug, Parser)]
#[command(
    name = "license-check-and-add",
    version,
    about = "Check, add, or remove license headers"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Check(CommonArgs),
    Add(AddArgs),
    Remove(CommonArgs),
}

#[derive(Debug, Args)]
struct CommonArgs {
    #[arg(short = 'f', long = "config-file", env = "LICENCE_CHECK_CONFIG_FILE")]
    config_file: Option<PathBuf>,
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
}

#[derive(Debug, Args)]
struct AddArgs {
    #[arg(short = 'f', long = "config-file", env = "LICENCE_CHECK_CONFIG_FILE")]
    config_file: Option<PathBuf>,
    #[arg(
        short = 'r',
        long = "regex-replacements",
        env = "LICENCE_CHECK_REGEX_REPLACEMENTS",
        num_args = 1..
    )]
    regex_replacements: Option<Vec<String>>,
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    let root = std::env::current_dir().unwrap_or_else(|error| {
        eprintln!("I/O error: {error}");
        process::exit(1);
    });

    let (mode, config_file, replacements, paths) = match cli.command {
        Command::Check(args) => (Mode::Check, args.config_file, None, args.paths),
        Command::Add(args) => (
            Mode::Add,
            args.config_file,
            args.regex_replacements,
            args.paths,
        ),
        Command::Remove(args) => (Mode::Remove, args.config_file, None, args.paths),
    };
    let result = if paths.is_empty() {
        match config_file {
            Some(config_file) => execute(&root, &config_file, mode, replacements),
            None => execute_without_config(&root, mode),
        }
    } else {
        license_check_and_add::execute_paths(
            &root,
            config_file.as_deref(),
            mode,
            replacements,
            &paths,
        )
    };

    match result {
        Ok(report) => {
            print_report(&report, mode);
            println!("Command succeeded");
        }
        Err(error) => {
            print_missing(&error);
            eprintln!("{error}");
            eprintln!("Command failed");
            process::exit(match error {
                AppError::CheckFailed { .. } => 1,
                _ => 1,
            });
        }
    }
}
