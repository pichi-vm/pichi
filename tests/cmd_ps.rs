// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Workload listing output through the CLI.

#[path = "support/storage.rs"]
mod storage;
use storage::Storage as _;

use assert_cmd::Command;

#[test]
fn quiet_lists_the_same_full_ids_as_the_table() {
    let root = tempfile::tempdir().unwrap();
    let command = || {
        let mut command = Command::cargo_bin("pichi").unwrap();
        command.storage(root.path());
        command
    };
    command().args(["ps", "-q"]).assert().success().stdout("");
    let records = root.path().join("pichi/storage.workloads");
    std::fs::create_dir_all(&records).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&records, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let ids = [
        "11111111-1111-4111-8111-111111111111",
        "22222222-2222-4222-8222-222222222222",
    ];
    for id in ids {
        let record = records.join(id);
        std::fs::create_dir(&record).unwrap();
        let identity = serde_json::json!({
            "version": 1, "id": id,
            "images": [format!("sha256:{}", "0".repeat(64))],
            "endpoint": root.path().join("missing.sock"),
        });
        std::fs::write(record.join("identity.json"), identity.to_string()).unwrap();
    }
    let expected = format!("{}\n", ids.join("\n"));
    for option in ["-q", "--quiet"] {
        command()
            .args(["ps", option])
            .assert()
            .success()
            .stdout(expected.clone());
    }
    let table = command().arg("ps").assert().success();
    let table = std::str::from_utf8(&table.get_output().stdout).unwrap();
    assert_eq!(
        table.lines().next(),
        Some("ID                                   STATE    IMAGES")
    );
    assert_eq!(
        table
            .lines()
            .skip(1)
            .map(|line| line.split_whitespace().next().unwrap())
            .collect::<Vec<_>>(),
        ids
    );
    // One confirmed completion changes selection, not the visibility of the
    // other record whose monitor is unavailable and state remains unknown.
    let monitor = records.join(ids[1]).join("monitor");
    std::fs::create_dir(&monitor).unwrap();
    let completion = madrillo::Completion {
        version: madrillo::PROTOCOL_VERSION,
        leader: madrillo::LeaderResult::Exit { code: 23 },
        cause: madrillo::CompletionCause::LeaderExit,
        exited_at_unix_ms: 0,
        cleanup: madrillo::CleanupResult::Complete,
        oom: madrillo::OomEvidence::NoneObserved,
        log_errors: Vec::new(),
        monitor_errors: Vec::new(),
        monitor_warnings: Vec::new(),
        output_truncated: false,
    };
    std::fs::write(
        monitor.join("result.json"),
        serde_json::to_vec(&completion).unwrap(),
    )
    .unwrap();
    command()
        .args(["ps", "-q"])
        .assert()
        .success()
        .stdout(format!("{}\n", ids[0]));
    for arguments in [["ps", "-a", "-q"], ["ps", "--all", "--quiet"]] {
        command()
            .args(arguments)
            .assert()
            .success()
            .stdout(expected.clone());
    }
    let table = command().arg("ps").assert().success();
    let table = std::str::from_utf8(&table.get_output().stdout).unwrap();
    assert!(
        table.contains(ids[0]) && table.contains("unknown"),
        "{table}"
    );
    assert!(!table.contains(ids[1]), "{table}");
    let all = command().args(["ps", "--all"]).assert().success();
    let all = std::str::from_utf8(&all.get_output().stdout).unwrap();
    assert!(
        all.contains(ids[0]) && all.contains(ids[1]) && all.contains("exited"),
        "{all}"
    );
}
