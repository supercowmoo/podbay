use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::backend::BackendKind;
use crate::config::NetworkMode;

/// Disposable, isolated containers around a project.
#[derive(Debug, Parser)]
#[command(name = "podbay", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start a container around a project.
    Run(RunArgs),
    /// Build (or rebuild) the images a profile needs.
    Build(BuildArgs),
    /// Run the profile's [provision] commands now, with network access.
    /// `run` does this by itself when what they depend on changed.
    Provision(ProvisionArgs),
    /// Manage profiles.
    #[command(subcommand)]
    Profile(ProfileCommand),
}

#[derive(Debug, Args)]
pub struct EngineArgs {
    /// Container engine [default: config `backend`, else podman if installed, else docker]
    #[arg(long, value_enum)]
    pub backend: Option<BackendKind>,
    /// Profile to apply [default: config `default_profile`, else `default` if it exists]
    #[arg(short, long)]
    pub profile: Option<String>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub engine: EngineArgs,
    /// Give this project its own copies of the profile's provision volumes
    /// instead of sharing them. Use for code you do not trust.
    #[arg(long)]
    pub local_cache: bool,
    /// Network access for the session, overriding any allowlist
    /// [default: profile or config `network`, else none]
    #[arg(long, value_enum, conflicts_with_all = ["online", "offline"])]
    pub network: Option<NetworkMode>,
    /// Same as `--network full`.
    #[arg(long, conflicts_with = "offline")]
    pub online: bool,
    /// Same as `--network none`.
    #[arg(long)]
    pub offline: bool,
    /// Print the engine command instead of running it. Builds nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Project directory to mount [default: current directory]
    pub project: Option<PathBuf>,
    /// Command to run instead of a login shell (the profile's `shell`, else
    /// bash).
    #[arg(last = true)]
    pub command: Vec<String>,
}

impl RunArgs {
    /// The network mode chosen on the command line, and the flag that chose
    /// it, for messages.
    pub fn network_flag(&self) -> Option<(NetworkMode, &'static str)> {
        match (self.network, self.online, self.offline) {
            (Some(mode), _, _) => Some((mode, "--network")),
            (None, true, _) => Some((NetworkMode::Full, "--online")),
            (None, _, true) => Some((NetworkMode::None, "--offline")),
            (None, false, false) => None,
        }
    }
}

#[derive(Debug, Args)]
pub struct ProvisionArgs {
    #[command(flatten)]
    pub engine: EngineArgs,
    /// Install into this project's own copies of the provision volumes, as
    /// `run --local-cache` uses, instead of the shared ones.
    #[arg(long)]
    pub local_cache: bool,
    /// Print the engine command instead of running it. Builds nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Project directory [default: current directory]
    pub project: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct BuildArgs {
    #[command(flatten)]
    pub engine: EngineArgs,
    /// Rebuild every layer from scratch: no build cache, and the Debian base
    /// pulled again, so updated packages and a rebuilt custom base come in.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// Create a profile from a commented template.
    Init {
        name: String,
        /// Profiles the new one builds on.
        #[arg(long, value_delimiter = ',')]
        extends: Vec<String>,
    },
    /// List profiles.
    List,
    /// Show a profile with everything it extends merged in.
    Show { name: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn trailing_command() {
        let cli =
            Cli::try_parse_from(["podbay", "run", "-p", "ai", ".", "--", "ls", "-la"]).unwrap();
        let Command::Run(run) = cli.command else {
            panic!()
        };
        assert_eq!(run.engine.profile.as_deref(), Some("ai"));
        assert_eq!(run.project, Some(PathBuf::from(".")));
        assert_eq!(run.command, ["ls", "-la"]);
        assert_eq!(run.network, None);
    }

    #[test]
    fn network_flag() {
        let cli = Cli::try_parse_from(["podbay", "run", "--network", "full", "."]).unwrap();
        let Command::Run(run) = cli.command else {
            panic!()
        };
        assert_eq!(run.network, Some(NetworkMode::Full));
        assert!(Cli::try_parse_from(["podbay", "run", "--network", "lan"]).is_err());
    }

    #[test]
    fn online_and_offline() {
        let flag = |args: &[&str]| {
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::Run(run) = cli.command else {
                panic!()
            };
            run.network_flag()
        };
        assert_eq!(flag(&["podbay", "run", "."]), None);
        assert_eq!(
            flag(&["podbay", "run", "--online", "."]),
            Some((NetworkMode::Full, "--online"))
        );
        assert_eq!(
            flag(&["podbay", "run", "--offline", "."]),
            Some((NetworkMode::None, "--offline"))
        );
        assert_eq!(
            flag(&["podbay", "run", "--network", "none", "."]),
            Some((NetworkMode::None, "--network"))
        );
        for conflicting in [
            &["podbay", "run", "--online", "--offline"][..],
            &["podbay", "run", "--online", "--network", "none"],
            &["podbay", "run", "--offline", "--network", "full"],
        ] {
            assert!(Cli::try_parse_from(conflicting).is_err(), "{conflicting:?}");
        }
    }
}
