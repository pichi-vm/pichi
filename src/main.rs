// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! `pichi`: the high-level, docker/podman-like front-end for the pichi VM
//! ecosystem.
//!
//! This binary owns image management — pulling, pushing, importing, and
//! inspecting OCI artifacts in the local content-addressed cache. Booting a
//! VM is delegated to the separate `dillo` launcher: `pichi run` derives the
//! device set from the cached artifact, then `exec()`s `dillo`.

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
    /// Import a raw image into the local cache as a base carapace artifact.
    Import(cli::ImportArgs),
    /// Build an artifact from a `pichi.build/` project inside a VM.
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
    /// Resolve `pichi.build/*.yaml` carapace references into `refs.lock`.
    Update(cli::UpdateArgs),
    /// Boot a cached artifact by preparing the environment and exec'ing dillo.
    Run(cli::RunArgs),
}

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    let config = config::Config::load().context("failed to load pichi config")?;

    match cli.command {
        Command::System { cmd } => match cmd {
            cli::SystemCmd::Info(args) => system::run(args, &config),
            cli::SystemCmd::Prune(args) => cmd::prune::run(args, &config),
        },
        Command::Images(args) => cmd::images::run(args, &config),
        Command::Inspect(args) => cmd::inspect::run(args, &config),
        Command::Rmi(args) => cmd::rmi::run(args, &config),
        Command::Tag(args) => cmd::tag::run(args, &config),
        Command::Import(args) => cmd::import::run(args, &config),
        Command::Build(args) => cmd::build::run(args, &config),
        Command::Pull(args) => cmd::pull::run(args, &config),
        Command::Push(args) => cmd::push::run(args, &config),
        Command::Manifest { cmd } => match cmd {
            cli::ManifestCmd::Create(args) => cmd::manifest::create(args, &config),
            cli::ManifestCmd::Annotate(args) => cmd::manifest::annotate(args, &config),
            cli::ManifestCmd::Push(args) => cmd::manifest::push(args, &config),
        },
        Command::Update(args) => cmd::update::run(args, &config),
        Command::Run(args) => cmd::run::run(args, &config),
    }
}
