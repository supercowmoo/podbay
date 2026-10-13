use std::fs;
use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::backend::{Backend, BackendKind};
use crate::cli::{BuildArgs, EngineArgs, ProfileCommand, RunArgs};
use crate::config::Config;
use crate::hash::sha256_hex;
use crate::image::{self, Action, BaseImage, LayerSource};
use crate::paths::Paths;
use crate::plan::{self, MiseData, RunPlan};
use crate::profile::{self, ResolvedProfile};
use crate::repo_key::repo_key;

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

    /// `--profile`, else the configured default, else `default` if it exists,
    /// else nothing.
    fn profile(&self, engine: &EngineArgs) -> Result<ResolvedProfile> {
        let name = engine
            .profile
            .clone()
            .or_else(|| self.config.default_profile.clone())
            .or_else(|| {
                self.paths
                    .profile_file("default")
                    .is_file()
                    .then(|| "default".to_owned())
            });
        let resolved = match name {
            Some(name) => profile::resolve(&self.paths, &name)?,
            None => ResolvedProfile::default(),
        };
        for warning in &resolved.warnings {
            eprintln!("podbay: warning: {warning}");
        }
        Ok(resolved)
    }

    fn base_image(&self) -> BaseImage {
        match &self.config.base_image {
            Some(image) => BaseImage::Custom(image.clone()),
            None => BaseImage::Bundled,
        }
    }
}

pub fn run(args: RunArgs) -> Result<()> {
    let ctx = Setup::load()?;
    let backend = ctx.backend(&args.engine)?;

    let project = args.project.unwrap_or_else(|| PathBuf::from("."));
    let project = fs::canonicalize(&project)
        .with_context(|| format!("project {} not found", project.display()))?;
    if !project.is_dir() {
        bail!("project {} is not a directory", project.display());
    }
    let key = repo_key(&project);

    let profile = ctx.profile(&args.engine)?;
    let stack = image::layers(&ctx.base_image(), &profile.extensions)?;
    let actions = image::ensure(&backend, &stack, false, args.dry_run)?;
    let image = stack.last().expect("at least the base layer").tag.clone();

    let identity = backend.identity()?;
    for note in &identity.notes {
        eprintln!("podbay: note: {note}");
    }

    let mise_data = if args.local_cache {
        let dir = ctx.paths.cache_dir.join("mise").join(&key);
        if !args.dry_run {
            fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        MiseData::Local(dir)
    } else {
        MiseData::Shared
    };
    let mise_config = mise_config(&ctx.paths, &profile.tools, args.dry_run)?;

    let plan = RunPlan::build(plan::Inputs {
        image,
        project,
        repo_key: &key,
        profile: &profile,
        identity: &identity,
        mise_data,
        mise_config,
        command: args.command,
        tty: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
    })?;
    let program = backend.kind().program();
    let argv = plan.args();

    if args.dry_run {
        if actions.contains(&Action::Build) {
            eprintln!("podbay: images would be built first; see `podbay build`");
        }
        let words: Vec<String> = std::iter::once(program.to_owned())
            .chain(argv)
            .map(|a| plan::shell_quote(&a))
            .collect();
        println!("{}", words.join(" "));
        return Ok(());
    }

    let chain = match profile.chain.is_empty() {
        true => "no profile".to_owned(),
        false => format!("profile {}", profile.chain.join(" > ")),
    };
    eprintln!("podbay: {key} with {chain} on {program}");
    let err = Command::new(program).args(&argv).exec();
    Err(err).with_context(|| format!("running {program}"))
}

/// Writes the profile's tools as a mise config file, named by its content so
/// concurrent sessions with different profiles never overwrite each other.
fn mise_config(paths: &Paths, tools: &toml::Table, dry_run: bool) -> Result<Option<PathBuf>> {
    if tools.is_empty() {
        return Ok(None);
    }
    let mut doc = toml::Table::new();
    doc.insert("tools".into(), toml::Value::Table(tools.clone()));
    let text = toml::to_string(&doc)?;
    let dir = paths.cache_dir.join("mise-config");
    let file = dir.join(format!("{}.toml", &sha256_hex(text.as_bytes())[..16]));
    if !dry_run && !file.exists() {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        fs::write(&file, text).with_context(|| format!("writing {}", file.display()))?;
    }
    Ok(Some(file))
}

pub fn build(args: BuildArgs) -> Result<()> {
    let ctx = Setup::load()?;
    let backend = ctx.backend(&args.engine)?;
    let profile = ctx.profile(&args.engine)?;
    let stack = image::layers(&ctx.base_image(), &profile.extensions)?;
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
        ProfileCommand::Show { name } => {
            let resolved = profile::resolve(&ctx.paths, &name)?;
            let stack = image::layers(&ctx.base_image(), &resolved.extensions)?;
            print!("{}", describe(&ctx.paths, &resolved, &stack));
        }
    }
    Ok(())
}

fn describe(paths: &Paths, p: &ResolvedProfile, stack: &[image::Layer]) -> String {
    let home = |path: &Path| match path.strip_prefix(&paths.home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    };
    let mut out = format!("chain: {}\n", p.chain.join(" > "));

    out.push_str("\nimage:\n");
    for layer in stack {
        let source = match &layer.source {
            LayerSource::Bundled => "bundled base".to_owned(),
            LayerSource::Custom => "custom base".to_owned(),
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
        out.push_str(&format!(
            "  {} -> {}  (from {})\n",
            v.name, v.container, v.profile
        ));
    }

    out.push_str("\ntools:\n");
    for (tool, version) in &p.tools {
        out.push_str(&format!("  {tool} = {version}\n"));
    }

    for w in &p.warnings {
        out.push_str(&format!("\nwarning: {w}\n"));
    }
    out
}
