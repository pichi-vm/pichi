// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Resolve a working-tree plan's selected remote images into an index lock.

use super::cache::ImageStore as _;
use crate::{
    cli::{PullArgs, UpdateArgs},
    cmd::manifest_ext::ManifestExt as _,
    config::Config,
};
use anyhow::{Context, Result, ensure};
use cap_std::fs::{Dir, OpenOptions};
use conglobate::{
    artifact::Reference,
    build::{ImagePin, Lock},
    plan::{Boot, Directive, Image, ImageRef, Plan, Stage},
};
use pichi_storage::{FilesystemBlobStore, FilesystemTagDb};
use std::{
    collections::BTreeSet,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

pub async fn run(args: UpdateArgs, config: &Config) -> Result<()> {
    let directory = Dir::open_ambient_dir(&args.dir, cap_std::ambient_authority())
        .context("opening project directory")?;
    let inputs = args.inspect(&directory)?;
    let lock = args.resolve(&inputs, config).await?;
    let (path, previous) = args.write_lock(&directory, &lock)?;
    if let Some(previous) = previous {
        let added = lock
            .images
            .keys()
            .filter(|key| !previous.images.contains_key(*key))
            .count();
        let changed = lock
            .images
            .iter()
            .filter(|(key, pin)| previous.images.get(*key).is_some_and(|old| old != *pin))
            .count();
        let removed = previous
            .images
            .keys()
            .filter(|key| !lock.images.contains_key(*key))
            .count();
        println!(
            "pichi update: {added} added, {changed} changed, {removed} removed -> {}",
            path.display()
        );
    } else {
        println!(
            "pichi update: pinned {} reference(s) -> {} (previous lock unavailable for comparison)",
            lock.images.len(),
            path.display()
        );
    }
    Ok(())
}

impl UpdateArgs {
    fn inspect(&self, directory: &Dir) -> Result<BTreeSet<String>> {
        ensure!(
            directory.metadata(&self.plan)?.is_file(),
            "plan must be a regular file"
        );
        let text = directory
            .read_to_string(&self.plan)
            .with_context(|| format!("reading plan {} within project directory", self.plan))?;
        let plan: Plan = serde_saphyr::from_str_with_options(
            &text,
            serde_saphyr::options! {
                strict_booleans: true,
                merge_keys: serde_saphyr::MergeKeyPolicy::AsOrdinary,
            },
        )
        .with_context(|| format!("parsing plan {}", self.plan))?;
        plan.inputs()
    }

    async fn resolve(&self, inputs: &BTreeSet<String>, config: &Config) -> Result<Lock> {
        let mut lock = Lock::default();
        if inputs.is_empty() {
            return Ok(lock);
        }
        let layout = config.resolve_layout()?;
        let db = FilesystemTagDb::open(&layout.graphroot)?;
        let blobs = FilesystemBlobStore::new(&layout.graphroot);
        for text in inputs {
            super::pull::run(
                PullArgs {
                    reference: text.clone(),
                    pull: Some(self.pull),
                    quiet: true,
                },
                config,
            )
            .await?;
            let reference: Reference = super::build::canonicalize_reference(text).parse()?;
            let digest = blobs.resolve(&reference, &db).await?;
            ensure!(
                blobs.metadata(&digest).await?.is_index(),
                "{text} is a bare manifest; import or publish it as a platform index before locking"
            );
            let (_, manifest) = blobs.manifest(&digest).await?;
            manifest.check_architecture()?;
            lock.images.insert(text.clone(), ImagePin { index: digest });
        }
        Ok(lock)
    }

    fn write_lock(&self, directory: &Dir, lock: &Lock) -> Result<(PathBuf, Option<Lock>)> {
        let path = Path::new(&self.lock);
        let name = path.file_name().context("lock path must name a file")?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = directory
            .open_dir(parent)
            .context("opening lock directory within project directory")?;
        match parent.symlink_metadata(name) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file(),
                    "lock path must be a regular file, not a symlink or directory"
                );
                ensure!(
                    directory.canonicalize(path)? != directory.canonicalize(&self.plan)?,
                    "lock must not replace the plan"
                );
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("checking lock path"),
        }
        // An existing lock affects the change summary only, never resolution.
        let previous = match parent.read_to_string(name) {
            Ok(text) => serde_saphyr::from_str::<Lock>(&text).ok(),
            Err(error) if error.kind() == ErrorKind::NotFound => Some(Lock::default()),
            Err(_) => None,
        };
        let temporary = format!(".pichi-lock-{}", uuid::Uuid::new_v4());
        let mut file =
            parent.open_with(&temporary, OpenOptions::new().write(true).create_new(true))?;
        let result = (|| -> Result<()> {
            writeln!(file, "# Generated by pichi update; commit with the plan.")?;
            serde_saphyr::to_io_writer(&mut file, lock)?;
            file.sync_all()?;
            drop(file);
            parent.rename(&temporary, &parent, name)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = parent.remove_file(&temporary);
        }
        result.with_context(|| format!("writing {}", self.dir.join(path).display()))?;
        Ok((self.dir.join(path), previous))
    }
}

/// Dependency discovery is a Pichi concern; shared types own the encoding.
trait PlanExt {
    fn inputs(&self) -> Result<BTreeSet<String>>;
}

impl PlanExt for Plan {
    fn inputs(&self) -> Result<BTreeSet<String>> {
        let sources: Vec<_> = match &self.image {
            Image::Shorthand(source) => vec![source],
            Image::Components { root, boot } => root
                .iter()
                .chain(boot.iter().map(|boot| match boot {
                    Boot::Shorthand(source) => source,
                    Boot::Inherit { inherit }
                    | Boot::InheritDefined { inherit, .. }
                    | Boot::InheritExtended { inherit, .. } => inherit,
                    Boot::Extract { extract, .. } => &extract.from,
                }))
                .collect(),
        };
        ensure!(!sources.is_empty(), "image contains neither root nor PMI");
        let mut images = BTreeSet::new();
        let mut selected = Vec::new();
        for source in sources {
            match source {
                ImageRef::Local(name) => selected.push(name),
                ImageRef::Remote(remote) => {
                    images.insert(remote.to_string());
                }
            }
        }
        let mut done = BTreeSet::new();
        let mut visiting = BTreeSet::new();
        // Validate the entire graph, but lock only the selected closure.
        for (collect, starts) in [(true, selected), (false, self.stages.keys().collect())] {
            for start in starts {
                let mut stack = vec![(start, false)];
                while let Some((name, exit)) = stack.pop() {
                    if exit {
                        visiting.remove(name);
                        done.insert(name);
                        continue;
                    }
                    if done.contains(name) {
                        continue;
                    }
                    let stage = self
                        .stages
                        .get(name)
                        .with_context(|| format!("unknown stage {name}"))?;
                    ensure!(visiting.insert(name), "stage dependency cycle at {name}");
                    let (source, steps) = match stage {
                        Stage::Create { r#use, steps, .. } => (r#use, steps),
                        Stage::Extend { extend, steps, .. } => (extend, steps),
                    };
                    stack.push((name, true));
                    match source {
                        ImageRef::Local(source) => stack.push((source, false)),
                        ImageRef::Remote(remote) if collect => {
                            images.insert(remote.to_string());
                        }
                        ImageRef::Remote(_) => {}
                    }
                    for step in steps {
                        if let Directive::Copy {
                            from: Some(source), ..
                        } = step
                        {
                            stack.push((source, false));
                        }
                    }
                }
            }
        }
        Ok(images)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;

    use pichi_storage::{BlobStore, TagDb};
    use std::collections::BTreeMap;

    fn arguments(dir: &Path) -> UpdateArgs {
        std::fs::write(dir.join("pichi.yaml"), "image: registry.example/base:1\n").unwrap();
        UpdateArgs {
            dir: dir.into(),
            plan: "pichi.yaml".into(),
            lock: "pichi.lock".into(),
            pull: crate::cli::PullPolicy::Never,
        }
    }

    #[tokio::test]
    async fn update_keeps_the_existing_index_and_cache_references() {
        let tmp = tempfile::tempdir().unwrap();
        let graphroot = tmp.path().join("storage");
        let raw = tmp.path().join("base.raw");
        std::fs::write(&raw, vec![0x42; 64 * 1024]).unwrap();
        let reference = "registry.example/base:1";
        crate::cmd::import::raw::ImportArgs {
            raw_image: raw,
            tag: Some(reference.into()),
            quiet: true,
            created_rfc3339: "2026-06-22T00:00:00Z".into(),
            annotations: BTreeMap::new(),
        }
        .run(&graphroot)
        .await
        .unwrap();
        let config = Config {
            storage: StorageConfig {
                graphroot: Some(graphroot.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let db = FilesystemTagDb::open(&graphroot).unwrap();
        let blobs = FilesystemBlobStore::new(&graphroot);
        let original = db.resolve_tag(reference).await.unwrap().unwrap();
        let original_bytes = blobs.get_blob(&original).await.unwrap();
        let args = arguments(tmp.path());
        let lock = args
            .resolve(&[reference.into()].into(), &config)
            .await
            .unwrap();
        let pin = &lock.images[reference].index;
        assert_eq!(pin, &original);
        assert_eq!(
            db.resolve_tag(reference).await.unwrap(),
            Some(original.clone())
        );
        assert_eq!(blobs.get_blob(pin).await.unwrap(), original_bytes);
        let pinned = format!("registry.example/base@{pin}");
        let pinned_inputs = [pinned.clone()].into();
        crate::cmd::prune::run(crate::cli::PruneArgs { dry_run: false }, &config)
            .await
            .unwrap();
        let (resolved, _) = crate::cmd::build::resolve_manifest(&pinned, &db, &blobs)
            .await
            .unwrap();
        assert_eq!(resolved, *pin);
        let pinned_lock = args.resolve(&pinned_inputs, &config).await.unwrap();
        assert_eq!(pinned_lock.images[&pinned].index, *pin);
        let (path, _) = args
            .write_lock(
                &Dir::open_ambient_dir(tmp.path(), cap_std::ambient_authority()).unwrap(),
                &lock,
            )
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            String::from_utf8(before.clone()).unwrap(),
            format!(
                "# Generated by pichi update; commit with the plan.\nversion: 1\nimages:\n  \"{reference}\":\n    index: {pin}\n"
            )
        );
        let decoded: Lock = serde_saphyr::from_slice(&before).unwrap();
        assert_eq!(decoded, lock);
        assert!(
            args.resolve(&["registry.example/missing:1".into()].into(), &config)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let (selected, _) = blobs.manifest(pin).await.unwrap();
        let bare = [format!("registry.example/base@{}", selected.digest)].into();
        assert!(
            args.resolve(&bare, &config)
                .await
                .unwrap_err()
                .to_string()
                .contains("bare manifest")
        );
        std::fs::write(blobs.blob_path(pin), b"tampered").unwrap();
        assert!(args.resolve(&pinned_inputs, &config).await.is_err());
    }

    #[tokio::test]
    async fn empty_inputs_write_an_empty_current_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let args = arguments(tmp.path());
        let lock = args
            .resolve(&BTreeSet::new(), &Config::default())
            .await
            .unwrap();
        assert!(lock.images.is_empty());
        let (path, _) = args
            .write_lock(
                &Dir::open_ambient_dir(tmp.path(), cap_std::ambient_authority()).unwrap(),
                &lock,
            )
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "# Generated by pichi update; commit with the plan.\nversion: 1\nimages: {}\n"
        );
        let mut args = args;
        args.lock = "../pichi.lock".into();
        assert!(
            args.write_lock(
                &Dir::open_ambient_dir(tmp.path(), cap_std::ambient_authority()).unwrap(),
                &lock
            )
            .is_err()
        );
    }

    #[test]
    fn selected_closure_covers_copies_creation_and_every_boot_form() {
        for boot in [
            "registry.test/boot:1",
            "{inherit: registry.test/boot:1}",
            "{inherit: registry.test/boot:1, define: {}}",
            "{inherit: registry.test/boot:1, extend: {kernel.quiet: null}}",
            "{extract: {from: registry.test/boot:1, path: /boot.pmi}}",
            "{extract: {from: firmware, path: /boot.pmi}}",
        ] {
            let plan: Plan = serde_saphyr::from_str(&format!(r#"
stages:
  tools: {{use: registry.test/tools:1}}
  root: {{extend: tools, steps: [{{copy: {{from: producer, path: /out, into: /out}}}}, {{exec: {{command: 'must never run'}}}}]}}
  producer: {{extend: tools, steps: [{{copy: {{from: other, path: /out, into: /out}}}}]}}
  other: {{extend: registry.test/other:1}}
  firmware: {{extend: registry.test/boot:1}}
  unused: {{extend: registry.test/unused:1}}
image: {{root: root, boot: {boot}}}
"#)).unwrap();
            assert_eq!(
                plan.inputs().unwrap(),
                [
                    "registry.test/tools:1",
                    "registry.test/other:1",
                    "registry.test/boot:1"
                ]
                .map(str::to_owned)
                .into()
            );
        }
        for image in [
            "registry.test/base:1",
            "{root: registry.test/base:1}",
            "{boot: registry.test/base:1}",
            "{root: registry.test/base:1, boot: registry.test/base:1}",
        ] {
            let plan: Plan = serde_saphyr::from_str(&format!("image: {image}")).unwrap();
            assert_eq!(
                plan.inputs().unwrap(),
                ["registry.test/base:1".into()].into()
            );
        }
    }

    #[test]
    fn missing_stages_cycles_and_empty_selections_are_rejected() {
        for text in [
            "image: {}",
            "image: missing",
            "image: {boot: missing}",
            "image: a\nstages: {a: {extend: a}}",
            "image: a\nstages: {a: {extend: b}, b: {extend: a}}",
            "image: a\nstages: {a: {extend: registry.test/base:1, steps: [{copy: {from: missing, path: /x, into: /x}}]}}",
            "image: a\nstages: {a: {extend: registry.test/base:1, steps: [{copy: {from: a, path: /x, into: /x}}]}}",
            "image: registry.test/base:1\nstages: {unused: {extend: missing}}",
            "image: registry.test/base:1\nstages: {a: {extend: b}, b: {extend: a}}",
        ] {
            let plan: Plan = serde_saphyr::from_str(text).unwrap();
            assert!(plan.inputs().is_err(), "accepted {text}");
        }
    }
}
