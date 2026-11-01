mod backend;
mod cli;
mod commands;
mod config;
mod egress;
mod forward;
mod gitconfig;
mod hash;
mod image;
mod paths;
mod plan;
mod profile;
mod provision;
mod repo_key;
mod report;
mod seccomp;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run(args) => commands::run(args),
        Command::Build(args) => commands::build(args).map(|()| ExitCode::SUCCESS),
        Command::Provision(args) => commands::provision(args),
        Command::Profile(cmd) => commands::profile(cmd).map(|()| ExitCode::SUCCESS),
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("podbay: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
