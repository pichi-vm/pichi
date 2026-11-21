// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

/// Give every subprocess the fixture store, including rootful and Windows runs.
pub(super) trait Storage {
    fn storage(&mut self, home: impl AsRef<Path>) -> &mut Self;
}

impl Storage for std::process::Command {
    fn storage(&mut self, home: impl AsRef<Path>) -> &mut Self {
        let home = home.as_ref();
        let directory = home.join("pichi");
        std::fs::create_dir_all(&directory).unwrap();
        let config = directory.join("config.toml");
        if !config.exists() {
            let value = pichi::config::Config {
                registries: std::env::var("PICHI_TEST_REGISTRY")
                    .ok()
                    .filter(|prefix| !prefix.is_empty())
                    .map(|prefix| pichi::config::RegistryConfig {
                        prefix,
                        insecure: true,
                        ..Default::default()
                    })
                    .into_iter()
                    .collect(),
                storage: pichi::config::StorageConfig {
                    graphroot: Some(directory.join("storage")),
                    runroot: Some(directory.join("run")),
                },
                ..Default::default()
            };
            std::fs::write(config, toml::to_string(&value).unwrap()).unwrap();
        }
        self.env("XDG_DATA_HOME", home)
            .env("XDG_CONFIG_HOME", home)
            .env("LOCALAPPDATA", home)
            .env("APPDATA", home)
            .env("PROGRAMDATA", home)
    }
}

impl Storage for assert_cmd::Command {
    fn storage(&mut self, home: impl AsRef<Path>) -> &mut Self {
        let mut settings = std::process::Command::new("pichi");
        self.envs(
            settings
                .storage(home)
                .get_envs()
                .map(|(key, value)| (key, value.unwrap())),
        )
    }
}
