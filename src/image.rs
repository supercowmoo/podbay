//! Image layering: the bundled base image or an adapter around any other
//! image, one derived image per profile extension script, and on rootful
//! Docker a final layer that gives `dev` the host user's ids. Every tag is a hash of podbay's inputs to the image (its
//! Containerfile, scripts and parent tag), so changing any of them produces a
//! new tag. What those inputs fetch is not hashed: the `FROM` images by tag,
//! the packages `apt-get` installs, and a custom base rebuilt under the same
//! tag. `podbay build --force` rebuilds against the latest of all of them.
//! The bundled base is always built that way, since it tracks Debian.

use std::fs;

use anyhow::{Context, Result, bail};

use crate::backend::{Backend, IMAGE_DEV_UID, Refresh};
use crate::hash::sha256_hex_fields;
use crate::profile::Extension;

const BASE_CONTAINERFILE: &str = include_str!("../base/Containerfile");
const INIT_SOURCE: &str = include_str!("../init/src/main.rs");
const RELAY_CONTAINERFILE: &str = include_str!("../relay/Containerfile");
const RELAY_SOURCE: &str = include_str!("../relay/src/main.rs");

/// Where the bottom layer comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseImage {
    /// The base image shipped inside this binary.
    Bundled,
    /// Any other image, such as `alpine:3.22`, which podbay adapts.
    Custom(String),
}

/// The build stage that compiles podbay-init, shared by the bundled base
/// and adapters. `base/Containerfile` has the same stage.
const INIT_STAGE: &str = "\
FROM rust:1.90.0-alpine3.22@sha256:b4b54b176a74db7e5c68fdfe6029be39a02ccbcfe72b6e5a3e18e2c61b57ae26 AS init
COPY init.rs /src/init.rs
RUN rustc --edition 2024 -C opt-level=s -C panic=abort \\
    -C target-feature=+crt-static -C strip=symbols -o /podbay-init /src/init.rs
";

/// Puts the profile's `path` back in front of PATH for login shells, which
/// reset it from /etc/profile.
const PATH_SNIPPET: &str = "\
if [ -n \"${PODBAY_PATH:-}\" ]; then
  case \":$PATH:\" in *\":$PODBAY_PATH:\"*) ;; *) PATH=\"$PODBAY_PATH:$PATH\" ;; esac
  export PATH
fi
";

/// Image names go into a Containerfile's `FROM` line and engine arguments,
/// so only the characters image references use are allowed.
pub fn validate_image_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 255
        && !name.starts_with(['-', '.', '/', ':', '@'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/:@".contains(c));
    if !valid {
        bail!("invalid image name {name:?}: use a reference such as \"alpine:3.22\"");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    pub tag: String,
    pub source: LayerSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerSource {
    Bundled,
    /// The network relay for allowlisted sessions and forwarded ports, built
    /// from `relay/`.
    Relay,
    /// Any image, with podbay-init, a `dev` user with these ids, and the
    /// login-shell PATH snippet added. Built without running anything in the
    /// image, so images without a shell work too.
    Adapter {
        image: String,
        uid: u32,
        gid: u32,
        /// Built by podbay just below it (the bundled base), so it's never
        /// pulled.
        local: bool,
    },
    Extension {
        parent: String,
        profile: String,
        script: Vec<u8>,
    },
}

/// Where the base image keeps a copy of `/home/dev`, which is a tmpfs at run
/// time; podbay-init seeds the home directory from it.
const HOME_SKELETON: &str = "/usr/local/share/podbay/home";

/// Computes the layer stack for a profile, bottom first. The last layer is
/// the image to run. `uid:gid` is what `dev` runs as (0 for staying root);
/// an adapter gives `dev` those ids directly.
pub fn layers(
    base: &BaseImage,
    extensions: &[Extension],
    uid: u32,
    gid: u32,
) -> Result<Vec<Layer>> {
    let mut stack = Vec::new();
    let (image, local) = match base {
        BaseImage::Bundled => {
            let tag = bundled_base_tag();
            stack.push(Layer {
                tag: tag.clone(),
                source: LayerSource::Bundled,
            });
            (tag, true)
        }
        BaseImage::Custom(image) => {
            validate_image_name(image)?;
            (image.clone(), false)
        }
    };
    // Every image, the bundled one too, gets podbay-init and a dev user
    // with the session's ids the same way. Staying root still needs a dev
    // user for HOME and the like.
    let (uid, gid) = match uid {
        0 => (IMAGE_DEV_UID, IMAGE_DEV_UID),
        _ => (uid, gid),
    };
    let hash = sha256_hex_fields(&[
        adapter_containerfile(&image, &[]).as_bytes(),
        INIT_SOURCE.as_bytes(),
        PATH_SNIPPET.as_bytes(),
        format!("{uid}:{gid}").as_bytes(),
    ]);
    stack.push(Layer {
        tag: format!("podbay-image:{}", &hash[..16]),
        source: LayerSource::Adapter {
            image,
            uid,
            gid,
            local,
        },
    });

    for ext in extensions {
        let script = fs::read(&ext.script)
            .with_context(|| format!("reading extension {}", ext.script.display()))?;
        if !script.starts_with(b"#!") {
            bail!(
                "extension {} must start with a shebang such as #!/bin/sh",
                ext.script.display()
            );
        }
        let parent = stack.last().expect("stack starts non-empty").tag.clone();
        let containerfile = extension_containerfile(&parent, &ext.profile);
        let hash = sha256_hex_fields(&[containerfile.as_bytes(), &script]);
        stack.push(Layer {
            tag: format!("podbay-{}:{}", ext.profile, &hash[..16]),
            source: LayerSource::Extension {
                parent,
                profile: ext.profile.clone(),
                script,
            },
        });
    }
    Ok(stack)
}

/// The bundled base's tag for this podbay.
pub fn bundled_base_tag() -> String {
    let hash = sha256_hex_fields(&[BASE_CONTAINERFILE.as_bytes()]);
    format!("podbay-base:{}", &hash[..16])
}

/// The relay image that sessions with an allowlist or forwarded ports share
/// a network namespace with.
/// It stands alone: nothing from the profile goes into it.
pub fn relay_layer() -> Layer {
    let hash = sha256_hex_fields(&[RELAY_CONTAINERFILE.as_bytes(), RELAY_SOURCE.as_bytes()]);
    Layer {
        tag: format!("podbay-relay:{}", &hash[..16]),
        source: LayerSource::Relay,
    }
}

/// What [`ensure`] did or would do with a layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Present,
    Build,
}

/// Builds any missing layers and returns the action taken per layer. With
/// `force`, every layer is rebuilt without the build cache, pulling the
/// registry images they start from again. With `dry_run`, nothing is built.
pub fn ensure(
    backend: &dyn Backend,
    stack: &[Layer],
    force: bool,
    dry_run: bool,
) -> Result<Vec<Action>> {
    let mut actions = Vec::with_capacity(stack.len());
    // Once a layer is rebuilt, everything above it must be too.
    let mut rebuild_rest = false;
    for layer in stack {
        let action = match layer.source {
            _ if force || rebuild_rest || !backend.image_exists(&layer.tag)? => Action::Build,
            _ => Action::Present,
        };
        if action == Action::Build {
            rebuild_rest = true;
            if !dry_run {
                build(backend, layer, force)?;
            }
        }
        actions.push(action);
    }
    Ok(actions)
}

fn build(backend: &dyn Backend, layer: &Layer, force: bool) -> Result<()> {
    let ctx = crate::tempdir::TempDir::new_in(&std::env::temp_dir(), "podbay-build-")?;
    match &layer.source {
        LayerSource::Bundled => {
            fs::write(ctx.path().join("Containerfile"), BASE_CONTAINERFILE)?;
            fs::write(ctx.path().join("init.rs"), INIT_SOURCE)?;
        }
        LayerSource::Relay => {
            fs::write(ctx.path().join("Containerfile"), RELAY_CONTAINERFILE)?;
            fs::write(ctx.path().join("main.rs"), RELAY_SOURCE)?;
        }
        LayerSource::Extension {
            parent,
            profile,
            script,
        } => {
            fs::write(
                ctx.path().join("Containerfile"),
                extension_containerfile(parent, profile),
            )?;
            fs::write(ctx.path().join("extension.sh"), script)?;
        }
        LayerSource::Adapter {
            image,
            uid,
            gid,
            local,
        } => {
            if *local {
                if !backend.image_exists(image)? {
                    bail!("{image} should have been built first");
                }
            } else if !backend.image_exists(image)? {
                backend.pull_image(image)?;
            } else if force && backend.pull_image(image).is_err() {
                eprintln!("podbay: note: couldn't pull {image}; using the local copy");
            }
            let passwd = backend.read_file(image, "/etc/passwd")?;
            let group = backend.read_file(image, "/etc/group")?;
            let command = backend.default_command(image)?;
            let text =
                |bytes: Option<Vec<u8>>| bytes.map(|b| String::from_utf8_lossy(&b).into_owned());
            fs::write(
                ctx.path().join("Containerfile"),
                adapter_containerfile(image, &command),
            )?;
            fs::write(ctx.path().join("init.rs"), INIT_SOURCE)?;
            fs::write(
                ctx.path().join("passwd"),
                with_dev_user(text(passwd).as_deref(), *uid, *gid),
            )?;
            fs::write(
                ctx.path().join("group"),
                with_dev_group(text(group).as_deref(), *gid),
            )?;
            fs::write(ctx.path().join("podbay-path.sh"), PATH_SNIPPET)?;
        }
    }
    eprintln!("podbay: building {}", layer.tag);
    backend.build_image(&layer.tag, ctx.path(), refresh(&layer.source, force))
}

/// How fresh a build of `source` must be.
///
/// The bundled base tracks Debian rather than pinning it, so every build of
/// it is a full refresh: a cached `apt-get upgrade` step or an old local
/// `debian:trixie-slim` would quietly bring back the packages a rebuild is
/// meant to replace. Other layers refresh only with `force`.
fn refresh(source: &LayerSource, force: bool) -> Refresh {
    let bundled = matches!(source, LayerSource::Bundled);
    Refresh {
        no_cache: force || bundled,
        // Only these start from a registry image; the others start from a
        // podbay layer. An adapted image was pulled above, and may exist
        // only locally.
        pull: bundled || (force && matches!(source, LayerSource::Relay)),
    }
}

fn extension_containerfile(parent: &str, profile: &str) -> String {
    let dest = format!("/usr/local/share/podbay/extensions/{profile}.sh");
    // Whatever the script puts in /home/dev must reach the skeleton, or the
    // tmpfs home would hide it.
    format!(
        "FROM {parent}\n\
         USER root\n\
         COPY extension.sh {dest}\n\
         RUN chmod 0755 {dest} && {dest} \\\n  \
         && if [ -d {HOME_SKELETON} ]; then cp -a /home/dev/. {HOME_SKELETON}/; fi\n"
    )
}

/// podbay's additions to any image. Nothing runs in the image while
/// building, so it needs no shell or tools: everything is copied in, and
/// the users file is prepared on the host from the image's own.
fn adapter_containerfile(image: &str, command: &[String]) -> String {
    // A new ENTRYPOINT clears the image's CMD, so its default command comes
    // back as the command podbay-init runs.
    let cmd = match command.is_empty() {
        true => String::new(),
        false => format!("CMD {}\n", crate::json::string_array(command)),
    };
    format!(
        "{INIT_STAGE}\n\
         FROM {image}\n\
         COPY --from=init /podbay-init /usr/local/bin/podbay-init\n\
         COPY passwd /etc/passwd\n\
         COPY group /etc/group\n\
         COPY podbay-path.sh /etc/profile.d/podbay-path.sh\n\
         USER 0:0\n\
         ENTRYPOINT [\"/usr/local/bin/podbay-init\"]\n\
         {cmd}"
    )
}

/// The image's users with `dev` added at `uid:gid`, home `/home/dev`. An
/// existing `dev` gets those ids. When another user already has the uid
/// (images often have one at 1000), `dev` goes just before it, so the uid
/// shows as `dev`; both names keep working.
fn with_dev_user(passwd: Option<&str>, uid: u32, gid: u32) -> String {
    let passwd = passwd.unwrap_or("root:x:0:0:root:/root:/bin/sh\n");
    let dev = format!("dev:x:{uid}:{gid}:podbay:/home/dev:/bin/sh");
    let mut lines: Vec<String> = passwd
        .lines()
        .filter(|l| !l.starts_with("dev:"))
        .map(str::to_owned)
        .collect();
    let same_uid = lines
        .iter()
        .position(|l| l.split(':').nth(2) == Some(uid.to_string().as_str()));
    match same_uid {
        Some(i) => lines.insert(i, dev),
        None => lines.push(dev),
    }
    lines.join("\n") + "\n"
}

/// The image's groups with a `dev` group at `gid`, placed like the user.
fn with_dev_group(group: Option<&str>, gid: u32) -> String {
    let group = group.unwrap_or("root:x:0:\n");
    let dev = format!("dev:x:{gid}:");
    let mut lines: Vec<String> = group
        .lines()
        .filter(|l| !l.starts_with("dev:"))
        .map(str::to_owned)
        .collect();
    let same_gid = lines
        .iter()
        .position(|l| l.split(':').nth(2) == Some(gid.to_string().as_str()));
    match same_gid {
        Some(i) => lines.insert(i, dev),
        None => lines.push(dev),
    }
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::path::Path;

    use super::*;
    use crate::backend::{BackendKind, Identity};

    #[derive(Default)]
    struct FakeBackend {
        images: RefCell<HashSet<String>>,
        built: RefCell<Vec<(String, String, Refresh)>>,
        pulled: RefCell<Vec<String>>,
    }

    impl Backend for FakeBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Docker
        }
        fn image_exists(&self, image: &str) -> Result<bool> {
            Ok(self.images.borrow().contains(image))
        }
        fn build_image(&self, tag: &str, context: &Path, refresh: Refresh) -> Result<()> {
            let containerfile = fs::read_to_string(context.join("Containerfile"))?;
            self.built
                .borrow_mut()
                .push((tag.into(), containerfile, refresh));
            self.images.borrow_mut().insert(tag.into());
            Ok(())
        }
        fn pull_image(&self, image: &str) -> Result<()> {
            self.pulled.borrow_mut().push(image.into());
            self.images.borrow_mut().insert(image.into());
            Ok(())
        }
        fn default_command(&self, _image: &str) -> Result<Vec<String>> {
            Ok(vec!["/bin/sh".into()])
        }
        fn read_file(&self, _image: &str, path: &str) -> Result<Option<Vec<u8>>> {
            Ok((path == "/etc/passwd").then(|| b"root:x:0:0::/root:/bin/sh\n".to_vec()))
        }
        fn identity(&self) -> Result<Identity> {
            unreachable!()
        }
        fn cgroup_controllers(&self) -> Result<Option<Vec<String>>> {
            unreachable!()
        }
        fn list_images(&self) -> Result<Vec<String>> {
            unreachable!()
        }
        fn remove_image(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn list_containers(&self, _: &str) -> Result<Vec<(String, bool)>> {
            unreachable!()
        }
        fn remove_container(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn list_volumes(&self) -> Result<Vec<String>> {
            unreachable!()
        }
    }

    fn ext(dir: &Path, profile: &str, body: &str) -> Extension {
        let script = dir.join(format!("{profile}.sh"));
        fs::write(&script, body).unwrap();
        Extension {
            profile: profile.into(),
            script,
        }
    }

    #[test]
    fn tags_chain_through_parents() {
        let dir = crate::tempdir::tempdir().unwrap();
        let a = ext(dir.path(), "a", "#!/bin/sh\necho a\n");
        let b = ext(dir.path(), "b", "#!/bin/sh\necho b\n");

        let stack = layers(
            &BaseImage::Bundled,
            &[a.clone(), b.clone()],
            IMAGE_DEV_UID,
            IMAGE_DEV_UID,
        )
        .unwrap();
        assert_eq!(stack.len(), 4);
        assert!(stack[0].tag.starts_with("podbay-base:"));
        assert!(stack[1].tag.starts_with("podbay-image:"));
        assert!(stack[2].tag.starts_with("podbay-a:"));
        assert!(stack[3].tag.starts_with("podbay-b:"));

        // Changing a lower layer changes every tag above it.
        fs::write(&a.script, "#!/bin/sh\necho changed\n").unwrap();
        let changed = layers(&BaseImage::Bundled, &[a, b], IMAGE_DEV_UID, IMAGE_DEV_UID).unwrap();
        assert_eq!(stack[1].tag, changed[1].tag);
        assert_ne!(stack[2].tag, changed[2].tag);
        assert_ne!(stack[3].tag, changed[3].tag);
    }

    #[test]
    fn the_bundled_base_is_adapted_like_any_image() {
        let stack = layers(&BaseImage::Bundled, &[], 1234, 5678).unwrap();
        assert_eq!(stack.len(), 2);
        assert_eq!(
            stack[1].source,
            LayerSource::Adapter {
                image: stack[0].tag.clone(),
                uid: 1234,
                gid: 5678,
                local: true,
            }
        );
        // Different ids, different adapter; the base stays the same.
        let other = layers(&BaseImage::Bundled, &[], IMAGE_DEV_UID, IMAGE_DEV_UID).unwrap();
        assert_eq!(other[0].tag, stack[0].tag);
        assert_ne!(other[1].tag, stack[1].tag);
        // The base no longer carries podbay-init itself: the adapter adds it.
        assert!(!BASE_CONTAINERFILE.contains("COPY --from=init"));
        assert!(!BASE_CONTAINERFILE.contains("ENTRYPOINT"));
    }

    #[test]
    fn a_local_base_is_never_pulled() {
        let stack = layers(&BaseImage::Bundled, &[], IMAGE_DEV_UID, IMAGE_DEV_UID).unwrap();
        let backend = FakeBackend::default();
        ensure(&backend, &stack, true, false).unwrap();
        assert!(backend.pulled.borrow().is_empty());
        let built = backend.built.borrow();
        assert!(
            built[1].1.contains(&format!("FROM {}\n", stack[0].tag)),
            "{}",
            built[1].1
        );
        // Without the base there's nothing to adapt.
        let backend = FakeBackend::default();
        assert!(ensure(&backend, &stack[1..], false, false).is_err());
    }

    #[test]
    fn extension_requires_shebang() {
        let dir = crate::tempdir::tempdir().unwrap();
        let e = ext(dir.path(), "a", "apt-get install x\n");
        assert!(layers(&BaseImage::Bundled, &[e], IMAGE_DEV_UID, IMAGE_DEV_UID).is_err());
    }

    #[test]
    fn builds_only_missing_layers_and_everything_above() {
        let dir = crate::tempdir::tempdir().unwrap();
        let a = ext(dir.path(), "a", "#!/bin/sh\n");
        let b = ext(dir.path(), "b", "#!/bin/sh\n");
        let stack = layers(&BaseImage::Bundled, &[a, b], IMAGE_DEV_UID, IMAGE_DEV_UID).unwrap();

        let backend = FakeBackend::default();
        backend.images.borrow_mut().insert(stack[0].tag.clone());
        backend.images.borrow_mut().insert(stack[1].tag.clone());
        backend.images.borrow_mut().insert(stack[3].tag.clone());

        let actions = ensure(&backend, &stack, false, false).unwrap();
        assert_eq!(
            actions,
            [
                Action::Present,
                Action::Present,
                Action::Build,
                Action::Build
            ]
        );
        let built = backend.built.borrow();
        assert_eq!(built.len(), 2);
        assert!(built[0].1.starts_with(&format!("FROM {}\n", stack[1].tag)));
        assert!(built[1].1.starts_with(&format!("FROM {}\n", stack[2].tag)));
        assert!(built.iter().all(|b| b.2 == Refresh::default()));
    }

    #[test]
    fn force_rebuilds_uncached_and_pulls_only_registry_bases() {
        let dir = crate::tempdir::tempdir().unwrap();
        let a = ext(dir.path(), "a", "#!/bin/sh\n");
        let stack = layers(&BaseImage::Bundled, &[a], IMAGE_DEV_UID, IMAGE_DEV_UID).unwrap();
        let backend = FakeBackend::default();
        for layer in &stack {
            backend.images.borrow_mut().insert(layer.tag.clone());
        }

        let actions = ensure(&backend, &stack, true, false).unwrap();
        assert_eq!(actions, [Action::Build, Action::Build, Action::Build]);
        let built = backend.built.borrow();
        let full = Refresh {
            no_cache: true,
            pull: true,
        };
        assert_eq!(built[0].2, full);
        for layer in &built[1..] {
            assert_eq!(
                layer.2,
                Refresh {
                    pull: false,
                    ..full
                }
            );
        }
    }

    #[test]
    fn bundled_base_always_builds_fresh() {
        // Tracking Debian only works if no build of the base reuses a
        // cached upgrade step or an old local debian image.
        let stack = layers(&BaseImage::Bundled, &[], IMAGE_DEV_UID, IMAGE_DEV_UID).unwrap();
        let backend = FakeBackend::default();
        ensure(&backend, &stack, false, false).unwrap();
        let built = backend.built.borrow();
        assert!(built[0].0.starts_with("podbay-base:"), "{}", built[0].0);
        let full = Refresh {
            no_cache: true,
            pull: true,
        };
        assert_eq!(built[0].2, full);
        // Layers above start from it, a local image, so there's nothing to
        // pull; the engine's cache already misses when their parent changed.
        assert!(built[1..].iter().all(|b| b.2 == Refresh::default()));
    }

    #[test]
    fn relay_image_builds_from_its_source() {
        let layer = relay_layer();
        assert!(layer.tag.starts_with("podbay-relay:"), "{}", layer.tag);
        let backend = FakeBackend::default();
        ensure(&backend, &[layer], false, false).unwrap();
        let built = backend.built.borrow();
        assert!(built[0].1.contains("FROM scratch"), "{}", built[0].1);
    }

    #[test]
    fn dry_run_builds_nothing() {
        let stack = layers(&BaseImage::Bundled, &[], IMAGE_DEV_UID, IMAGE_DEV_UID).unwrap();
        let backend = FakeBackend::default();
        let actions = ensure(&backend, &stack, false, true).unwrap();
        assert_eq!(actions, [Action::Build, Action::Build]);
        assert!(backend.built.borrow().is_empty());
    }

    #[test]
    fn image_names() {
        for ok in [
            "alpine",
            "alpine:3.22",
            "docker.io/library/debian:trixie-slim",
            "ghcr.io/o/r@sha256:abc123",
            "localhost:5000/x",
        ] {
            validate_image_name(ok).unwrap();
        }
        for bad in ["", "-x", "a b", "a\nRUN rm -rf /", "a;b", "$(x)", "/abs"] {
            assert!(validate_image_name(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn any_image_gets_an_adapter_with_dev_at_the_session_ids() {
        let stack = layers(&BaseImage::Custom("alpine:3.22".into()), &[], 1500, 1600).unwrap();
        assert!(
            stack[0].tag.starts_with("podbay-image:"),
            "{}",
            stack[0].tag
        );
        assert_eq!(
            stack[0].source,
            LayerSource::Adapter {
                image: "alpine:3.22".into(),
                uid: 1500,
                gid: 1600,
                local: false,
            }
        );
        // Staying root still gets a dev user, at the image default.
        let root = layers(&BaseImage::Custom("alpine:3.22".into()), &[], 0, 0).unwrap();
        assert!(matches!(
            root[0].source,
            LayerSource::Adapter {
                uid: 1000,
                gid: 1000,
                ..
            }
        ));
        // Different images and ids get different tags.
        let other = layers(&BaseImage::Custom("debian".into()), &[], 1500, 1600).unwrap();
        assert_ne!(other[0].tag, stack[0].tag);
        assert_ne!(root[0].tag, stack[0].tag);
        assert!(layers(&BaseImage::Custom("a b".into()), &[], 0, 0).is_err());
    }

    #[test]
    fn adapter_containerfile_runs_nothing_in_the_image() {
        let file = adapter_containerfile(
            "sbx/claude-image:latest",
            &["tini".into(), "--".into(), "claude".into()],
        );
        assert!(file.contains("FROM sbx/claude-image:latest\n"), "{file}");
        let after_from = &file[file.find("FROM sbx/claude-image").unwrap()..];
        assert!(!after_from.contains("RUN"), "{file}");
        assert!(
            file.contains("ENTRYPOINT [\"/usr/local/bin/podbay-init\"]\n"),
            "{file}"
        );
        assert!(
            file.ends_with("CMD [\"tini\",\"--\",\"claude\"]\n"),
            "{file}"
        );
        assert!(!adapter_containerfile("x", &[]).contains("CMD"));
    }

    #[test]
    fn dev_user_is_added_to_the_image_users() {
        let image = "root:x:0:0:root:/root:/bin/bash\nagent:x:1000:1000::/home/agent:/bin/bash\n";
        assert_eq!(
            with_dev_user(Some(image), 1000, 1000),
            "root:x:0:0:root:/root:/bin/bash\n\
             dev:x:1000:1000:podbay:/home/dev:/bin/sh\n\
             agent:x:1000:1000::/home/agent:/bin/bash\n"
        );
        assert_eq!(
            with_dev_user(Some(image), 1500, 1500),
            format!("{image}dev:x:1500:1500:podbay:/home/dev:/bin/sh\n")
        );
        // An existing dev takes the session's ids.
        let has_dev = "root:x:0:0::/root:/bin/sh\ndev:x:1000:1000::/home/dev:/bin/bash\n";
        assert_eq!(
            with_dev_user(Some(has_dev), 1500, 1500),
            "root:x:0:0::/root:/bin/sh\ndev:x:1500:1500:podbay:/home/dev:/bin/sh\n"
        );
        // Images without users at all, like scratch.
        assert_eq!(
            with_dev_user(None, 1000, 1000),
            "root:x:0:0:root:/root:/bin/sh\ndev:x:1000:1000:podbay:/home/dev:/bin/sh\n"
        );
        assert_eq!(
            with_dev_group(Some("root:x:0:\nagent:x:1000:\n"), 1000),
            "root:x:0:\ndev:x:1000:\nagent:x:1000:\n"
        );
        assert_eq!(with_dev_group(None, 1000), "root:x:0:\ndev:x:1000:\n");
    }

    #[test]
    fn adapter_build_reads_the_image_and_pulls_it_when_missing() {
        let stack = layers(&BaseImage::Custom("alpine:3.22".into()), &[], 1000, 1000).unwrap();
        let backend = FakeBackend::default();
        assert_eq!(
            ensure(&backend, &stack, false, false).unwrap(),
            [Action::Build]
        );
        assert_eq!(*backend.pulled.borrow(), ["alpine:3.22"]);
        let built = backend.built.borrow();
        assert!(built[0].1.contains("FROM alpine:3.22\n"), "{}", built[0].1);
        assert!(built[0].1.contains("CMD [\"/bin/sh\"]"), "{}", built[0].1);
        // Never --pull: the image may exist only locally.
        assert!(!built[0].2.pull);
        drop(built);

        // Present now, and nothing is pulled again.
        assert_eq!(
            ensure(&backend, &stack, false, false).unwrap(),
            [Action::Present]
        );
        assert_eq!(backend.pulled.borrow().len(), 1);
    }
}
