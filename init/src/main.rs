//! podbay-init: the first process in every podbay container.
//!
//! It starts as (possibly namespaced) root with a handful of capabilities,
//! sets up the dev user's home on its tmpfs, fixes ownership of mount
//! points, then drops to dev with no capabilities beyond those the profile
//! asked for, and runs the session's command in its place.
//!
//! It needs nothing from the image: no shell, no `setpriv`, no coreutils.
//! It is built into a static binary from this one file with plain `rustc`,
//! so it uses the standard library only, and calls the few C library
//! functions it needs (capabilities, groups) directly.
//!
//! Inputs from podbay, all optional:
//!   PODBAY_UID / PODBAY_GID   ids dev runs as; 0 means stay root (the
//!                             engine is rootless and root is the host user)
//!   PODBAY_MOUNT_PATHS        colon-separated mount points; their parent
//!                             directories must belong to dev
//!   PODBAY_VOLUME_PATHS       colon-separated mount points dev must own
//!   PODBAY_CAPS               comma-separated capabilities to keep, such as
//!                             sys_ptrace (empty: none)
//!   PODBAY_WORKSPACE          directory to start in
//!   PODBAY_PATH               colon-separated directories to put in front
//!                             of PATH (the profile's `path`)
//!
//! Usage: podbay-init <command> [args...]

use std::env;
use std::ffi::{CString, OsString, c_char, c_int, c_uint};
use std::fs::{self, File, FileTimes};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, chown, lchown, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const HOME: &str = "/home/dev";
const DEFAULT_WORKSPACE: &str = "/home/dev/workspace/repomount";
/// A copy of the image's `/home/dev`, which is a tmpfs at run time.
const SKELETON: &str = "/usr/local/share/podbay/home";
const DEFAULT_ID: u32 = 1000;

/// Capability names, without `CAP_`, at their number.
const CAPABILITIES: &[&str] = &[
    "chown",
    "dac_override",
    "dac_read_search",
    "fowner",
    "fsetid",
    "kill",
    "setgid",
    "setuid",
    "setpcap",
    "linux_immutable",
    "net_bind_service",
    "net_broadcast",
    "net_admin",
    "net_raw",
    "ipc_lock",
    "ipc_owner",
    "sys_module",
    "sys_rawio",
    "sys_chroot",
    "sys_ptrace",
    "sys_pacct",
    "sys_admin",
    "sys_boot",
    "sys_nice",
    "sys_resource",
    "sys_time",
    "sys_tty_config",
    "mknod",
    "lease",
    "audit_write",
    "audit_control",
    "setfcap",
    "mac_override",
    "mac_admin",
    "syslog",
    "wake_alarm",
    "block_suspend",
    "audit_read",
    "perfmon",
    "bpf",
    "checkpoint_restore",
];

mod sys {
    use super::{c_char, c_int, c_uint};

    pub const PR_SET_KEEPCAPS: c_int = 8;
    pub const PR_CAPBSET_READ: c_int = 23;
    pub const PR_CAPBSET_DROP: c_int = 24;
    pub const PR_CAP_AMBIENT: c_int = 47;
    pub const PR_CAP_AMBIENT_RAISE: c_uint = 2;
    pub const PR_CAP_AMBIENT_CLEAR_ALL: c_uint = 4;
    pub const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

    #[repr(C)]
    pub struct CapHeader {
        pub version: u32,
        pub pid: c_int,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct CapData {
        pub effective: u32,
        pub permitted: u32,
        pub inheritable: u32,
    }

    unsafe extern "C" {
        pub fn prctl(option: c_int, ...) -> c_int;
        pub fn capset(header: *mut CapHeader, data: *const CapData) -> c_int;
        pub fn setresuid(r: c_uint, e: c_uint, s: c_uint) -> c_int;
        pub fn setresgid(r: c_uint, e: c_uint, s: c_uint) -> c_int;
        pub fn initgroups(user: *const c_char, group: c_uint) -> c_int;
        pub fn geteuid() -> c_uint;
    }
}

/// Fails with the OS error when a C call returns -1.
fn check(what: &str, ret: c_int) -> io::Result<()> {
    match ret {
        -1 => {
            let err = io::Error::last_os_error();
            Err(io::Error::new(err.kind(), format!("{what}: {err}")))
        }
        _ => Ok(()),
    }
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("podbay: {msg}");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let Some((program, rest)) = args.split_first() else {
        return fail(
            "the image has no default command; give one after --, such as \
             `podbay run . -- sh`",
        );
    };
    let workspace = env::var_os("PODBAY_WORKSPACE").unwrap_or_else(|| DEFAULT_WORKSPACE.into());
    let mut command = Command::new(program);
    command.args(rest);

    // SAFETY: no arguments, no memory involved.
    if unsafe { sys::geteuid() } != 0 {
        return run(command, &workspace);
    }

    match setup() {
        Ok(privileges) => {
            if let Err(err) = env::set_current_dir(&workspace) {
                return fail(format_args!("{}: {err}", Path::new(&workspace).display()));
            }
            set_session_path();
            if let Err(err) = privileges.apply() {
                return fail(err);
            }
            exec(command)
        }
        Err(err) => fail(err),
    }
}

/// Not root (a custom image that starts as its user): just run.
fn run(command: Command, workspace: &OsString) -> ExitCode {
    if let Err(err) = env::set_current_dir(workspace) {
        return fail(format_args!("{}: {err}", Path::new(workspace).display()));
    }
    set_session_path();
    exec(command)
}

/// The profile's PATH entries apply to the session only. They usually point
/// into volumes the session can write, so nothing runs from them while this
/// is still privileged: the command is looked up only once it has dropped.
fn set_session_path() {
    if let Some(extra) = env::var_os("PODBAY_PATH").filter(|p| !p.is_empty()) {
        let mut path = extra;
        if let Some(old) = env::var_os("PATH") {
            path.push(":");
            path.push(old);
        }
        // SAFETY: single-threaded; nothing else reads the environment.
        unsafe { env::set_var("PATH", path) };
    }
}

/// Replaces this process with the command, as a shell's `exec` does.
fn exec(mut command: Command) -> ExitCode {
    let err = command.exec();
    eprintln!(
        "podbay: {}: {err}",
        Path::new(command.get_program()).display()
    );
    match err.kind() {
        ErrorKind::NotFound => ExitCode::from(127),
        _ => ExitCode::from(126),
    }
}

/// How the process drops its privileges once set up.
struct Privileges {
    uid: u32,
    gid: u32,
    /// Capability numbers to keep.
    keep: Vec<u32>,
}

/// Prepares the home directory and mounts as root, and returns how to drop.
fn setup() -> io::Result<Privileges> {
    let uid = id_var("PODBAY_UID")?;
    let gid = id_var("PODBAY_GID")?;
    let keep = parse_caps(&env::var("PODBAY_CAPS").unwrap_or_default())?;
    // SAFETY: single-threaded; nothing else reads the environment.
    unsafe {
        env::set_var("HOME", HOME);
        env::set_var("USER", "dev");
        env::set_var("LOGNAME", "dev");
    }

    if uid != 0 {
        check_dev(&fs::read_to_string("/etc/passwd")?, uid, gid)?;
    }
    seed_home(Path::new(SKELETON), Path::new(HOME), uid, gid)?;
    for path in paths_var("PODBAY_MOUNT_PATHS") {
        own_parents(&path, uid, gid)?;
    }
    for path in paths_var("PODBAY_VOLUME_PATHS") {
        own_volume(&path, uid, gid)?;
    }
    Ok(Privileges { uid, gid, keep })
}

fn id_var(name: &str) -> io::Result<u32> {
    match env::var(name) {
        Ok(v) if !v.is_empty() => v
            .parse()
            .map_err(|_| io::Error::other(format!("{name}={v:?} is not a number"))),
        _ => Ok(DEFAULT_ID),
    }
}

fn paths_var(name: &str) -> Vec<PathBuf> {
    env::var_os(name)
        .map(|v| {
            v.as_bytes()
                .split(|b| *b == b':')
                .filter(|p| !p.is_empty())
                .map(|p| PathBuf::from(std::ffi::OsStr::from_bytes(p)))
                .collect()
        })
        .unwrap_or_default()
}

/// `sys_ptrace,net_raw` as capability numbers.
fn parse_caps(list: &str) -> io::Result<Vec<u32>> {
    let mut keep = Vec::new();
    for name in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let lower = name.to_ascii_lowercase();
        let bare = lower.strip_prefix("cap_").unwrap_or(&lower);
        let Some(n) = CAPABILITIES.iter().position(|c| *c == bare) else {
            return Err(io::Error::other(format!("unknown capability {name:?}")));
        };
        keep.push(n as u32);
    }
    Ok(keep)
}

/// The image's dev user must have the ids podbay runs it as.
fn check_dev(passwd: &str, uid: u32, gid: u32) -> io::Result<()> {
    let found = passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.first() == Some(&"dev") && fields.len() >= 4)
            .then(|| (fields[2].parse::<u32>().ok(), fields[3].parse::<u32>().ok()))
    });
    match found {
        Some((Some(u), Some(g))) if (u, g) == (uid, gid) => Ok(()),
        Some((Some(u), Some(g))) => Err(io::Error::other(format!(
            "the image's dev user is {u}:{g}, expected {uid}:{gid}; rebuild with podbay build"
        ))),
        _ => Err(io::Error::other(
            "the image has no dev user; rebuild with podbay build",
        )),
    }
}

/// Copies what the image put in the home directory into the fresh tmpfs,
/// owned by dev, as `useradd` copies `/etc/skel`. Files the engine already
/// placed there (bind-mounted dotfiles) are left alone.
fn seed_home(skeleton: &Path, home: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let context = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", home.display()));
    if skeleton.is_dir() {
        copy_missing(skeleton, home, (uid, gid))?;
        // The home directory itself takes the skeleton's mode and times
        // (usually 0700), as `cp -a skeleton/. home` would give it.
        let meta = fs::metadata(skeleton).map_err(context)?;
        chown(home, Some(uid), Some(gid)).map_err(context)?;
        fs::set_permissions(home, fs::Permissions::from_mode(meta.mode() & 0o7777))
            .map_err(context)?;
        let times = FileTimes::new()
            .set_accessed(meta.accessed()?)
            .set_modified(meta.modified()?);
        return File::open(home)
            .and_then(|f| f.set_times(times))
            .map_err(context);
    }
    chown(home, Some(uid), Some(gid)).map_err(context)
}

/// Copies the tree under `from` into `to`, keeping modes and modification
/// times, owned by `owner`, and never replacing anything already in `to`.
/// Symbolic links are copied as links, never followed.
fn copy_missing(from: &Path, to: &Path, owner: (u32, u32)) -> io::Result<()> {
    let context = |path: &Path| {
        let path = path.to_owned();
        move |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()))
    };
    for entry in fs::read_dir(from).map_err(context(from))? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let meta = fs::symlink_metadata(&src).map_err(context(&src))?;
        let existing = fs::symlink_metadata(&dst).ok();
        let ft = meta.file_type();
        if ft.is_dir() {
            match &existing {
                Some(m) if m.is_dir() => {}
                Some(_) => continue,
                None => fs::create_dir(&dst).map_err(context(&dst))?,
            }
            copy_missing(&src, &dst, owner)?;
            if existing.is_none() {
                finish(&dst, &meta, owner).map_err(context(&dst))?;
            }
        } else if existing.is_some() {
            continue;
        } else if ft.is_symlink() {
            symlink(fs::read_link(&src)?, &dst).map_err(context(&dst))?;
            lchown(&dst, Some(owner.0), Some(owner.1)).map_err(context(&dst))?;
        } else if ft.is_file() {
            fs::copy(&src, &dst).map_err(context(&dst))?;
            finish(&dst, &meta, owner).map_err(context(&dst))?;
        }
        // Sockets, fifos and devices have no place in a home skeleton.
    }
    Ok(())
}

/// Gives a copied file or directory `owner`, and the source's mode and
/// times. Setuid, setgid and sticky bits are dropped: nothing in a home
/// directory needs them.
fn finish(path: &Path, meta: &fs::Metadata, owner: (u32, u32)) -> io::Result<()> {
    lchown(path, Some(owner.0), Some(owner.1))?;
    fs::set_permissions(path, fs::Permissions::from_mode(meta.mode() & 0o777))?;
    let times = FileTimes::new()
        .set_accessed(meta.accessed()?)
        .set_modified(meta.modified()?);
    File::open(path)?.set_times(times)
}

/// Engines create missing parents of a mount point as root; give the ones
/// below the home directory to dev. Symbolic links are never followed.
fn own_parents(mount: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let mut dir = mount.parent();
    while let Some(d) = dir {
        if d == Path::new(HOME) || d == Path::new("/") || d.as_os_str().is_empty() {
            break;
        }
        let meta = fs::symlink_metadata(d)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", d.display())))?;
        if meta.uid() != uid {
            lchown(d, Some(uid), Some(gid))
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", d.display())))?;
        }
        dir = d.parent();
    }
    Ok(())
}

/// Volumes belong to dev. Recurses only when the volume is new or the host
/// user changed, and never follows symbolic links, which the session can
/// plant in a volume to point anywhere.
fn own_volume(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let context = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    fs::create_dir_all(path).map_err(context)?;
    if fs::symlink_metadata(path).map_err(context)?.uid() != uid {
        chown_tree(path, uid, gid)?;
    }
    Ok(())
}

fn chown_tree(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let context = |e: io::Error| io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    lchown(path, Some(uid), Some(gid)).map_err(context)?;
    if fs::symlink_metadata(path).map_err(context)?.is_dir() {
        for entry in fs::read_dir(path).map_err(context)? {
            chown_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

impl Privileges {
    /// Drops to dev (or keeps root when it is the host user), keeping only
    /// the listed capabilities. The bounding set stops anything else from
    /// coming back, and no-new-privileges (set by podbay) stops setuid
    /// binaries from adding any.
    fn apply(&self) -> io::Result<()> {
        let mask = cap_mask(&self.keep);
        drop_bounding_set(mask)?;
        if self.uid == 0 {
            // Root inside a rootless engine: keep the uid, lose the
            // capabilities. Root gets its bounding set back as permitted
            // capabilities on exec, plus the inheritable ones; both are now
            // just the kept ones.
            return set_caps(mask, mask, mask);
        }

        let user = CString::new("dev").expect("no NUL");
        // SAFETY: a valid C string; gid is a plain integer.
        check("initgroups", unsafe {
            sys::initgroups(user.as_ptr(), self.gid)
        })?;
        if mask != 0 {
            // Keep the permitted set across the uid change, so the kept
            // capabilities can be raised as ambient ones below.
            // SAFETY: prctl with integer arguments.
            check("prctl(PR_SET_KEEPCAPS)", unsafe {
                sys::prctl(
                    sys::PR_SET_KEEPCAPS,
                    1 as std::ffi::c_ulong,
                    0 as std::ffi::c_ulong,
                    0 as std::ffi::c_ulong,
                    0 as std::ffi::c_ulong,
                )
            })?;
        }
        // SAFETY: plain integers.
        check("setresgid", unsafe {
            sys::setresgid(self.gid, self.gid, self.gid)
        })?;
        // SAFETY: plain integers.
        check("setresuid", unsafe {
            sys::setresuid(self.uid, self.uid, self.uid)
        })?;
        set_caps(mask, mask, mask)?;
        // Ambient capabilities survive exec for a non-root user; they're
        // how the kept ones reach the command.
        // SAFETY: prctl with integer arguments.
        check("prctl(PR_CAP_AMBIENT_CLEAR_ALL)", unsafe {
            sys::prctl(
                sys::PR_CAP_AMBIENT,
                sys::PR_CAP_AMBIENT_CLEAR_ALL as std::ffi::c_ulong,
                0 as std::ffi::c_ulong,
                0 as std::ffi::c_ulong,
                0 as std::ffi::c_ulong,
            )
        })?;
        for cap in &self.keep {
            // SAFETY: prctl with integer arguments.
            check("prctl(PR_CAP_AMBIENT_RAISE)", unsafe {
                sys::prctl(
                    sys::PR_CAP_AMBIENT,
                    sys::PR_CAP_AMBIENT_RAISE as std::ffi::c_ulong,
                    *cap as std::ffi::c_ulong,
                    0 as std::ffi::c_ulong,
                    0 as std::ffi::c_ulong,
                )
            })?;
        }
        Ok(())
    }
}

fn cap_mask(keep: &[u32]) -> u64 {
    keep.iter().fold(0, |m, c| m | 1 << c)
}

/// Removes every capability not in `mask` from the bounding set, up to the
/// highest one the kernel knows.
fn drop_bounding_set(mask: u64) -> io::Result<()> {
    for cap in 0..64u32 {
        // SAFETY: prctl with integer arguments.
        let present = unsafe {
            sys::prctl(
                sys::PR_CAPBSET_READ,
                cap as std::ffi::c_ulong,
                0 as std::ffi::c_ulong,
                0 as std::ffi::c_ulong,
                0 as std::ffi::c_ulong,
            )
        };
        if present < 0 {
            break; // past the last capability this kernel has
        }
        if present == 1 && mask & (1 << cap) == 0 {
            // SAFETY: prctl with integer arguments.
            check("prctl(PR_CAPBSET_DROP)", unsafe {
                sys::prctl(
                    sys::PR_CAPBSET_DROP,
                    cap as std::ffi::c_ulong,
                    0 as std::ffi::c_ulong,
                    0 as std::ffi::c_ulong,
                    0 as std::ffi::c_ulong,
                )
            })?;
        }
    }
    Ok(())
}

/// Sets this thread's effective, permitted and inheritable capabilities.
fn set_caps(effective: u64, permitted: u64, inheritable: u64) -> io::Result<()> {
    let mut header = sys::CapHeader {
        version: sys::CAPABILITY_VERSION_3,
        pid: 0,
    };
    let split = |m: u64| [m as u32, (m >> 32) as u32];
    let (e, p, i) = (split(effective), split(permitted), split(inheritable));
    let data = [0, 1].map(|n| sys::CapData {
        effective: e[n],
        permitted: p[n],
        inheritable: i[n],
    });
    // SAFETY: a version 3 header with two data structs, as capset expects.
    check("capset", unsafe { sys::capset(&mut header, data.as_ptr()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities() {
        assert_eq!(parse_caps("").unwrap(), Vec::<u32>::new());
        assert_eq!(parse_caps("sys_ptrace").unwrap(), [19]);
        assert_eq!(parse_caps("CAP_NET_RAW, sys_admin").unwrap(), [13, 21]);
        assert_eq!(parse_caps("checkpoint_restore").unwrap(), [40]);
        assert!(parse_caps("sys_everything").is_err());
        assert_eq!(cap_mask(&[0, 19, 40]), 1 | 1 << 19 | 1 << 40);
    }

    #[test]
    fn dev_user() {
        let passwd = "root:x:0:0::/root:/bin/sh\ndev:x:1000:1000::/home/dev:/bin/bash\n";
        check_dev(passwd, 1000, 1000).unwrap();
        let err = check_dev(passwd, 1500, 1500).unwrap_err().to_string();
        assert!(
            err.contains("dev user is 1000:1000, expected 1500:1500"),
            "{err}"
        );
        let err = check_dev("root:x:0:0::/root:/bin/sh\n", 1000, 1000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no dev user"), "{err}");
        // A user whose name merely starts with dev doesn't count.
        assert!(check_dev("devops:x:1000:1000::/h:/bin/sh\n", 1000, 1000).is_err());
    }

    #[test]
    fn home_is_seeded_without_replacing_anything() {
        let dir = std::env::temp_dir().join(format!("podbay-init-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (skel, home) = (dir.join("skel"), dir.join("home"));
        fs::create_dir_all(skel.join(".config/tool")).unwrap();
        fs::write(skel.join(".profile"), "skeleton").unwrap();
        fs::write(skel.join(".bashrc"), "skeleton").unwrap();
        fs::write(skel.join(".config/tool/conf"), "x").unwrap();
        fs::set_permissions(
            skel.join(".config/tool/conf"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        symlink("/etc/hostname", skel.join("link")).unwrap();
        fs::create_dir_all(&home).unwrap();
        // Already there, as a bind-mounted dotfile would be.
        fs::write(home.join(".bashrc"), "mine").unwrap();

        fs::set_permissions(&skel, fs::Permissions::from_mode(0o700)).unwrap();
        let me = fs::metadata(&dir).unwrap();
        seed_home(&skel, &home, me.uid(), me.gid()).unwrap();
        assert_eq!(
            fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::read_to_string(home.join(".profile")).unwrap(),
            "skeleton"
        );
        assert_eq!(fs::read_to_string(home.join(".bashrc")).unwrap(), "mine");
        assert_eq!(
            fs::metadata(home.join(".config/tool/conf"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::read_link(home.join("link")).unwrap(),
            Path::new("/etc/hostname")
        );
        // Seeding again changes nothing.
        seed_home(&skel, &home, me.uid(), me.gid()).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Copies belong to the session's user, whoever owned the skeleton, and
    /// lose setuid bits. Needs root to give files away; skipped otherwise.
    #[test]
    fn seeded_home_belongs_to_the_user() {
        // SAFETY: no arguments.
        if unsafe { sys::geteuid() } != 0 {
            return;
        }
        let dir = std::env::temp_dir().join(format!("podbay-init-own-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (skel, home) = (dir.join("skel"), dir.join("home"));
        fs::create_dir_all(skel.join(".config")).unwrap();
        fs::write(skel.join(".config/x"), "").unwrap();
        fs::set_permissions(skel.join(".config/x"), fs::Permissions::from_mode(0o4755)).unwrap();
        symlink("x", skel.join(".config/link")).unwrap();
        fs::create_dir_all(&home).unwrap();
        seed_home(&skel, &home, 4242, 4343).unwrap();
        for path in [".config", ".config/x", ".config/link"] {
            let meta = fs::symlink_metadata(home.join(path)).unwrap();
            assert_eq!((meta.uid(), meta.gid()), (4242, 4343), "{path}");
        }
        let mode = fs::metadata(home.join(".config/x"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o755);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn volume_ownership_never_follows_links() {
        let dir = std::env::temp_dir().join(format!("podbay-init-vol-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let vol = dir.join("vol");
        fs::create_dir_all(vol.join("sub")).unwrap();
        let outside = dir.join("outside");
        fs::write(&outside, "").unwrap();
        symlink(&outside, vol.join("sub/escape")).unwrap();
        let me = fs::metadata(&dir).unwrap();
        // Same owner as now: nothing to do, and nothing outside is touched
        // either way.
        own_volume(&vol, me.uid(), me.gid()).unwrap();
        chown_tree(&vol, me.uid(), me.gid()).unwrap();
        assert_eq!(fs::metadata(&outside).unwrap().uid(), me.uid());
        let _ = fs::remove_dir_all(&dir);
    }
}
