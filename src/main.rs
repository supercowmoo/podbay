mod backend;
mod cli;
mod commands;
mod config;
mod hash;
mod image;
mod paths;
mod plan;
mod profile;
mod repo_key;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run(args) => commands::run(args),
        Command::Build(args) => commands::build(args),
        Command::Profile(cmd) => commands::profile(cmd),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("podbay: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
