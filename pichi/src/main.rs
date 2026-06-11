// SPDX-License-Identifier: Apache-2.0

//! `pichi`: the high-level, docker/podman-like front-end for the pichi VM
//! ecosystem.
//!
//! This binary owns image management — pulling, pushing, importing, and
//! inspecting OCI artifacts in the local content-addressed cache. Actually
//! booting a VM is delegated to the separate `dillo` launcher (pichi prepares
//! the environment and `exec()`s it); that path is not yet wired here.

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
    /// Pull a pichi artifact from an OCI registry.
    Pull(cli::PullArgs),
    /// Push a cached pichi artifact to an OCI registry.
    Push(cli::PushArgs),
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
        Command::Pull(args) => cmd::pull::run(args, &config),
        Command::Push(args) => cmd::push::run(args, &config),
    }
}
