// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use assert_cmd::Command;
use serde_json::{Value, json};

#[test]
fn system_info_reports_native_user_config_and_explicit_overrides() {
    let tmp = tempfile::tempdir().unwrap();
    let config_root = tmp.path().join("native config");
    let user_config = config_root.join("pichi").join("config.toml");
    let graph = tmp.path().join("user graph,=cache");
    let runtime = tmp.path().join("user runtime");
    std::fs::create_dir_all(user_config.parent().unwrap()).unwrap();
    std::fs::write(
        &user_config,
        toml::to_string(&json!({"storage": {"graphroot": graph, "runroot": runtime}})).unwrap(),
    )
    .unwrap();
    let explicit_config = tmp.path().join("explicit.toml");
    let override_graph = tmp.path().join("explicit graph");
    std::fs::write(
        &explicit_config,
        toml::to_string(&json!({"storage": {"graphroot": override_graph}})).unwrap(),
    )
    .unwrap();

    for explicit in [false, true] {
        let mut command = Command::cargo_bin("pichi").unwrap();
        command
            .args(["system", "info", "--json"])
            .env("HOME", tmp.path().join("home"))
            .env("PROGRAMDATA", tmp.path().join("system"))
            .env("LOCALAPPDATA", tmp.path().join("data"))
            .env("XDG_DATA_HOME", tmp.path().join("data"))
            .env("XDG_RUNTIME_DIR", tmp.path().join("runtime"))
            .env("APPDATA", tmp.path().join("unused"))
            .env("XDG_CONFIG_HOME", tmp.path().join("unused"))
            .env(
                if cfg!(windows) {
                    "APPDATA"
                } else {
                    "XDG_CONFIG_HOME"
                },
                &config_root,
            )
            .env_remove("PICHI_CONFIG");
        if explicit {
            command.env("PICHI_CONFIG", &explicit_config);
        }
        let result = command.assert().success();
        let info: Value = serde_json::from_slice(&result.get_output().stdout).unwrap();
        let expected_graph = if explicit { &override_graph } else { &graph };
        assert_eq!(info["store"]["graphRoot"], json!(expected_graph));
        assert_eq!(info["store"]["runRoot"], json!(runtime));
        let files = info["config"]["files"].as_array().unwrap();
        assert!(files.contains(&json!({"path": user_config, "status": "loaded"})));
        if explicit {
            assert!(files.contains(&json!({"path": explicit_config, "status": "loaded"})));
        }
    }
}
