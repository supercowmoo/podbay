use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::backend::{Backend, BackendKind, IMAGE_DEV_UID, Identity};
use crate::cli::{
    BuildArgs, CleanArgs, DoctorArgs, EngineArgs, ProfileCommand, ProvisionArgs, RunArgs,
};
use crate::config::{Config, DEFAULT_PIDS, Limits, Network, NetworkMode, NetworkSetting};
use crate::egress;
use crate::forward::{self, Forwarder, Port};
use crate::gitguard;
use crate::image::{self, Action, BaseImage, LayerSource};
use crate::paths::Paths;
use crate::plan::{self, Net, RunPlan};
use crate::profile::{self, ResolvedProfile};
use crate::provision;
use crate::repo_key::repo_key;
use crate::report;
use crate::seccomp;

struct Setup {
    paths: Paths,
    config: Config,
}

impl Setup {
    fn load() -> Result<Self> {
        let paths = Paths::from_env()?;
        let config = Config::load(&paths.config_file())?;
        Ok(Self { paths, config })
    }

    fn backend(&self, engine: &EngineArgs) -> Result<crate::backend::Cli> {
        let kind = match engine.backend.or(self.config.backend) {
            Some(kind) => kind,
            None => BackendKind::detect()?,
        };
        Ok(crate::backend::Cli::new(kind))
    }

    /// `--profile`, else the `[projects]` entry for `project`, else the
    /// configured default, else `default` if it exists, else nothing.
    /// Several are stacked in order.
    fn profile(&self, engine: &EngineArgs, project: Option<&Path>) -> Result<ResolvedProfile> {
        let listed = project.and_then(|p| self.config.project_profiles(&self.paths, p));
        let names = if !engine.profiles.is_empty() {
            engine.profiles.clone()
        } else if let Some((dir, names)) = listed {
            eprintln!(
                "podbay: profiles {} for projects in {dir} (config.toml)",
                names.join(", ")
            );
            names.to_vec()
        } else if !self.config.default_profile.is_empty() {
            self.config.default_profile.clone()
        } else if self.paths.profile_file("default").is_file() {
            vec!["default".to_owned()]
        } else {
            Vec::new()
        };
        let resolved = profile::resolve_all(&self.paths, &names)?;
        for warning in &resolved.warnings {
            eprintln!("podbay: warning: {warning}");
        }
        for note in &resolved.overrides {
            eprintln!("podbay: note: {note}");
        }
        Ok(resolved)
    }

    /// The default pids limit, then config.toml, then the profile chain.
    fn limits(&self, profile: &ResolvedProfile) -> Limits {
        let mut limits = Limits {
            pids: Some(DEFAULT_PIDS),
            ..Default::default()
        };
        limits.merge(&self.config.limits);
        limits.merge(&profile.limits);
        limits
    }

    /// config.toml, then the profile chain, then the command line, each
    /// applied on top of the last; with where the result came from, for
    /// messages.
    fn network(
        &self,
        profile: &ResolvedProfile,
        flag: Option<(NetworkMode, &str)>,
    ) -> (Network, String) {
        let mut network = Network::default();
        let mut sources: Vec<String> = Vec::new();
        let mut apply = |setting: &NetworkSetting, source: String| {
            // Host lists add up; anything else starts over.
            if !matches!(
                (setting, &network),
                (NetworkSetting::Allow(_), Network::Allow(_))
            ) {
                sources.clear();
            }
            network.apply(setting);
            sources.push(source);
        };
        if let Some(setting) = &self.config.network {
            apply(setting, "config.toml".into());
        }
        for (setting, name) in &profile.network {
            apply(setting, format!("profile {name}"));
        }
        if let Some((mode, name)) = flag {
            apply(&NetworkSetting::Mode(mode), name.into());
        }
        let profiles: Vec<&str> = sources
            .iter()
            .filter_map(|s| s.strip_prefix("profile "))
            .collect();
        let source = match sources.len() {
            0 => "default".to_owned(),
            n if n > 1 && profiles.len() == n => format!("profiles {}", profiles.join(", ")),
            _ => sources.join(", "),
        };
        (network, source)
    }

    /// The profiles' `image`, else config.toml's `base_image`, else the
    /// bundled base.
    fn base_image(&self, profile: &ResolvedProfile) -> BaseImage {
        if let Some((image, _)) = &profile.image {
            return BaseImage::Custom(image.clone());
        }
        match &self.config.base_image {
            Some(image) => BaseImage::Custom(image.clone()),
            None => BaseImage::Bundled,
        }
    }
}

/// What both a session and provisioning need: the project, its profile,
/// and the image.
struct Prepared {
    backend: crate::backend::Cli,
    project: PathBuf,
    key: String,
    profile: ResolvedProfile,
    identity: Identity,
    image: String,
    actions: Vec<Action>,
    limits: Limits,
    /// `--local-cache`: provision volumes are this project's own copies.
    local_cache: bool,
    /// podbay's seccomp filter with the profile's additions, or `None` when
    /// the profile chose the engine's default.
    seccomp: Option<PathBuf>,
}

impl Prepared {
    fn new(
        ctx: &Setup,
        engine: &EngineArgs,
        project: Option<PathBuf>,
        local_cache: bool,
        dry_run: bool,
    ) -> Result<Self> {
        let backend = ctx.backend(engine)?;

        let project = project.unwrap_or_else(|| PathBuf::from("."));
        let project = fs::canonicalize(&project)
            .with_context(|| format!("project {} not found", project.display()))?;
        if !project.is_dir() {
            bail!("project {} is not a directory", project.display());
        }
        let key = repo_key(&project);

        let mut profile = ctx.profile(engine, Some(&project))?;
        if local_cache {
            profile.localize_provision_volumes(&key);
        }
        let identity = backend.identity()?;
        for note in &identity.notes {
            eprintln!("podbay: note: {note}");
        }
        let stack = image::layers(
            &ctx.base_image(&profile),
            &profile.extensions,
            identity.uid,
            identity.gid,
        )?;
        let actions = image::ensure(&backend, &stack, false, dry_run)?;
        let image = stack.last().expect("at least the base layer").tag.clone();

        let limits = applicable_limits(&backend, ctx.limits(&profile), &ctx.config.limits)?;

        let seccomp = match &profile.engine_seccomp {
            Some(_) => None,
            None => {
                let names: Vec<String> = profile.syscalls.iter().map(|s| s.name.clone()).collect();
                let json = seccomp::profile(&names);
                Some(match dry_run {
                    true => seccomp::path(&ctx.paths, &json),
                    false => seccomp::write(&ctx.paths, &json)?,
                })
            }
        };

        Ok(Self {
            backend,
            project,
            key,
            profile,
            identity,
            image,
            actions,
            limits,
            local_cache,
            seccomp,
        })
    }

    fn program(&self) -> &'static str {
        self.backend.kind().program()
    }

    /// The provisioning fingerprint, or `None` if there's nothing to install.
    fn fingerprint(&self) -> Option<String> {
        provision::fingerprint(&self.project, &self.image, &self.profile, self.local_cache)
    }

    /// A container that installs the project's tools: network on, the
    /// project read-only, the provision volumes and the profile's
    /// environment, and none of its other mounts, volumes or capabilities.
    fn provision_plan(&self) -> Result<RunPlan> {
        RunPlan::build(plan::Inputs {
            image: self.image.clone(),
            project: self.project.clone(),
            project_readonly: true,
            repo_key: &self.key,
            profile: &self.profile.for_provisioning(),
            identity: &self.identity,
            command: vec!["sh".into(), "-c".into(), provision::script(&self.profile)],
            tty: is_tty(),
            caps: &[],
            limits: &self.limits,
            network: Net::Full,
            seccomp: self.seccomp.clone(),
            // The whole project is read-only here already.
            protected: &[],
        })
    }

    /// Runs provisioning, and records it when it succeeds.
    fn provision(&self, ctx: &Setup, fingerprint: Option<&str>) -> Result<ExitStatus> {
        let argv = self.provision_plan()?.args();
        let program = self.program();
        eprintln!(
            "podbay: provisioning tools for {} on {program} (network on, project read-only)",
            self.key
        );
        let child = Command::new(program)
            .args(&argv)
            .spawn()
            .with_context(|| format!("running {program}"))?;
        let status =
            wait_ignoring_interrupts(child).with_context(|| format!("waiting for {program}"))?;
        if status.success()
            && let Some(fingerprint) = fingerprint
        {
            provision::record(&ctx.paths, &self.key, self.local_cache, fingerprint)?;
        }
        Ok(status)
    }
}

fn is_tty() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Prints `program argv...` as one shell command line.
fn print_command(program: &str, argv: Vec<String>) {
    let words: Vec<String> = std::iter::once(program.to_owned())
        .chain(argv)
        .map(|a| plan::shell_quote(&a))
        .collect();
    println!("{}", words.join(" "));
}

/// True if the engine was stopped by Ctrl-C (or Ctrl-\\).
fn interrupted(status: ExitStatus) -> bool {
    matches!(status.code(), Some(130 | 131))
        || matches!(status.signal(), Some(libc::SIGINT | libc::SIGQUIT))
}

pub fn run(args: RunArgs) -> Result<ExitCode> {
    let ctx = Setup::load()?;
    let network_flag = args.network_flag();
    let prep = Prepared::new(
        &ctx,
        &args.engine,
        args.project,
        args.local_cache,
        args.dry_run,
    )?;
    let profile = &prep.profile;
    let (network, network_source) = ctx.network(profile, network_flag);

    if !profile.caps.is_empty() {
        let caps: Vec<String> = profile
            .caps
            .iter()
            .map(|c| format!("{} (from {})", c.name, c.profile))
            .collect();
        eprintln!(
            "podbay: note: the session keeps capabilities {}",
            caps.join(", ")
        );
    }
    let caps: Vec<String> = profile.caps.iter().map(|c| c.name.clone()).collect();
    if let Some(source) = &profile.engine_seccomp {
        eprintln!(
            "podbay: note: the session uses {}'s default seccomp filter instead of podbay's \
             (from {source})",
            prep.program()
        );
    } else if !profile.syscalls.is_empty() {
        let syscalls: Vec<String> = profile
            .syscalls
            .iter()
            .map(|s| format!("{} (from {})", s.name, s.profile))
            .collect();
        eprintln!(
            "podbay: note: the seccomp filter also allows {}",
            syscalls.join(", ")
        );
    }

    let mut ports: Vec<Port> = profile.ports.iter().map(|(p, _)| *p).collect();
    for port in &args.ports {
        forward::add(&mut ports, *port)?;
    }
    if let Network::Allow(_) = &network
        && let Some(port) = ports.iter().find(|p| p.container == egress::PROXY_PORT)
    {
        bail!(
            "session port {} is where podbay's proxy listens in an allowlisted session; \
             forward a different one",
            port.container
        );
    }
    let needs_relay = matches!(network, Network::Allow(_)) || !ports.is_empty();

    let relay_name = format!("podbay-relay-{}-{}", prep.key, std::process::id());
    let relay_image = match needs_relay {
        true => {
            let layer = image::relay_layer();
            let actions = image::ensure(
                &prep.backend,
                std::slice::from_ref(&layer),
                false,
                args.dry_run,
            )?;
            if args.dry_run && actions.contains(&Action::Build) {
                eprintln!("podbay: the network relay image would be built first");
            }
            Some(layer.tag)
        }
        false => None,
    };
    let net = match (&network, needs_relay) {
        (Network::Allow(_), _) => Net::Relay {
            container: relay_name.clone(),
            proxy: true,
        },
        (_, true) => Net::Relay {
            container: relay_name.clone(),
            proxy: false,
        },
        (Network::None, false) => Net::None,
        (Network::Full, false) => Net::Full,
    };

    // Variables from your environment go to the session only, by name: the
    // engine reads the values, so they never appear in podbay's output.
    let mut session_profile = profile.clone();
    session_profile.env_from_host.retain(|(key, from)| {
        let set = std::env::var_os(key).is_some();
        if !set {
            eprintln!(
                "podbay: warning: {key} isn't set in your environment (env_from_host in \
                 profile {from}); the session won't have it"
            );
        }
        set
    });
    if !session_profile.env_from_host.is_empty() {
        let names: Vec<&str> = session_profile
            .env_from_host
            .iter()
            .map(|(k, _)| k.as_str())
            .collect();
        eprintln!(
            "podbay: passing {} from your environment to the session",
            names.join(", ")
        );
    }

    // Git hooks and config, read-only unless a profile says otherwise.
    let guard = match &profile.protect_git {
        Some((false, from)) => {
            eprintln!(
                "podbay: note: the session can change git hooks and config (protect_git = false \
                 from profile {from}); check the session report before using git here"
            );
            gitguard::Guard::default()
        }
        _ => gitguard::guard(&prep.project, !args.dry_run),
    };
    for warning in &guard.warnings {
        eprintln!("podbay: warning: {warning}");
    }
    if guard.protects() {
        eprintln!("podbay: git hooks and config are read-only in the session (protect_git)");
    }

    // With `writable`, the project is read-only apart from those paths.
    let writable = gitguard::writable(&prep.project, &profile.writable, !args.dry_run)?;
    let project_readonly = !profile.writable.is_empty();
    if project_readonly {
        let listed: Vec<&str> = profile.writable.iter().map(|(p, _)| p.as_str()).collect();
        eprintln!(
            "podbay: the project is read-only in the session except {}",
            listed.join(", ")
        );
        if !guard.mounts.iter().any(|m| m.target.ends_with("/.git"))
            && prep.project.join(".git").is_dir()
            && !listed.contains(&".git")
        {
            eprintln!(
                "podbay: note: .git is read-only too, so git can't commit; add \".git\" to \
                 writable to allow it"
            );
        }
    }
    // Git protection's mounts go over the writable ones: `.git` listed
    // in `writable` is already mounted writable by it.
    let mut inner: Vec<plan::InnerMount> = writable
        .into_iter()
        .filter(|w| !guard.mounts.iter().any(|g| g.target == w.target))
        .collect();
    inner.extend(guard.mounts.iter().cloned());

    let plan = RunPlan::build(plan::Inputs {
        image: prep.image.clone(),
        project: prep.project.clone(),
        project_readonly,
        repo_key: &prep.key,
        profile: &session_profile,
        identity: &prep.identity,
        command: args.command,
        tty: is_tty(),
        caps: &caps,
        limits: &prep.limits,
        network: net,
        seccomp: prep.seccomp.clone(),
        protected: &inner,
    })?;
    let program = prep.program();
    let argv = plan.args();

    let due = prep
        .fingerprint()
        .filter(|f| provision::is_due(&ctx.paths, &prep.key, prep.local_cache, f));

    if args.dry_run {
        if prep.actions.contains(&Action::Build) {
            eprintln!("podbay: images would be built first; see `podbay build`");
        }
        if due.is_some() {
            eprintln!("podbay: tools would be provisioned first; see `podbay provision --dry-run`");
        }
        if let Network::Allow(_) = &network {
            eprintln!(
                "podbay: the session would share the network of relay container {relay_name}, \
                 with podbay's proxy on {}",
                egress::PROXY_ADDR
            );
        } else if needs_relay {
            eprintln!(
                "podbay: the session would share the network of relay container {relay_name}"
            );
        }
        for port in &ports {
            eprintln!(
                "podbay: 127.0.0.1:{} on the host would be forwarded to session port {}",
                port.host, port.container
            );
        }
        print_command(program, argv);
        return Ok(ExitCode::SUCCESS);
    }

    // Taken ports fail the run here, before provisioning or the session.
    let forwarder = match ports.is_empty() {
        true => None,
        false => Some(Forwarder::start(&ports)?),
    };

    if let Some(fingerprint) = &due {
        let status = prep.provision(&ctx, Some(fingerprint))?;
        if interrupted(status) {
            return Ok(exit_code(status));
        }
        if !status.success() {
            eprintln!(
                "podbay: warning: provisioning failed ({status}); the session may be missing \
                 tools. Retry with `podbay provision`"
            );
        }
    }

    let project = &prep.project;
    let key = &prep.key;
    match &network {
        // Full access you chose yourself, in config.toml or on the command
        // line, is just stated. A profile turning it on gets a warning:
        // that's the case you might not expect.
        Network::Full if network_source.starts_with("profile") => {
            eprintln!(
                "podbay: warning: the session has full network access (network = \"full\" \
                 from {network_source})"
            );
        }
        Network::Full => eprintln!("podbay: network: full (from {network_source})"),
        Network::Allow(_) => eprintln!(
            "podbay: network: HTTPS to {} only (from {network_source})",
            network.describe().trim_start_matches("allow ")
        ),
        Network::None => {}
    }
    let relay = match relay_image {
        Some(image) => {
            let allow = match &network {
                Network::Allow(hosts) => Some(hosts.clone()),
                _ => None,
            };
            let relay = Relay::start(
                &ctx.paths,
                &prep,
                RelayJobs {
                    allow,
                    forwarder,
                    full_network: matches!(network, Network::Full),
                },
                &image,
                relay_name,
            )?;
            if let Some(forwarder) = &relay.forwarder {
                eprint!("{}", forwarder.describe());
            }
            Some(relay)
        }
        None => None,
    };
    let chain = match profile.chain.is_empty() {
        true => "no profile".to_owned(),
        false => format!("profile {}", profile.chain.join(" > ")),
    };
    eprintln!("podbay: {key} with {chain} on {program}");

    let rw_mounts: Vec<PathBuf> = profile
        .mounts
        .iter()
        .filter(|m| !m.readonly)
        .map(|m| m.host.clone())
        .collect();
    let watch = report::Watch::begin(project, &rw_mounts);
    let child = Command::new(program)
        .args(&argv)
        .spawn()
        .with_context(|| format!("running {program}"))?;
    let status =
        wait_ignoring_interrupts(child).with_context(|| format!("waiting for {program}"))?;

    let network_summary = relay.map(Relay::stop);
    let report = watch.finish().render(project);
    let mut stderr = io::stderr().lock();
    if stderr.is_terminal() {
        // The container controlled the terminal until now. Undo text styling
        // and character set switches so the report can't be hidden by them.
        let _ = stderr.write_all(b"\x1b[0m\x1b(B");
    }
    let _ = stderr.write_all(report.as_bytes());
    if let Some(summary) = network_summary {
        let _ = stderr.write_all(summary.as_bytes());
    }
    Ok(exit_code(status))
}

/// What a relay container does for its session.
struct RelayJobs {
    /// The allowlist, for a session that has one.
    allow: Option<Vec<egress::HostPattern>>,
    /// Forwarded ports, already listening on the host.
    forwarder: Option<Forwarder>,
    /// The session has full network access, which it gets through the
    /// relay's namespace.
    full_network: bool,
}

/// The relay container a session shares its network namespace with, when
/// it has an allowlist or forwarded ports, and the host side of both. The
/// container is removed when this is dropped, however the session ended.
struct Relay {
    program: &'static str,
    name: String,
    server: Option<egress::Server>,
    forwarder: Option<Forwarder>,
}

impl Relay {
    fn start(
        paths: &Paths,
        prep: &Prepared,
        jobs: RelayJobs,
        image: &str,
        name: String,
    ) -> Result<Self> {
        // podbay's own binary: the base filter, whatever the profile says.
        let filter = seccomp::write(paths, &seccomp::profile(&[]))?;
        let server = jobs.allow.map(egress::Server::start).transpose()?;
        let relay = Self {
            program: prep.program(),
            name,
            server,
            forwarder: jobs.forwarder,
        };
        let container_ports = relay
            .forwarder
            .as_ref()
            .map(Forwarder::container_ports)
            .unwrap_or_default();
        let args = plan::relay_args(&plan::RelayInputs {
            image,
            name: &relay.name,
            egress_dir: relay.server.as_ref().map(egress::Server::dir),
            forward: relay
                .forwarder
                .as_ref()
                .map(|f| (f.dir(), container_ports.as_slice())),
            full_network: jobs.full_network,
            identity: &prep.identity,
            repo_key: &prep.key,
            limits: &prep.limits,
            seccomp: Some(&filter),
        })?;
        let status = Command::new(relay.program)
            .args(&args)
            .stdout(Stdio::null())
            .status()
            .with_context(|| format!("running {}", relay.program))?;
        if !status.success() {
            bail!("starting the network relay container failed ({status})");
        }
        let timeout = Duration::from_secs(20);
        let ready = relay.server.as_ref().is_none_or(|s| s.wait_ready(timeout))
            && relay
                .forwarder
                .as_ref()
                .is_none_or(|f| f.wait_ready(timeout));
        if !ready {
            bail!("the network relay container did not start within 20 seconds");
        }
        Ok(relay)
    }

    /// Removes the container and returns what the session connected to,
    /// and what connected to it.
    fn stop(mut self) -> String {
        let mut summary = String::new();
        if let Some(server) = &self.server {
            summary.push_str(&server.summary());
        }
        // Stop listening before the summary, so the counts are final.
        if let Some(forwarder) = self.forwarder.take() {
            let text = forwarder.summary();
            drop(forwarder);
            summary.push_str(&text);
        }
        drop(self);
        summary
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = Command::new(self.program)
            .args(["rm", "--force", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Waits for the engine with terminal interrupts ignored, as `system(3)`
/// does: Ctrl-C still reaches the engine, which passes it to the container,
/// and podbay stays alive to print the session report.
fn wait_ignoring_interrupts(mut child: Child) -> io::Result<ExitStatus> {
    // SAFETY: setting SIG_IGN installs no handler code. The child was
    // spawned before this, so it keeps the default dispositions.
    let previous = unsafe {
        [
            libc::signal(libc::SIGINT, libc::SIG_IGN),
            libc::signal(libc::SIGQUIT, libc::SIG_IGN),
        ]
    };
    let status = child.wait();
    // SAFETY: restores the dispositions returned above.
    unsafe {
        libc::signal(libc::SIGINT, previous[0]);
        libc::signal(libc::SIGQUIT, previous[1]);
    }
    status
}

/// The container's exit status as podbay's own, shell style: 128 + the
/// signal number if the engine was killed.
fn exit_code(status: ExitStatus) -> ExitCode {
    match (status.code(), status.signal()) {
        (Some(code), _) => ExitCode::from(code as u8),
        (None, Some(signal)) => ExitCode::from(128u8.wrapping_add(signal as u8)),
        (None, None) => ExitCode::FAILURE,
    }
}

/// Drops the limits the engine has no cgroup controller for, with a warning:
/// rootless engines only get the controllers systemd delegates to the user.
fn applicable_limits(
    backend: &dyn Backend,
    mut limits: Limits,
    configured: &Limits,
) -> Result<Limits> {
    let Some(controllers) = backend.cgroup_controllers()? else {
        return Ok(limits);
    };
    let has = |c: &str| controllers.iter().any(|have| have == c);
    let program = backend.kind().program();
    let skip = |what: &str, controller: &str| {
        eprintln!(
            "podbay: warning: {program} cannot apply the {what} limit for your user (no \
             {controller} cgroup controller); running without it"
        );
    };
    if limits.pids.is_some_and(|n| n > 0) && !has("pids") {
        skip("process", "pids");
        if configured.pids.is_none() {
            eprintln!(
                "podbay: hint: delegate the pids controller to your user, or set \
                 `[limits] pids = 0` in config.toml to silence this"
            );
        }
        limits.pids = None;
    }
    if limits.memory.is_some() && !has("memory") {
        skip("memory", "memory");
        limits.memory = None;
    }
    if limits.cpus.is_some() && !has("cpu") {
        skip("cpu", "cpu");
        limits.cpus = None;
    }
    Ok(limits)
}

pub fn provision(args: ProvisionArgs) -> Result<ExitCode> {
    let ctx = Setup::load()?;
    let prep = Prepared::new(
        &ctx,
        &args.engine,
        args.project,
        args.local_cache,
        args.dry_run,
    )?;
    if prep.profile.provision.is_empty() {
        eprintln!("podbay: the profile has no [provision] command; nothing to provision");
        return Ok(ExitCode::SUCCESS);
    }
    if args.dry_run {
        if prep.actions.contains(&Action::Build) {
            eprintln!("podbay: images would be built first; see `podbay build`");
        }
        print_command(prep.program(), prep.provision_plan()?.args());
        return Ok(ExitCode::SUCCESS);
    }
    let status = prep.provision(&ctx, prep.fingerprint().as_deref())?;
    Ok(exit_code(status))
}

pub fn build(args: BuildArgs) -> Result<()> {
    let ctx = Setup::load()?;
    let backend = ctx.backend(&args.engine)?;
    let current = std::env::current_dir().and_then(fs::canonicalize).ok();
    let profile = ctx.profile(&args.engine, current.as_deref())?;
    let identity = backend.identity()?;
    let stack = image::layers(
        &ctx.base_image(&profile),
        &profile.extensions,
        identity.uid,
        identity.gid,
    )?;
    let actions = image::ensure(&backend, &stack, args.force, false)?;
    for (layer, action) in stack.iter().zip(actions) {
        let state = match action {
            Action::Build => "built",
            Action::Present => "up to date",
        };
        eprintln!("podbay: {} ({state})", layer.tag);
    }
    println!("{}", stack.last().expect("at least the base layer").tag);
    Ok(())
}

/// `podbay clean`: see [`crate::clean`].
pub fn clean(args: CleanArgs) -> Result<ExitCode> {
    let ctx = Setup::load()?;
    let backend = ctx.backend(&EngineArgs {
        backend: args.backend,
        profiles: Vec::new(),
    })?;
    let program = backend.kind().program();
    let would = if args.dry_run {
        "would remove"
    } else {
        "removed"
    };
    let mut failed = false;

    // Relay containers whose podbay is gone.
    for (name, _running) in backend.list_containers("podbay-relay-")? {
        let alive = crate::clean::relay_owner(&name)
            .is_some_and(|pid| crate::clean::owner_alive(Path::new("/proc"), pid));
        if alive {
            continue;
        }
        if !args.dry_run
            && let Err(e) = backend.remove_container(&name)
        {
            eprintln!("podbay: couldn't remove container {name}: {e:#}");
            failed = true;
            continue;
        }
        println!("{would} container {name} (its podbay process is gone)");
    }

    // Images, except what this podbay would otherwise rebuild from scratch.
    let keep = [image::bundled_base_tag(), image::relay_layer().tag];
    let images = backend.list_images()?;
    let mut removed = 0;
    for image in crate::clean::images_to_remove(&images, &keep) {
        if !args.dry_run && backend.remove_image(&image).is_err() {
            println!("kept image {image} (in use)");
            continue;
        }
        println!("{would} image {image}");
        removed += 1;
    }
    let kept: Vec<&String> = images
        .iter()
        .filter(|i| {
            let bare = i.strip_prefix("localhost/").unwrap_or(i);
            keep.iter().any(|k| k == bare)
        })
        .collect();
    for image in kept {
        println!("kept image {image} (this podbay's base or relay)");
    }

    // Seccomp filters, written again when a session needs one.
    let seccomp = ctx.paths.cache_dir.join("seccomp");
    let filters = fs::read_dir(&seccomp)
        .map(|entries| entries.flatten().count())
        .unwrap_or(0);
    if filters > 0 {
        if !args.dry_run {
            fs::remove_dir_all(&seccomp)
                .with_context(|| format!("removing {}", seccomp.display()))?;
        }
        println!("{would} {filters} cached seccomp filters");
    }

    let volumes: Vec<String> = backend
        .list_volumes()?
        .into_iter()
        .filter(|v| v.starts_with("podbay-"))
        .collect();
    if !volumes.is_empty() {
        println!(
            "\nkept {} volumes, which podbay never removes (they hold logins, history and \
             installed tools):",
            volumes.len()
        );
        for v in &volumes {
            println!("  {v}");
        }
        println!("remove one you no longer need with `{program} volume rm NAME`");
    }
    if removed == 0 && filters == 0 && !args.dry_run {
        println!("nothing to remove");
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `podbay doctor`: the engine, this machine, config.toml and every
/// profile. Exit status 1 if anything would stop sessions from working.
pub fn doctor(args: DoctorArgs) -> Result<ExitCode> {
    use crate::doctor::{self, Check, Level};
    let paths = Paths::from_env()?;
    let mut checks: Vec<Check> = Vec::new();
    let config = match Config::load(&paths.config_file()) {
        Ok(config) => config,
        Err(e) => {
            checks.push(Check {
                level: Level::Fail,
                what: "config",
                detail: format!("{e:#}"),
                hint: None,
            });
            Config::default()
        }
    };

    let (kind, source) = match (args.backend, config.backend) {
        (Some(kind), _) => (Some(kind), "--backend"),
        (None, Some(kind)) => (Some(kind), "config.toml"),
        (None, None) => (BackendKind::detect().ok(), "detected"),
    };
    match kind {
        Some(kind) => {
            checks.extend(doctor::engine(&crate::backend::Cli::new(kind), source));
            checks.extend(doctor::host(kind));
        }
        None => checks.push(Check {
            level: Level::Fail,
            what: "engine",
            detail: "neither podman nor docker is on PATH".into(),
            hint: Some("install Podman (preferred, rootless by default) or Docker".into()),
        }),
    }

    if !checks.iter().any(|c| c.what == "config") {
        checks.push(Check {
            level: Level::Ok,
            what: "config",
            detail: match paths.config_file().is_file() {
                true => format!("{} loads", paths.config_file().display()),
                false => format!("no {} (all defaults)", paths.config_file().display()),
            },
            hint: None,
        });
    }
    let names = profile::list(&paths)?;
    let mut broken = 0;
    for name in &names {
        match profile::resolve_all(&paths, std::slice::from_ref(name)) {
            Ok(resolved) => {
                for warning in resolved.warnings {
                    checks.push(Check {
                        level: Level::Warn,
                        what: "profile",
                        detail: warning,
                        hint: None,
                    });
                }
            }
            Err(e) => {
                broken += 1;
                checks.push(Check {
                    level: Level::Fail,
                    what: "profile",
                    detail: format!("{name}: {e:#}"),
                    hint: None,
                });
            }
        }
    }
    if broken == 0 {
        checks.push(Check {
            level: Level::Ok,
            what: "profiles",
            detail: match names.len() {
                0 => format!("none yet in {}", paths.profiles_dir().display()),
                n => format!("{n} load: {}", names.join(", ")),
            },
            hint: None,
        });
    }

    print!("{}", doctor::render(&checks));
    Ok(match checks.iter().any(|c| c.level == Level::Fail) {
        true => ExitCode::FAILURE,
        false => ExitCode::SUCCESS,
    })
}

pub fn profile(cmd: ProfileCommand) -> Result<()> {
    let ctx = Setup::load()?;
    match cmd {
        ProfileCommand::Init { name, extends } => {
            let file = profile::init(&ctx.paths, &name, &extends)?;
            println!("{}", file.display());
            eprintln!("podbay: created profile {name:?}; edit it, then `podbay run -p {name}`");
        }
        ProfileCommand::List => {
            for name in profile::list(&ctx.paths)? {
                println!("{name}");
            }
        }
        ProfileCommand::Show { names } => {
            let resolved = profile::resolve_all(&ctx.paths, &names)?;
            let stack = image::layers(
                &ctx.base_image(&resolved),
                &resolved.extensions,
                IMAGE_DEV_UID,
                IMAGE_DEV_UID,
            )?;
            let limits = ctx.limits(&resolved);
            let network = ctx.network(&resolved, None);
            print!(
                "{}",
                describe(&ctx.paths, &resolved, &stack, &limits, &network)
            );
        }
    }
    Ok(())
}

fn describe(
    paths: &Paths,
    p: &ResolvedProfile,
    stack: &[image::Layer],
    limits: &Limits,
    (network, source): &(Network, String),
) -> String {
    let home = |path: &Path| match path.strip_prefix(&paths.home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    };
    let mut out = format!("chain: {}\n", p.chain.join(" > "));

    out.push_str("\nimage:\n");
    for layer in stack {
        let source = match &layer.source {
            LayerSource::Bundled => "bundled base".to_owned(),
            LayerSource::Relay => "network relay".to_owned(),
            LayerSource::Adapter { local: true, .. } => {
                "podbay-init and the dev user added".to_owned()
            }
            LayerSource::Adapter { image, .. } => {
                let from = match &p.image {
                    Some((_, profile)) => format!("profile {profile}"),
                    None => "config.toml".to_owned(),
                };
                format!("{image} with podbay-init and the dev user added, from {from}")
            }
            LayerSource::Extension { profile, .. } => format!("extension from {profile}"),
        };
        out.push_str(&format!("  {}  ({source})\n", layer.tag));
    }

    out.push_str("\nmounts:\n");
    for m in &p.mounts {
        let mode = if m.readonly { "ro" } else { "rw" };
        out.push_str(&format!(
            "  {} -> {}  ({mode}, from {})\n",
            home(&m.host),
            m.container,
            m.profile
        ));
    }

    out.push_str("\nvolumes:\n");
    for v in &p.volumes {
        let provision = if v.provision { "provision, " } else { "" };
        out.push_str(&format!(
            "  {} -> {}  ({provision}from {})\n",
            v.name, v.container, v.profile
        ));
    }

    out.push_str(&format!(
        "\nshell: {}\n",
        p.shell.as_deref().unwrap_or("image default")
    ));

    out.push_str("\nenv:\n");
    for (k, v) in &p.env {
        out.push_str(&format!("  {k} = {v}\n"));
    }

    out.push_str(&match &p.protect_git {
        Some((false, from)) => format!("\ngit hooks and config: writable  (from {from})\n"),
        Some((true, from)) => format!("\ngit hooks and config: read-only  (from {from})\n"),
        None => "\ngit hooks and config: read-only\n".to_owned(),
    });

    if !p.writable.is_empty() {
        out.push_str("\nproject writable only at:\n");
        for (path, profile) in &p.writable {
            out.push_str(&format!("  {path}  (from {profile})\n"));
        }
    }

    out.push_str("\nenv from your environment (session only):\n");
    for (k, profile) in &p.env_from_host {
        out.push_str(&format!("  {k}  (from {profile})\n"));
    }

    out.push_str("\npath:\n");
    for dir in &p.path {
        out.push_str(&format!("  {dir}\n"));
    }

    out.push_str("\nprovision:\n");
    for step in &p.provision {
        out.push_str(&format!("  {}  (from {})\n", step.command, step.profile));
    }
    if !p.watch.is_empty() {
        out.push_str(&format!("  watching {}\n", p.watch.join(", ")));
    }

    out.push_str("\ncapabilities:\n");
    for c in &p.caps {
        out.push_str(&format!("  {}  (from {})\n", c.name, c.profile));
    }

    match &p.engine_seccomp {
        Some(source) => out.push_str(&format!(
            "\nseccomp: the engine's default  (from {source})\n"
        )),
        None => {
            out.push_str("\nseccomp: podbay's filter\n");
            for s in &p.syscalls {
                out.push_str(&format!("  + {}  (from {})\n", s.name, s.profile));
            }
        }
    }

    out.push_str(&format!(
        "\nnetwork: {}  (from {source})\n",
        network.describe()
    ));

    out.push_str("\nports:\n");
    for (port, profile) in &p.ports {
        out.push_str(&format!(
            "  127.0.0.1:{} -> session port {}  (from {profile})\n",
            port.host, port.container
        ));
    }

    out.push_str("\nlimits:\n");
    let pids = match limits.pids {
        Some(0) | None => "unlimited".to_owned(),
        Some(n) => n.to_string(),
    };
    out.push_str(&format!("  pids = {pids}\n"));
    out.push_str(&format!(
        "  memory = {}\n",
        limits.memory.as_deref().unwrap_or("unlimited")
    ));
    out.push_str(&format!(
        "  cpus = {}\n",
        limits
            .cpus
            .map_or("unlimited".to_owned(), |c| c.to_string())
    ));

    for w in &p.warnings {
        out.push_str(&format!("\nwarning: {w}\n"));
    }
    for note in &p.overrides {
        out.push_str(&format!("\nnote: {note}\n"));
    }
    out
}
