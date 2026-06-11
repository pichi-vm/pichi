// SPDX-License-Identifier: Apache-2.0

//! `pichi system info` subcommand. STORAGE-09.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;

use pichi_storage::{CacheLayout, Mode};

use crate::cli::InfoArgs;
use crate::config::Config;

/// Dispatch entry point for `pichi system <verb>`.
pub fn run(args: InfoArgs, config: &Config) -> Result<()> {
    let layout = CacheLayout::resolve()?;

    // Apply config overrides for graphroot / runroot (visibility only —
    // the layout reported here matches what other commands will see).
    let graph_root = config
        .storage
        .graphroot
        .clone()
        .unwrap_or_else(|| layout.graphroot.clone());
    let run_root = config
        .storage
        .runroot
        .clone()
        .unwrap_or_else(|| layout.runroot.clone());

    let mode_str = match layout.mode {
        Mode::Rootless => "rootless",
        Mode::Rootful => "rootful",
    };

    let files = collect_config_file_entries();

    let info = InfoOutput {
        store: StoreInfo {
            graph_root: graph_root.to_string_lossy().into_owned(),
            run_root: run_root.to_string_lossy().into_owned(),
            mode: mode_str,
        },
        config: ConfigInfo { files },
        version: VersionInfo {
            pichi: env!("CARGO_PKG_VERSION"),
        },
    };

    if args.json {
        let json = serde_json::to_string_pretty(&info)?;
        println!("{json}");
    } else {
        info.print_text();
    }
    Ok(())
}

fn collect_config_file_entries() -> Vec<ConfigFileEntry> {
    let mut files = Vec::new();

    // Report exactly the paths the loader consults (per-OS), so `system
    // info` never disagrees with `Config::load`.
    if let Some(p) = crate::config::system_config_path() {
        files.push(entry(p));
    }
    if let Some(p) = crate::config::user_config_path() {
        files.push(entry(p));
    }
    if let Some(env_path) = std::env::var_os("PICHI_CONFIG") {
        files.push(entry(PathBuf::from(env_path)));
    }

    files
}

fn entry(path: PathBuf) -> ConfigFileEntry {
    let status = if std::fs::metadata(&path).is_ok() {
        "loaded"
    } else {
        "absent"
    };
    ConfigFileEntry {
        path: path.to_string_lossy().into_owned(),
        status,
    }
}

#[derive(Serialize)]
struct InfoOutput {
    store: StoreInfo,
    config: ConfigInfo,
    version: VersionInfo,
}

#[derive(Serialize)]
struct StoreInfo {
    #[serde(rename = "graphRoot")]
    graph_root: String,
    #[serde(rename = "runRoot")]
    run_root: String,
    mode: &'static str,
}

#[derive(Serialize)]
struct ConfigInfo {
    files: Vec<ConfigFileEntry>,
}

#[derive(Serialize)]
struct ConfigFileEntry {
    path: String,
    status: &'static str,
}

#[derive(Serialize)]
struct VersionInfo {
    pichi: &'static str,
}

impl InfoOutput {
    fn print_text(&self) {
        println!("store:");
        println!("  graphRoot: {}", self.store.graph_root);
        println!("  runRoot:   {}", self.store.run_root);
        println!("  mode:      {}", self.store.mode);
        println!("config:");
        println!("  files:");
        for f in &self.config.files {
            println!("    - {}  ({})", f.path, f.status);
        }
        println!("version:");
        println!("  pichi:    {}", self.version.pichi);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_output_serialises_to_valid_json() {
        let info = InfoOutput {
            store: StoreInfo {
                graph_root: "/tmp/g".into(),
                run_root: "/tmp/r".into(),
                mode: "rootless",
            },
            config: ConfigInfo {
                files: vec![ConfigFileEntry {
                    path: "/etc/pichi/config.toml".into(),
                    status: "absent",
                }],
            },
            version: VersionInfo { pichi: "0.1.0" },
        };
        let json = serde_json::to_string_pretty(&info).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("store").is_some());
        assert!(parsed.get("config").is_some());
        assert!(parsed.get("version").is_some());
        assert_eq!(parsed["store"]["mode"], "rootless");
        assert_eq!(parsed["store"]["graphRoot"], "/tmp/g");
    }
}
