// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Create immutable instance configuration and disk without launching a monitor.

use super::{
    build::{canonicalize_reference, image_cached},
    cache::ImageStore as _,
    manifest_ext::ManifestExt as _,
    requirements,
    workload::{
        instance::{Instance, Spec},
        record::Records,
    },
    writable_storage::WritableStorage,
};
use crate::{cli::CreateArgs, config::Config};
use anyhow::{Context, Result, ensure};
use conglobate::artifact::{Memory, Reference, Requirements};
use pichi_storage::{FilesystemBlobStore, FilesystemTagDb};

/// Dillo defaults become explicit creation-time resource choices.
const DEFAULT_CPUS: u32 = 1;
const DEFAULT_MEMORY_MIB: u32 = 1024;

pub async fn run(args: CreateArgs, config: &Config) -> Result<()> {
    ensure!(
        config.run.storage_file.is_none(),
        "create uses an instance-owned disk; remove run.storage_file from the configuration"
    );
    let layout = config.resolve_layout()?;
    let db = FilesystemTagDb::open(&layout.graphroot)?;
    let blob_store = FilesystemBlobStore::new(&layout.graphroot);

    // Resolve indexes through the same path as build, including informational
    // tags on digest-pinned references.
    let canonical = canonicalize_reference(&args.reference);
    let target_ref: Reference = canonical
        .parse()
        .with_context(|| format!("invalid reference: {}", args.reference))?;
    if !image_cached(&canonical, &db, &blob_store).await? {
        super::run::auto_pull(&target_ref, config).await?;
    }
    // Serialize resolution, validation and publication of the image pin with
    // image deletion.
    let instance = layout
        .with_index_lock(|| async {
            let root = blob_store.resolve(&target_ref, &db).await?;
            let (metadata, manifest) = blob_store
                .cached_manifest(&root)
                .await
                .with_context(|| format!("reading image {}", args.reference))?;

            // Architecture-mismatch fail-closed check (accept if absent).
            manifest.check_architecture()?;
            manifest.partition_layers()?;

            // Apply the artifact config's required floors (BUILD.md §7) and
            // freeze concrete resources for every later boot.
            let reqs = requirements::load_requirements(&manifest, &blob_store).await?;
            let cpus = requirements::Sizing::Cpus
                .resolve(
                    args.cpus.or(config.run.cpus),
                    reqs.as_ref().and_then(Requirements::cpus_required),
                    reqs.as_ref().and_then(Requirements::cpus_recommended),
                )?
                .unwrap_or(DEFAULT_CPUS);
            let memory_mib = requirements::Sizing::Memory
                .resolve(
                    args.memory.or(config.run.memory_mib),
                    reqs.as_ref().and_then(Requirements::memory_required_mib),
                    reqs.as_ref().and_then(Requirements::memory_recommended_mib),
                )?
                .unwrap_or(DEFAULT_MEMORY_MIB);
            let memory_mib = Memory::from_mib(memory_mib)
                .context("memory must be a positive multiple of 2 MiB")?;
            let requirements = reqs.unwrap_or_default();
            let records = Records::open(&layout.graphroot)?;
            let available = fs4::available_space(records.path())?;
            let storage_bytes = WritableStorage::capacity(
                Some(&requirements.storage),
                args.storage.or(config.run.storage_mib),
                available,
            )?;
            ensure!(
                storage_bytes <= available,
                "insufficient host space for storage allocation"
            );
            Instance::create(
                &records,
                Spec {
                    root,
                    manifest: metadata.digest,
                    cpus,
                    memory_mib,
                    storage_bytes,
                },
            )
        })
        .await?;
    println!("{}", instance.id());
    Ok(())
}
