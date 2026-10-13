//! Image layering: the bundled base image plus one derived image per profile
//! extension script. Every tag is a hash of everything that went into the
//! image, so changing any input produces a new tag and stale images are never
//! reused.

use std::fs;

use anyhow::{Context, Result, bail};

use crate::backend::Backend;
use crate::hash::sha256_hex_fields;
use crate::profile::Extension;

const BASE_CONTAINERFILE: &str = include_str!("../base/Containerfile");
const BASE_ENTRYPOINT: &str = include_str!("../base/entrypoint.sh");

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
    Custom,
    Extension {
        parent: String,
        profile: String,
        script: Vec<u8>,
    },
}

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
        let hash = sha256_hex_fields(&[parent.as_bytes(), ext.profile.as_bytes(), &script]);
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

/// What [`ensure`] did or would do with a layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Present,
    Build,
}

/// Builds any missing layers (all of them with `force`) and returns the
/// action taken per layer. With `dry_run`, nothing is built.
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
                build(backend, layer)?;
            }
        }
        actions.push(action);
    }
    Ok(actions)
}

fn build(backend: &dyn Backend, layer: &Layer) -> Result<()> {
    let ctx = tempfile::Builder::new().prefix("podbay-build-").tempdir()?;
    match &layer.source {
        LayerSource::Bundled => {
            fs::write(ctx.path().join("Containerfile"), BASE_CONTAINERFILE)?;
            fs::write(ctx.path().join("entrypoint.sh"), BASE_ENTRYPOINT)?;
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
        LayerSource::Custom => unreachable!("custom images are never built"),
    }
    eprintln!("podbay: building {}", layer.tag);
    backend.build_image(&layer.tag, ctx.path())
}

fn extension_containerfile(parent: &str, profile: &str) -> String {
    let dest = format!("/usr/local/share/podbay/extensions/{profile}.sh");
    format!(
        "FROM {parent}\n\
         USER root\n\
         COPY extension.sh {dest}\n\
         RUN chmod 0755 {dest} && {dest}\n"
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
        built: RefCell<Vec<(String, String)>>,
    }

    impl Backend for FakeBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Docker
        }
        fn image_exists(&self, image: &str) -> Result<bool> {
            Ok(self.images.borrow().contains(image))
        }
        fn build_image(&self, tag: &str, context: &Path) -> Result<()> {
            let containerfile = fs::read_to_string(context.join("Containerfile"))?;
            self.built.borrow_mut().push((tag.into(), containerfile));
            self.images.borrow_mut().insert(tag.into());
            Ok(())
        }
        fn identity(&self) -> Result<Identity> {
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
