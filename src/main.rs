// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi`: the high-level, docker/podman-like front-end for the pichi VM
//! ecosystem.
//!
//! This binary owns image management — pulling, pushing, importing, and
//! inspecting OCI artifacts in the local content-addressed cache. Booting a
//! VM is delegated to the separate `dillo` launcher: `pichi run` derives the
//! device set from the cached artifact, then launches it through madrillo.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]

use anyhow::Context;
use clap::{Parser, Subcommand};

use pichi::{cli, cmd, config, system};

/// pichi: image management for the pichi VM ecosystem.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// System-level inspection and maintenance commands.
    System {
        #[command(subcommand)]
        cmd: cli::SystemCmd,
    },
    /// List cached artifacts.
    Images(cli::ImagesArgs),
    /// Inspect a cached manifest.
    Inspect(cli::InspectArgs),
    /// Remove one or more tags (refcount-aware blob GC).
    Rmi(cli::RmiArgs),
    /// Create a new tag pointing at the same manifest digest as `src`.
    Tag(cli::TagArgs),
    /// Bring external bytes into the local cache (raw image → carapace, or a
    /// pre-built PMI → bootable artifact).
    Import {
        #[command(subcommand)]
        cmd: cli::ImportCmd,
    },
    /// Export a cached artifact to an OCI image layout directory.
    Save(cli::SaveArgs),
    /// Import an OCI image layout directory into the local cache.
    Load(cli::LoadArgs),
    /// Build an artifact with the Conglobate appliance.
    Build(cli::BuildArgs),
    /// Pull a pichi artifact from an OCI registry.
    Pull(cli::PullArgs),
    /// Push a cached pichi artifact to an OCI registry.
    Push(cli::PushArgs),
    /// Assemble and push a multi-arch OCI image index (mirrors `docker manifest`).
    Manifest {
        #[command(subcommand)]
        cmd: cli::ManifestCmd,
    },
    /// Update the lock for a working-tree plan.
    Update(cli::UpdateArgs),
    /// Boot a cached artifact under madrillo supervision.
    Run(cli::RunArgs),
    /// Attach to a workload's live console.
    Attach(cli::AttachArgs),

    /// Print retained console output for a workload.
    Logs(cli::WorkloadArgs),

    /// Wait for a workload and print its exit status.
    Wait(cli::WorkloadArgs),

    /// Shut down a workload, forcing cleanup after the grace period.
    Stop(cli::StopArgs),

    /// List retained workloads, including unknown and completed workloads.
    Ps,

    #[command(hide = true)]
    CleanupWorkload { path: std::path::PathBuf },
}

#[tokio::main]
async fn main() -> anyhow::Result<NativeExit> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let cli = Cli::parse();

    let config = if matches!(cli.command, Command::CleanupWorkload { .. }) {
        config::Config::default()
    } else {
        config::Config::load().context("failed to load pichi config")?
    };

    // Every command runs on the one async runtime. Commands whose work is
    // purely local fs/CPU offload their blocking parts via `spawn_blocking`
    // internally; network commands are async end to end.
    match cli.command {
        Command::CleanupWorkload { path } => cmd::workload::Workload::cleanup(&path),
        Command::System { cmd } => match cmd {
            cli::SystemCmd::Info(args) => system::run(args, &config).await,
            cli::SystemCmd::Prune(args) => cmd::prune::run(args, &config).await,
        },
        Command::Images(args) => cmd::images::run(args, &config).await,
        Command::Inspect(args) => cmd::inspect::run(args, &config).await,
        Command::Rmi(args) => cmd::rmi::run(args, &config).await,
        Command::Tag(args) => cmd::tag::run(args, &config).await,
        Command::Import { cmd } => match cmd {
            cli::ImportCmd::Raw(args) => cmd::import::run_raw(args, &config).await,
            cli::ImportCmd::Pmi(args) => cmd::import::run_pmi(args, &config).await,
        },
        Command::Save(args) => cmd::save::run(args, &config).await,
        Command::Load(args) => cmd::load::run(args, &config).await,
        Command::Build(args) => cmd::build::run(args, &config).await,
        Command::Pull(args) => cmd::pull::run(args, &config).await,
        Command::Push(args) => cmd::push::run(args, &config).await,
        Command::Manifest { cmd } => match cmd {
            cli::ManifestCmd::Create(args) => cmd::manifest::create(args, &config).await,
            cli::ManifestCmd::Annotate(args) => cmd::manifest::annotate(args, &config).await,
            cli::ManifestCmd::Push(args) => cmd::manifest::push(args, &config).await,
        },
        Command::Update(args) => cmd::update::run(args, &config).await,
        Command::Run(args) => return Ok(NativeExit(cmd::run::run(args, &config).await?)),
        Command::Attach(args) => {
            return Ok(NativeExit(
                cmd::workload::Workload::open(args.id, &config)?
                    .attach(args.interactive)
                    .await?,
            ));
        }
        Command::Wait(args) => {
            println!(
                "{}",
                cmd::workload::Workload::open(args.id, &config)?
                    .wait()
                    .await?
                    .code()?
            );
            Ok(())
        }
        Command::Logs(args) => {
            cmd::workload::Workload::open(args.id, &config)?
                .logs()
                .await
        }
        Command::Stop(args) => {
            cmd::workload::Workload::open(args.id, &config)?
                .stop(std::time::Duration::from_secs(u64::from(args.time)))
                .await?
                .code()?;
            println!("{}", args.id);
            Ok(())
        }
        Command::Ps => cmd::workload::Workload::list(&config).await,
    }?;
    Ok(NativeExit(0))
}

#[derive(Debug)]
struct NativeExit(u32);

impl std::process::Termination for NativeExit {
    fn report(self) -> std::process::ExitCode {
        // Preserve all Windows status bits; Unix applies its native low byte.
        // The command and runtime have already dropped their resources here.
        std::process::exit(self.0.cast_signed())
    }
}
