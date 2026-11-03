//! The command line, parsed by hand: a handful of subcommands and flags
//! don't need a parsing library, and help, errors and exit codes stay the
//! same as they were with one.
//!
//! Options may come anywhere before `--`, as `--opt value`, `--opt=value`,
//! `-p value` or `-pvalue`. Everything after `--` is the session's command.
//! Usage errors exit 2, help and the version exit 0.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use crate::backend::BackendKind;
use crate::config::NetworkMode;
use crate::forward::Port;

#[derive(Debug)]
pub struct Cli {
    pub command: Command,
}

#[derive(Debug)]
pub enum Command {
    Run(RunArgs),
    Build(BuildArgs),
    Provision(ProvisionArgs),
    Profile(ProfileCommand),
    Doctor(DoctorArgs),
    Clean(CleanArgs),
}

#[derive(Debug, Default)]
pub struct CleanArgs {
    pub backend: Option<BackendKind>,
    pub dry_run: bool,
}

#[derive(Debug, Default)]
pub struct DoctorArgs {
    pub backend: Option<BackendKind>,
}

#[derive(Debug, Default)]
pub struct EngineArgs {
    pub backend: Option<BackendKind>,
    pub profiles: Vec<String>,
}

#[derive(Debug, Default)]
pub struct RunArgs {
    pub engine: EngineArgs,
    pub local_cache: bool,
    pub network: Option<NetworkMode>,
    pub online: bool,
    pub offline: bool,
    pub ports: Vec<Port>,
    pub dry_run: bool,
    pub project: Option<PathBuf>,
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

#[derive(Debug, Default)]
pub struct ProvisionArgs {
    pub engine: EngineArgs,
    pub local_cache: bool,
    pub dry_run: bool,
    pub project: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct BuildArgs {
    pub engine: EngineArgs,
    pub force: bool,
}

#[derive(Debug)]
pub enum ProfileCommand {
    Init { name: String, extends: Vec<String> },
    List,
    Show { names: Vec<String> },
}

/// Why parsing stopped without a command to run.
#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    /// Help or the version, for stdout; exit 0.
    Print(String),
    /// A usage error, for stderr; exit 2. `help` is printed instead of a
    /// one-line error when no subcommand was given at all.
    Usage(String),
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Print(text) | Self::Usage(text) => f.write_str(text),
        }
    }
}

const VERSION: &str = concat!("podbay ", env!("CARGO_PKG_VERSION"), "\n");

const MAIN_HELP: &str = "\
Disposable, isolated containers around a project

Usage: podbay <COMMAND>

Commands:
  run        Start a container around a project
  build      Build (or rebuild) the images a profile needs
  provision  Run the profile's [provision] commands now, with network access. `run` does this by itself when what they depend on changed
  profile    Manage profiles
  doctor     Check that this machine can run sessions, and what to change if not
  clean      Remove podbay's old images and leftover containers; volumes are kept
  help       Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
";

const ENGINE_OPTIONS: &str = "\
      --backend <BACKEND>  Container engine [default: config `backend`, else podman if installed, else docker] [possible values: podman, docker]
  -p, --profile <NAME>     Profiles to apply, stacked in order: `-p rust -p dsh` or `-p rust,dsh` [default: config `default_profile`, else `default` if it exists]
";

fn run_help() -> String {
    format!(
        "\
Start a container around a project

Usage: podbay run [OPTIONS] [PROJECT] [-- <COMMAND>...]

Arguments:
  [PROJECT]     Project directory to mount [default: current directory]
  [COMMAND]...  Command to run instead of a login shell (the profile's `shell`, else bash)

Options:
{ENGINE_OPTIONS}      --local-cache        Give this project its own copies of the profile's provision volumes instead of sharing them. Use for code you do not trust
      --network <NETWORK>  Network access for the session, overriding any allowlist [default: profile or config `network`, else none]
                           none: loopback only; full: the engine's default network (the internet, the LAN, and depending on the engine, the host)
      --online             Same as `--network full`
      --offline            Same as `--network none`
      --port <PORT>        Make a port in the session reachable on the host's 127.0.0.1, in addition to the profile's `ports`: PORT, or HOST_PORT:SESSION_PORT. Repeatable
      --dry-run            Print the engine command instead of running it. Builds nothing
  -h, --help               Print help
"
    )
}

fn build_help() -> String {
    format!(
        "\
Build (or rebuild) the images a profile needs

Usage: podbay build [OPTIONS]

Options:
{ENGINE_OPTIONS}      --force              Rebuild every layer from scratch: no build cache, and base images pulled again
  -h, --help               Print help
"
    )
}

fn provision_help() -> String {
    format!(
        "\
Run the profile's [provision] commands now, with network access. `run` does this by itself when what they depend on changed

Usage: podbay provision [OPTIONS] [PROJECT]

Arguments:
  [PROJECT]  Project directory [default: current directory]

Options:
{ENGINE_OPTIONS}      --local-cache        Install into this project's own copies of the provision volumes, as `run --local-cache` uses, instead of the shared ones
      --dry-run            Print the engine command instead of running it. Builds nothing
  -h, --help               Print help
"
    )
}

const PROFILE_HELP: &str = "\
Manage profiles

Usage: podbay profile <COMMAND>

Commands:
  init  Create a profile from a commented template
  list  List profiles
  show  Show what profiles resolve to, with everything they extend merged in. Several are stacked as `run -p` would stack them
  help  Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
";

const PROFILE_INIT_HELP: &str = "\
Create a profile from a commented template

Usage: podbay profile init [OPTIONS] <NAME>

Arguments:
  <NAME>  The new profile's name

Options:
      --extends <EXTENDS>  Profiles the new one builds on, comma-separated or repeated
  -h, --help               Print help
";

const PROFILE_LIST_HELP: &str = "\
List profiles

Usage: podbay profile list

Options:
  -h, --help  Print help
";

const PROFILE_SHOW_HELP: &str = "\
Show what profiles resolve to, with everything they extend merged in. Several are stacked as `run -p` would stack them

Usage: podbay profile show <NAMES>...

Arguments:
  <NAMES>...  Profiles, comma-separated or as separate arguments

Options:
  -h, --help  Print help
";

/// A subcommand's usage line, for errors.
#[derive(Clone, Copy)]
struct Usage {
    line: &'static str,
}

const RUN: Usage = Usage {
    line: "podbay run [OPTIONS] [PROJECT] [-- <COMMAND>...]",
};
const BUILD: Usage = Usage {
    line: "podbay build [OPTIONS]",
};
const PROVISION: Usage = Usage {
    line: "podbay provision [OPTIONS] [PROJECT]",
};
const PROFILE_INIT: Usage = Usage {
    line: "podbay profile init [OPTIONS] <NAME>",
};
const PROFILE_LIST: Usage = Usage {
    line: "podbay profile list",
};
const PROFILE_SHOW: Usage = Usage {
    line: "podbay profile show <NAMES>...",
};

const COMMANDS: &[&str] = &[
    "run",
    "build",
    "provision",
    "profile",
    "doctor",
    "clean",
    "help",
];

const CLEAN_HELP: &str = "\
Remove podbay's old images and leftover containers; volumes are kept

Removes relay containers whose podbay process is gone, podbay's images
except the bundled base and relay this podbay uses (the rest are rebuilt
when needed), and cached seccomp filters. Lists podbay's volumes, which it
never removes: they hold logins, history and installed tools.

Usage: podbay clean [OPTIONS]

Options:
      --backend <BACKEND>  Container engine [default: config `backend`, else podman if installed, else docker] [possible values: podman, docker]
      --dry-run            Show what would be removed, and remove nothing
  -h, --help               Print help
";

const CLEAN: Usage = Usage {
    line: "podbay clean [OPTIONS]",
};

fn parse_clean(items: Vec<OsString>) -> Result<CleanArgs, Exit> {
    let mut args = Args::new(items, CLEAN);
    let mut engine = EngineArgs::default();
    let mut out = CleanArgs::default();
    loop {
        let item = args.next()?;
        if is_help(&item) {
            return Err(Exit::Print(CLEAN_HELP.to_owned()));
        }
        match item {
            Item::End => break,
            Item::Long(ref name, _) if name == "backend" => {
                engine_option(&mut args, &mut engine, &item)?;
            }
            Item::Long(name, value) if name == "dry-run" => {
                out.dry_run = flag(&mut args, "--dry-run", &value)?;
            }
            Item::Long(name, _) => {
                return Err(args.unknown_long(&name, &["backend", "dry-run", "help"]));
            }
            Item::Short(c, _) => return Err(args.unexpected(&format!("-{c}"))),
            Item::Positional(p) => args.positionals.push(p),
        }
    }
    args.positionals(0)?;
    reject_rest(&args)?;
    out.backend = engine.backend;
    Ok(out)
}

const DOCTOR_HELP: &str = "\
Check that this machine can run sessions, and what to change if not

Usage: podbay doctor [OPTIONS]

Options:
      --backend <BACKEND>  Container engine to check [default: config `backend`, else podman if installed, else docker] [possible values: podman, docker]
  -h, --help               Print help
";

const DOCTOR: Usage = Usage {
    line: "podbay doctor [OPTIONS]",
};

fn parse_doctor(items: Vec<OsString>) -> Result<DoctorArgs, Exit> {
    let mut args = Args::new(items, DOCTOR);
    let mut engine = EngineArgs::default();
    loop {
        let item = args.next()?;
        if is_help(&item) {
            return Err(Exit::Print(DOCTOR_HELP.to_owned()));
        }
        match item {
            Item::End => break,
            Item::Long(ref name, _) if name == "backend" => {
                engine_option(&mut args, &mut engine, &item)?;
            }
            Item::Long(name, _) => return Err(args.unknown_long(&name, &["backend", "help"])),
            Item::Short(c, _) => return Err(args.unexpected(&format!("-{c}"))),
            Item::Positional(p) => args.positionals.push(p),
        }
    }
    args.positionals(0)?;
    reject_rest(&args)?;
    Ok(DoctorArgs {
        backend: engine.backend,
    })
}
const PROFILE_COMMANDS: &[&str] = &["init", "list", "show", "help"];

fn usage_error(message: String, usage: Option<&str>) -> Exit {
    let mut text = format!("error: {message}\n");
    if let Some(line) = usage {
        text.push_str(&format!("\nUsage: {line}\n"));
    }
    text.push_str("\nFor more information, try '--help'.\n");
    Exit::Usage(text)
}

/// The closest of `known` to `word`, if any is close enough to be a typo.
fn suggestion<'a>(word: &str, known: &[&'a str]) -> Option<&'a str> {
    known
        .iter()
        .map(|k| (edit_distance(word, k), *k))
        .filter(|(d, k)| *d <= 2.min(k.len() / 2).max(1))
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut row = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            row.push((prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1));
        }
        prev = row;
    }
    prev[b.len()]
}

fn unknown_subcommand(word: &str, known: &[&str], usage: &str) -> Exit {
    let mut message = format!("unrecognized subcommand '{word}'");
    if let Some(close) = suggestion(word, known) {
        message.push_str(&format!(
            "\n\n  tip: a similar subcommand exists: '{close}'"
        ));
    }
    usage_error(message, Some(usage))
}

/// One subcommand's arguments, consumed left to right.
struct Args {
    items: std::vec::IntoIter<OsString>,
    usage: Usage,
    /// Options seen so far, to refuse repeats of single-valued ones.
    seen: Vec<&'static str>,
    positionals: Vec<OsString>,
    /// Everything after `--`.
    rest: Option<Vec<OsString>>,
}

/// One parsed option or argument.
enum Item {
    Long(String, Option<String>),
    Short(char, Option<String>),
    Positional(OsString),
    End,
}

impl Args {
    fn new(items: Vec<OsString>, usage: Usage) -> Self {
        Self {
            items: items.into_iter(),
            usage,
            seen: Vec::new(),
            positionals: Vec::new(),
            rest: None,
        }
    }

    fn error(&self, message: String) -> Exit {
        usage_error(message, Some(self.usage.line))
    }

    fn next(&mut self) -> Result<Item, Exit> {
        let Some(arg) = self.items.next() else {
            return Ok(Item::End);
        };
        if arg == "--" {
            self.rest = Some(self.items.by_ref().collect());
            return Ok(Item::End);
        }
        let Some(text) = arg.to_str() else {
            return Ok(Item::Positional(arg));
        };
        if let Some(long) = text.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n.to_owned(), Some(v.to_owned())),
                None => (long.to_owned(), None),
            };
            return Ok(Item::Long(name, value));
        }
        if let Some(short) = text.strip_prefix('-').filter(|s| !s.is_empty()) {
            let mut chars = short.chars();
            let c = chars.next().expect("not empty");
            let rest: String = chars.collect();
            let value =
                (!rest.is_empty()).then(|| rest.strip_prefix('=').unwrap_or(&rest).to_owned());
            return Ok(Item::Short(c, value));
        }
        Ok(Item::Positional(arg))
    }

    /// The value of `--name`, given inline or as the next argument.
    fn value(
        &mut self,
        name: &str,
        inline: Option<String>,
        placeholder: &str,
    ) -> Result<String, Exit> {
        if let Some(v) = inline {
            return Ok(v);
        }
        match self.items.next() {
            Some(v) if v != "--" => v
                .into_string()
                .map_err(|_| self.error(format!("invalid UTF-8 in the value for '{name}'"))),
            _ => Err(self.error(format!(
                "a value is required for '{name} <{placeholder}>' but none was supplied"
            ))),
        }
    }

    /// Refuses a second `--name`, for options that take one value or none.
    fn once(&mut self, name: &'static str) -> Result<(), Exit> {
        if self.seen.contains(&name) {
            return Err(self.error(format!(
                "the argument '{name}' cannot be used multiple times"
            )));
        }
        self.seen.push(name);
        Ok(())
    }

    fn unexpected(&self, shown: &str) -> Exit {
        let mut message = format!("unexpected argument '{shown}' found");
        if shown.starts_with('-') {
            message.push_str(&format!(
                "\n\n  tip: to pass '{shown}' as a value, use '-- {shown}'"
            ));
        }
        self.error(message)
    }

    /// An option this subcommand doesn't have, with a tip if it's close to
    /// one it does.
    fn unknown_long(&self, name: &str, known: &[&str]) -> Exit {
        match suggestion(name, known) {
            Some(close) => self.error(format!(
                "unexpected argument '--{name}' found\n\n  tip: a similar argument exists: '--{close}'"
            )),
            None => self.unexpected(&format!("--{name}")),
        }
    }

    /// At most `max` positional arguments.
    fn positionals(&self, max: usize) -> Result<Vec<OsString>, Exit> {
        if let Some(extra) = self.positionals.get(max) {
            return Err(self.unexpected(&extra.to_string_lossy()));
        }
        Ok(self.positionals.clone())
    }
}

/// Splits comma-separated lists, as `-p rust,dsh`.
fn split_list(value: &str) -> impl Iterator<Item = String> + '_ {
    value.split(',').map(str::to_owned)
}

/// Handles the options every engine-using subcommand has. `Ok(false)` if
/// `item` isn't one of them.
fn engine_option(args: &mut Args, engine: &mut EngineArgs, item: &Item) -> Result<bool, Exit> {
    match item {
        Item::Long(name, value) if name == "backend" => {
            args.once("--backend")?;
            let v = args.value("--backend", value.clone(), "BACKEND")?;
            engine.backend = Some(v.parse().map_err(|_| {
                usage_error(
                    format!(
                        "invalid value '{v}' for '--backend <BACKEND>'\n  [possible values: podman, docker]"
                    ),
                    None,
                )
            })?);
        }
        Item::Long(name, value) if name == "profile" => {
            let v = args.value("--profile", value.clone(), "NAME")?;
            engine.profiles.extend(split_list(&v));
        }
        Item::Short('p', value) => {
            let v = args.value("-p", value.clone(), "NAME")?;
            engine.profiles.extend(split_list(&v));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Help asked for with `-h` or `--help`.
fn is_help(item: &Item) -> bool {
    matches!(item, Item::Short('h', None)) || matches!(item, Item::Long(n, None) if n == "help")
}

fn flag(args: &mut Args, name: &'static str, value: &Option<String>) -> Result<bool, Exit> {
    if value.is_some() {
        return Err(args.error(format!("unexpected value for '{name}': it takes none")));
    }
    args.once(name)?;
    Ok(true)
}

fn parse_run(items: Vec<OsString>) -> Result<RunArgs, Exit> {
    const LONGS: &[&str] = &[
        "backend",
        "profile",
        "local-cache",
        "network",
        "online",
        "offline",
        "port",
        "dry-run",
        "help",
    ];
    let mut args = Args::new(items, RUN);
    let mut out = RunArgs::default();
    loop {
        let item = args.next()?;
        if is_help(&item) {
            return Err(Exit::Print(run_help()));
        }
        if engine_option(&mut args, &mut out.engine, &item)? {
            continue;
        }
        match item {
            Item::End => break,
            Item::Long(name, value) => match name.as_str() {
                "local-cache" => out.local_cache = flag(&mut args, "--local-cache", &value)?,
                "online" => out.online = flag(&mut args, "--online", &value)?,
                "offline" => out.offline = flag(&mut args, "--offline", &value)?,
                "dry-run" => out.dry_run = flag(&mut args, "--dry-run", &value)?,
                "network" => {
                    args.once("--network")?;
                    let v = args.value("--network", value, "NETWORK")?;
                    out.network = Some(v.parse().map_err(|_| {
                        usage_error(
                            format!(
                                "invalid value '{v}' for '--network <NETWORK>'\n  [possible values: none, full]"
                            ),
                            None,
                        )
                    })?);
                }
                "port" => {
                    let v = args.value("--port", value, "PORT")?;
                    let port = Port::parse(&v).map_err(|e| {
                        usage_error(
                            format!("invalid value '{v}' for '--port <PORT>': {e}"),
                            None,
                        )
                    })?;
                    out.ports.push(port);
                }
                _ => return Err(args.unknown_long(&name, LONGS)),
            },
            Item::Short(c, _) => return Err(args.unexpected(&format!("-{c}"))),
            Item::Positional(p) => args.positionals.push(p),
        }
    }
    // The same conflicts clap enforced: one way of choosing the network.
    let chosen: Vec<&str> = [
        (out.network.is_some(), "--network <NETWORK>"),
        (out.online, "--online"),
        (out.offline, "--offline"),
    ]
    .iter()
    .filter(|(set, _)| *set)
    .map(|(_, name)| *name)
    .collect();
    if let [first, second, ..] = chosen[..] {
        return Err(args.error(format!(
            "the argument '{first}' cannot be used with '{second}'"
        )));
    }
    out.project = args.positionals(1)?.pop().map(PathBuf::from);
    out.command = args
        .rest
        .take()
        .unwrap_or_default()
        .into_iter()
        .map(|a| {
            a.into_string()
                .map_err(|_| args.error("invalid UTF-8 in the command".into()))
        })
        .collect::<Result<_, _>>()?;
    Ok(out)
}

fn parse_build(items: Vec<OsString>) -> Result<BuildArgs, Exit> {
    const LONGS: &[&str] = &["backend", "profile", "force", "help"];
    let mut args = Args::new(items, BUILD);
    let mut out = BuildArgs::default();
    loop {
        let item = args.next()?;
        if is_help(&item) {
            return Err(Exit::Print(build_help()));
        }
        if engine_option(&mut args, &mut out.engine, &item)? {
            continue;
        }
        match item {
            Item::End => break,
            Item::Long(name, value) if name == "force" => {
                out.force = flag(&mut args, "--force", &value)?;
            }
            Item::Long(name, _) => return Err(args.unknown_long(&name, LONGS)),
            Item::Short(c, _) => return Err(args.unexpected(&format!("-{c}"))),
            Item::Positional(p) => args.positionals.push(p),
        }
    }
    args.positionals(0)?;
    reject_rest(&args)?;
    Ok(out)
}

fn parse_provision(items: Vec<OsString>) -> Result<ProvisionArgs, Exit> {
    const LONGS: &[&str] = &["backend", "profile", "local-cache", "dry-run", "help"];
    let mut args = Args::new(items, PROVISION);
    let mut out = ProvisionArgs::default();
    loop {
        let item = args.next()?;
        if is_help(&item) {
            return Err(Exit::Print(provision_help()));
        }
        if engine_option(&mut args, &mut out.engine, &item)? {
            continue;
        }
        match item {
            Item::End => break,
            Item::Long(name, value) => match name.as_str() {
                "local-cache" => out.local_cache = flag(&mut args, "--local-cache", &value)?,
                "dry-run" => out.dry_run = flag(&mut args, "--dry-run", &value)?,
                _ => return Err(args.unknown_long(&name, LONGS)),
            },
            Item::Short(c, _) => return Err(args.unexpected(&format!("-{c}"))),
            Item::Positional(p) => args.positionals.push(p),
        }
    }
    out.project = args.positionals(1)?.pop().map(PathBuf::from);
    reject_rest(&args)?;
    Ok(out)
}

/// Only `run` takes a command after `--`.
fn reject_rest(args: &Args) -> Result<(), Exit> {
    match args.rest.as_ref().and_then(|r| r.first()) {
        Some(extra) => Err(args.unexpected(&extra.to_string_lossy())),
        None => Ok(()),
    }
}

/// Positional names for `profile` subcommands, as UTF-8.
fn names(args: &Args, positionals: Vec<OsString>) -> Result<Vec<String>, Exit> {
    positionals
        .into_iter()
        .map(|p| {
            p.into_string()
                .map_err(|_| args.error("invalid UTF-8 in a profile name".into()))
        })
        .collect()
}

fn parse_profile(mut items: Vec<OsString>) -> Result<ProfileCommand, Exit> {
    if items.is_empty() {
        return Err(Exit::Usage(PROFILE_HELP.to_owned()));
    }
    let sub = items.remove(0);
    let sub = sub.to_string_lossy().into_owned();
    match sub.as_str() {
        "-h" | "--help" => Err(Exit::Print(PROFILE_HELP.to_owned())),
        "help" => Err(help_for(&["profile".into()], &items)),
        "init" => {
            let mut args = Args::new(items, PROFILE_INIT);
            let mut extends = Vec::new();
            loop {
                let item = args.next()?;
                if is_help(&item) {
                    return Err(Exit::Print(PROFILE_INIT_HELP.to_owned()));
                }
                match item {
                    Item::End => break,
                    Item::Long(name, value) if name == "extends" => {
                        let v = args.value("--extends", value, "EXTENDS")?;
                        extends.extend(split_list(&v));
                    }
                    Item::Long(name, _) => {
                        return Err(args.unknown_long(&name, &["extends", "help"]));
                    }
                    Item::Short(c, _) => return Err(args.unexpected(&format!("-{c}"))),
                    Item::Positional(p) => args.positionals.push(p),
                }
            }
            reject_rest(&args)?;
            let positionals = args.positionals(1)?;
            let mut given = names(&args, positionals)?;
            let Some(name) = given.pop() else {
                return Err(args.error(
                    "the following required arguments were not provided:\n  <NAME>".into(),
                ));
            };
            Ok(ProfileCommand::Init { name, extends })
        }
        "list" => {
            let mut args = Args::new(items, PROFILE_LIST);
            loop {
                let item = args.next()?;
                if is_help(&item) {
                    return Err(Exit::Print(PROFILE_LIST_HELP.to_owned()));
                }
                match item {
                    Item::End => break,
                    Item::Long(name, _) => return Err(args.unknown_long(&name, &["help"])),
                    Item::Short(c, _) => return Err(args.unexpected(&format!("-{c}"))),
                    Item::Positional(p) => args.positionals.push(p),
                }
            }
            args.positionals(0)?;
            reject_rest(&args)?;
            Ok(ProfileCommand::List)
        }
        "show" => {
            let mut args = Args::new(items, PROFILE_SHOW);
            loop {
                let item = args.next()?;
                if is_help(&item) {
                    return Err(Exit::Print(PROFILE_SHOW_HELP.to_owned()));
                }
                match item {
                    Item::End => break,
                    Item::Long(name, _) => return Err(args.unknown_long(&name, &["help"])),
                    Item::Short(c, _) => return Err(args.unexpected(&format!("-{c}"))),
                    Item::Positional(p) => args.positionals.push(p),
                }
            }
            reject_rest(&args)?;
            let positionals = args.positionals.clone();
            let given: Vec<String> = names(&args, positionals)?
                .iter()
                .flat_map(|n| split_list(n).collect::<Vec<_>>())
                .collect();
            if given.is_empty() {
                return Err(args.error(
                    "the following required arguments were not provided:\n  <NAMES>...".into(),
                ));
            }
            Ok(ProfileCommand::Show { names: given })
        }
        other => Err(unknown_subcommand(
            other,
            PROFILE_COMMANDS,
            "podbay profile <COMMAND>",
        )),
    }
}

/// `podbay help [COMMAND...]`: the help of what's named, after `prefix`.
fn help_for(prefix: &[String], named: &[OsString]) -> Exit {
    let path: Vec<String> = prefix
        .iter()
        .cloned()
        .chain(named.iter().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    match path[..] {
        [] => Exit::Print(MAIN_HELP.to_owned()),
        ["run"] => Exit::Print(run_help()),
        ["build"] => Exit::Print(build_help()),
        ["provision"] => Exit::Print(provision_help()),
        ["doctor"] => Exit::Print(DOCTOR_HELP.to_owned()),
        ["clean"] => Exit::Print(CLEAN_HELP.to_owned()),
        ["profile"] => Exit::Print(PROFILE_HELP.to_owned()),
        ["profile", "init"] => Exit::Print(PROFILE_INIT_HELP.to_owned()),
        ["profile", "list"] => Exit::Print(PROFILE_LIST_HELP.to_owned()),
        ["profile", "show"] => Exit::Print(PROFILE_SHOW_HELP.to_owned()),
        ["help"] | ["profile", "help"] => Exit::Print(MAIN_HELP.to_owned()),
        ["profile", other, ..] => {
            unknown_subcommand(other, PROFILE_COMMANDS, "podbay profile <COMMAND>")
        }
        [other, ..] => unknown_subcommand(other, COMMANDS, "podbay <COMMAND>"),
    }
}

impl Cli {
    /// Parses podbay's own arguments, or prints help or an error and exits.
    pub fn parse() -> Self {
        match Self::try_parse_from(std::env::args_os()) {
            Ok(cli) => cli,
            Err(Exit::Print(text)) => {
                print!("{text}");
                std::process::exit(0);
            }
            Err(Exit::Usage(text)) => {
                eprint!("{text}");
                std::process::exit(2);
            }
        }
    }

    /// Parses `args`, the first of which is the program's name.
    pub fn try_parse_from<I, T>(args: I) -> Result<Self, Exit>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let mut args: Vec<OsString> = args.into_iter().map(Into::into).skip(1).collect();
        if args.is_empty() {
            return Err(Exit::Usage(MAIN_HELP.to_owned()));
        }
        let first = args.remove(0).to_string_lossy().into_owned();
        let command = match first.as_str() {
            "-h" | "--help" => return Err(Exit::Print(MAIN_HELP.to_owned())),
            "-V" | "--version" => return Err(Exit::Print(VERSION.to_owned())),
            "help" => return Err(help_for(&[], &args)),
            "run" => Command::Run(parse_run(args)?),
            "build" => Command::Build(parse_build(args)?),
            "provision" => Command::Provision(parse_provision(args)?),
            "profile" => Command::Profile(parse_profile(args)?),
            "doctor" => Command::Doctor(parse_doctor(args)?),
            "clean" => Command::Clean(parse_clean(args)?),
            other if other.starts_with('-') => {
                return Err(usage_error(
                    format!("unexpected argument '{other}' found"),
                    Some("podbay <COMMAND>"),
                ));
            }
            other => return Err(unknown_subcommand(other, COMMANDS, "podbay <COMMAND>")),
        };
        Ok(Self { command })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, Exit> {
        Cli::try_parse_from(std::iter::once("podbay").chain(args.iter().copied()))
    }

    fn run(args: &[&str]) -> RunArgs {
        let mut all = vec!["run"];
        all.extend(args);
        match parse(&all).unwrap().command {
            Command::Run(run) => run,
            other => panic!("{other:?}"),
        }
    }

    fn usage(args: &[&str]) -> String {
        match parse(args) {
            Err(Exit::Usage(text)) => text,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    #[test]
    fn trailing_command() {
        let r = run(&["-p", "ai", ".", "--", "ls", "-la"]);
        assert_eq!(r.engine.profiles, ["ai"]);
        assert_eq!(r.project, Some(PathBuf::from(".")));
        assert_eq!(r.command, ["ls", "-la"]);
        assert_eq!(r.network, None);
        // Options may follow the project; `--` ends them, even for flags.
        let r = run(&[".", "--dry-run", "--", "--offline"]);
        assert!(r.dry_run && !r.offline);
        assert_eq!(r.command, ["--offline"]);
    }

    #[test]
    fn value_spellings() {
        for args in [
            &["-p", "a", "--backend", "docker"][..],
            &["-pa", "--backend=docker"],
            &["-p=a", "--backend", "docker"],
            &["--profile=a", "--backend=docker"],
        ] {
            let r = run(args);
            assert_eq!(r.engine.profiles, ["a"], "{args:?}");
            assert_eq!(r.engine.backend, Some(BackendKind::Docker), "{args:?}");
        }
    }

    #[test]
    fn network_flag() {
        assert_eq!(
            run(&["--network", "full", "."]).network,
            Some(NetworkMode::Full)
        );
        let err = usage(&["run", "--network", "lan"]);
        assert!(
            err.contains("invalid value 'lan' for '--network <NETWORK>'"),
            "{err}"
        );
        assert!(err.contains("[possible values: none, full]"), "{err}");
    }

    #[test]
    fn online_and_offline() {
        assert_eq!(run(&["."]).network_flag(), None);
        assert_eq!(
            run(&["--online", "."]).network_flag(),
            Some((NetworkMode::Full, "--online"))
        );
        assert_eq!(
            run(&["--offline", "."]).network_flag(),
            Some((NetworkMode::None, "--offline"))
        );
        assert_eq!(
            run(&["--network", "none", "."]).network_flag(),
            Some((NetworkMode::None, "--network"))
        );
        for conflicting in [
            &["run", "--online", "--offline"][..],
            &["run", "--online", "--network", "none"],
            &["run", "--offline", "--network", "full"],
            &["run", "--online", "--online"],
            &["run", "--backend", "docker", "--backend", "podman"],
        ] {
            let err = usage(conflicting);
            assert!(err.contains("cannot be used"), "{conflicting:?}: {err}");
        }
    }

    #[test]
    fn stacked_profiles() {
        let r = run(&["-p", "rust", "-p", "dsh,base", "."]);
        assert_eq!(r.engine.profiles, ["rust", "dsh", "base"]);
        let Command::Profile(ProfileCommand::Show { names }) =
            parse(&["profile", "show", "rust", "dsh"]).unwrap().command
        else {
            panic!()
        };
        assert_eq!(names, ["rust", "dsh"]);
        let Command::Profile(ProfileCommand::Show { names }) =
            parse(&["profile", "show", "rust,dsh"]).unwrap().command
        else {
            panic!()
        };
        assert_eq!(names, ["rust", "dsh"]);
    }

    #[test]
    fn ports() {
        let r = run(&["--port", "3080", "--port", "8080:3000", "."]);
        let ports: Vec<String> = r.ports.iter().map(Port::to_string).collect();
        assert_eq!(ports, ["3080", "8080:3000"]);
        let err = usage(&["run", "--port", "0", "."]);
        assert!(
            err.contains("invalid value '0' for '--port <PORT>'"),
            "{err}"
        );
    }

    #[test]
    fn profile_commands() {
        let Command::Profile(ProfileCommand::Init { name, extends }) = parse(&[
            "profile",
            "init",
            "ai",
            "--extends",
            "base,x",
            "--extends=y",
        ])
        .unwrap()
        .command
        else {
            panic!()
        };
        assert_eq!(name, "ai");
        assert_eq!(extends, ["base", "x", "y"]);
        assert!(matches!(
            parse(&["profile", "list"]).unwrap().command,
            Command::Profile(ProfileCommand::List)
        ));
        assert!(usage(&["profile", "show"]).contains("<NAMES>..."));
        assert!(usage(&["profile", "init"]).contains("<NAME>"));
        assert!(usage(&["profile", "list", "x"]).contains("unexpected argument 'x'"));
    }

    #[test]
    fn help_and_version() {
        for args in [&["--help"][..], &["-h"], &["help"]] {
            assert_eq!(parse(args).unwrap_err(), Exit::Print(MAIN_HELP.into()));
        }
        assert_eq!(parse(&["-V"]).unwrap_err(), Exit::Print(VERSION.into()));
        assert_eq!(
            parse(&["--version"]).unwrap_err(),
            Exit::Print(VERSION.into())
        );
        for (args, start) in [
            (&["run", "--help"][..], "Start a container"),
            (&["run", "-h"], "Start a container"),
            (&["help", "run"], "Start a container"),
            (&["build", "--help"], "Build (or rebuild)"),
            (&["provision", "--help"], "Run the profile's"),
            (&["profile", "--help"], "Manage profiles"),
            (&["help", "profile", "show"], "Show what profiles"),
            (&["profile", "help", "init"], "Create a profile"),
            (&["profile", "show", "--help"], "Show what profiles"),
        ] {
            match parse(args) {
                Err(Exit::Print(text)) => assert!(text.starts_with(start), "{args:?}: {text}"),
                other => panic!("{args:?}: {other:?}"),
            }
        }
        // No subcommand at all: the help, as an error.
        assert_eq!(parse(&[]).unwrap_err(), Exit::Usage(MAIN_HELP.into()));
        assert_eq!(
            parse(&["profile"]).unwrap_err(),
            Exit::Usage(PROFILE_HELP.into())
        );
    }

    #[test]
    fn doctor() {
        let Command::Doctor(d) = parse(&["doctor", "--backend", "podman"]).unwrap().command else {
            panic!()
        };
        assert_eq!(d.backend, Some(BackendKind::Podman));
        assert!(matches!(
            parse(&["doctor"]).unwrap().command,
            Command::Doctor(_)
        ));
        assert!(usage(&["doctor", "-p", "x"]).contains("unexpected argument '-p'"));
        assert!(matches!(parse(&["doctor", "--help"]), Err(Exit::Print(_))));
        assert!(usage(&["docter"]).contains("a similar subcommand exists: 'doctor'"));
    }

    #[test]
    fn clean() {
        let Command::Clean(c) = parse(&["clean", "--dry-run", "--backend=docker"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert!(c.dry_run);
        assert_eq!(c.backend, Some(BackendKind::Docker));
        assert!(usage(&["clean", "--all"]).contains("unexpected argument '--all'"));
    }

    #[test]
    fn typos_get_tips() {
        let err = usage(&["runn"]);
        assert!(
            err.starts_with("error: unrecognized subcommand 'runn'"),
            "{err}"
        );
        assert!(
            err.contains("tip: a similar subcommand exists: 'run'"),
            "{err}"
        );
        let err = usage(&["profile", "shwo"]);
        assert!(err.contains("a similar subcommand exists: 'show'"), "{err}");
        let err = usage(&["run", "--dryrun"]);
        assert!(
            err.contains("a similar argument exists: '--dry-run'"),
            "{err}"
        );
        let err = usage(&["run", "--bogus"]);
        assert!(err.contains("use '-- --bogus'"), "{err}");
        assert!(!usage(&["zzzzzz"]).contains("tip"));
        let err = usage(&["run", "a", "b"]);
        assert!(err.contains("unexpected argument 'b' found"), "{err}");
        assert!(usage(&["build", "--", "x"]).contains("unexpected argument 'x'"));
        assert!(usage(&["run", "--dry-run=yes"]).contains("takes none"));
        assert!(usage(&["run", "--port"]).contains("a value is required"));
    }
}
