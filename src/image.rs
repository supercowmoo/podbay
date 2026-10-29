//! Image layering: the bundled base image, one derived image per profile
//! extension script, and on rootful Docker a final layer that gives `dev` the
//! host user's ids. Every tag is a hash of podbay's inputs to the image (its
//! Containerfile, scripts and parent tag), so changing any of them produces a
//! new tag. What those inputs fetch is not hashed: the `FROM` images by tag,
//! the packages `apt-get` installs, and a custom base rebuilt under the same
//! tag. `podbay build --force` rebuilds against the latest of all of them.

use std::fs;

use anyhow::{Context, Result, bail};

use crate::backend::{Backend, IMAGE_DEV_UID, Refresh};
use crate::hash::sha256_hex_fields;
use crate::profile::Extension;

const BASE_CONTAINERFILE: &str = include_str!("../base/Containerfile");
const BASE_ENTRYPOINT: &str = include_str!("../base/entrypoint.sh");
const RELAY_CONTAINERFILE: &str = include_str!("../relay/Containerfile");
const RELAY_SOURCE: &str = include_str!("../relay/src/main.rs");

/// Where the bottom layer comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseImage {
    /// The base image shipped inside this binary.
    Bundled,
    /// A user-supplied image, used as-is.
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    pub tag: String,
    pub source: LayerSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerSource {
    Bundled,
    /// The network relay for allowlisted sessions, built from `relay/`.
    Relay,
    Custom,
    Extension {
        parent: String,
        profile: String,
        script: Vec<u8>,
    },
    /// Renumbers `dev`, which the read-only root filesystem rules out at run
    /// time.
    Identity {
        parent: String,
        uid: u32,
        gid: u32,
    },
}

/// Where the base image keeps a copy of `/home/dev`, which is a tmpfs at run
/// time; the entrypoint seeds the home directory from it.
const HOME_SKELETON: &str = "/usr/local/share/podbay/home";

/// Computes the layer stack for a profile, bottom first. The last layer is
/// the image to run.
pub fn layers(base: &BaseImage, extensions: &[Extension]) -> Result<Vec<Layer>> {
    let mut stack = vec![match base {
        BaseImage::Bundled => {
            let hash =
                sha256_hex_fields(&[BASE_CONTAINERFILE.as_bytes(), BASE_ENTRYPOINT.as_bytes()]);
            Layer {
                tag: format!("podbay-base:{}", &hash[..16]),
                source: LayerSource::Bundled,
            }
        }
        BaseImage::Custom(image) => Layer {
            tag: image.clone(),
            source: LayerSource::Custom,
        },
    }];

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

/// Adds the layer that gives `dev` the ids `uid:gid`, when the session runs
/// as someone other than the image's dev user (rootful Docker). 0 means the
/// session stays root and needs no layer.
pub fn add_identity(stack: &mut Vec<Layer>, uid: u32, gid: u32) {
    if uid == 0 || (uid, gid) == (IMAGE_DEV_UID, IMAGE_DEV_UID) {
        return;
    }
    let parent = stack.last().expect("stack starts non-empty").tag.clone();
    let containerfile = identity_containerfile(&parent, uid, gid);
    let hash = sha256_hex_fields(&[containerfile.as_bytes()]);
    stack.push(Layer {
        tag: format!("podbay-uid{uid}:{}", &hash[..16]),
        source: LayerSource::Identity { parent, uid, gid },
    });
}

/// The relay image that allowlisted sessions share a network namespace with.
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
            LayerSource::Custom => {
                if !backend.image_exists(&layer.tag)? {
                    bail!("base image {} not found", layer.tag);
                }
                Action::Present
            }
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
    let ctx = tempfile::Builder::new().prefix("podbay-build-").tempdir()?;
    match &layer.source {
        LayerSource::Bundled => {
            fs::write(ctx.path().join("Containerfile"), BASE_CONTAINERFILE)?;
            fs::write(ctx.path().join("entrypoint.sh"), BASE_ENTRYPOINT)?;
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
        LayerSource::Identity { parent, uid, gid } => {
            fs::write(
                ctx.path().join("Containerfile"),
                identity_containerfile(parent, *uid, *gid),
            )?;
        }
        LayerSource::Custom => unreachable!("custom images are never built"),
    }
    let refresh = Refresh {
        no_cache: force,
        // Only these start from a registry image; the others start from a
        // podbay layer or a custom base, which exist only locally.
        pull: force && matches!(layer.source, LayerSource::Bundled | LayerSource::Relay),
    };
    eprintln!("podbay: building {}", layer.tag);
    backend.build_image(&layer.tag, ctx.path(), refresh)
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

fn identity_containerfile(parent: &str, uid: u32, gid: u32) -> String {
    format!(
        "FROM {parent}\n\
         USER root\n\
         RUN old_uid=$(id -u dev) && old_gid=$(id -g dev) \\\n  \
         && sed -i \"s/^dev:x:$old_uid:$old_gid:/dev:x:{uid}:{gid}:/\" /etc/passwd \\\n  \
         && sed -i \"s/^dev:x:$old_gid:/dev:x:{gid}:/\" /etc/group \\\n  \
         && for dir in /home/dev {HOME_SKELETON}; do \\\n    \
         if [ -d \"$dir\" ]; then \\\n      \
         find \"$dir\" \\( -uid \"$old_uid\" -o -gid \"$old_gid\" \\) -exec chown -h {uid}:{gid} {{}} +; \\\n    \
         fi; \\\n  \
         done\n"
    )
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
        fn identity(&self) -> Result<Identity> {
            unreachable!()
        }
        fn cgroup_controllers(&self) -> Result<Option<Vec<String>>> {
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
        let dir = tempfile::tempdir().unwrap();
        let a = ext(dir.path(), "a", "#!/bin/sh\necho a\n");
        let b = ext(dir.path(), "b", "#!/bin/sh\necho b\n");

        let stack = layers(&BaseImage::Bundled, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(stack.len(), 3);
        assert!(stack[0].tag.starts_with("podbay-base:"));
        assert!(stack[1].tag.starts_with("podbay-a:"));
        assert!(stack[2].tag.starts_with("podbay-b:"));

        // Changing a lower layer changes every tag above it.
        fs::write(&a.script, "#!/bin/sh\necho changed\n").unwrap();
        let changed = layers(&BaseImage::Bundled, &[a, b]).unwrap();
        assert_eq!(stack[0].tag, changed[0].tag);
        assert_ne!(stack[1].tag, changed[1].tag);
        assert_ne!(stack[2].tag, changed[2].tag);
    }

    #[test]
    fn identity_layer_only_when_ids_differ() {
        let mut stack = layers(&BaseImage::Bundled, &[]).unwrap();
        add_identity(&mut stack, 0, 0);
        add_identity(&mut stack, IMAGE_DEV_UID, IMAGE_DEV_UID);
        assert_eq!(stack.len(), 1);

        add_identity(&mut stack, 1234, 5678);
        assert_eq!(stack.len(), 2);
        assert!(
            stack[1].tag.starts_with("podbay-uid1234:"),
            "{}",
            stack[1].tag
        );
        let LayerSource::Identity { parent, .. } = &stack[1].source else {
            panic!("{:?}", stack[1].source)
        };
        let file = identity_containerfile(parent, 1234, 5678);
        assert!(
            file.starts_with(&format!("FROM {}\n", stack[0].tag)),
            "{file}"
        );
        assert!(file.contains("dev:x:1234:5678:"), "{file}");
        assert!(file.contains("-exec chown -h 1234:5678 {} +"), "{file}");
    }

    #[test]
    fn extension_requires_shebang() {
        let dir = tempfile::tempdir().unwrap();
        let e = ext(dir.path(), "a", "apt-get install x\n");
        assert!(layers(&BaseImage::Bundled, &[e]).is_err());
    }

    #[test]
    fn builds_only_missing_layers_and_everything_above() {
        let dir = tempfile::tempdir().unwrap();
        let a = ext(dir.path(), "a", "#!/bin/sh\n");
        let b = ext(dir.path(), "b", "#!/bin/sh\n");
        let stack = layers(&BaseImage::Bundled, &[a, b]).unwrap();

        let backend = FakeBackend::default();
        backend.images.borrow_mut().insert(stack[0].tag.clone());
        backend.images.borrow_mut().insert(stack[2].tag.clone());

        let actions = ensure(&backend, &stack, false, false).unwrap();
        assert_eq!(actions, [Action::Present, Action::Build, Action::Build]);
        let built = backend.built.borrow();
        assert_eq!(built.len(), 2);
        assert!(built[0].1.starts_with(&format!("FROM {}\n", stack[0].tag)));
        assert!(built[1].1.starts_with(&format!("FROM {}\n", stack[1].tag)));
        assert!(built.iter().all(|b| b.2 == Refresh::default()));
    }

    #[test]
    fn force_rebuilds_uncached_and_pulls_only_registry_bases() {
        let dir = tempfile::tempdir().unwrap();
        let a = ext(dir.path(), "a", "#!/bin/sh\n");
        let stack = layers(&BaseImage::Bundled, &[a]).unwrap();
        let backend = FakeBackend::default();
        for layer in &stack {
            backend.images.borrow_mut().insert(layer.tag.clone());
        }

        let actions = ensure(&backend, &stack, true, false).unwrap();
        assert_eq!(actions, [Action::Build, Action::Build]);
        let built = backend.built.borrow();
        let full = Refresh {
            no_cache: true,
            pull: true,
        };
        assert_eq!(built[0].2, full);
        assert_eq!(
            built[1].2,
            Refresh {
                pull: false,
                ..full
            }
        );
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
        let stack = layers(&BaseImage::Bundled, &[]).unwrap();
        let backend = FakeBackend::default();
        let actions = ensure(&backend, &stack, false, true).unwrap();
        assert_eq!(actions, [Action::Build]);
        assert!(backend.built.borrow().is_empty());
    }

    #[test]
    fn custom_base_must_exist_and_is_never_built() {
        let stack = layers(&BaseImage::Custom("mine:1".into()), &[]).unwrap();
        let backend = FakeBackend::default();
        assert!(ensure(&backend, &stack, true, false).is_err());
        backend.images.borrow_mut().insert("mine:1".into());
        assert_eq!(
            ensure(&backend, &stack, true, false).unwrap(),
            [Action::Present]
        );
        assert!(backend.built.borrow().is_empty());
    }
}
