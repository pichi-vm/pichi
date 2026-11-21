// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Batch wait selection, output order and per-workload failures through the CLI.

#[path = "support/storage.rs"]
mod storage;
use storage::Storage as _;

use assert_cmd::Command;

#[test]
fn waits_in_argument_order_and_continues_after_failed_observation() {
    let root = tempfile::tempdir().unwrap();
    let command = || {
        let mut command = Command::cargo_bin("pichi").unwrap();
        command.storage(root.path());
        command
    };
    command().arg("wait").assert().code(125).stdout("");
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
        "33333333-3333-4333-8333-333333333333",
    ];
    for (id, code) in ids.into_iter().zip([Some(23), Some(7), None]) {
        let record = records.join(id);
        std::fs::create_dir(&record).unwrap();
        let identity = serde_json::json!({
            "version": 1, "id": id,
            "images": [format!("sha256:{}", "0".repeat(64))],
            "endpoint": root.path().join("missing.sock"),
        });
        std::fs::write(record.join("identity.json"), identity.to_string()).unwrap();
        if let Some(code) = code {
            let completion = madrillo::Completion {
                version: madrillo::PROTOCOL_VERSION,
                leader: madrillo::LeaderResult::Exit { code },
                cause: madrillo::CompletionCause::LeaderExit,
                exited_at_unix_ms: 0,
                cleanup: madrillo::CleanupResult::Complete,
                oom: madrillo::OomEvidence::NoneObserved,
                log_errors: Vec::new(),
                monitor_errors: Vec::new(),
                monitor_warnings: Vec::new(),
                output_truncated: false,
            };
            let monitor = record.join("monitor");
            std::fs::create_dir(&monitor).unwrap();
            std::fs::write(
                monitor.join("result.json"),
                serde_json::to_vec(&completion).unwrap(),
            )
            .unwrap();
        }
    }
    command()
        .args(["wait", ids[1], &ids[0][..8], ids[1]])
        .assert()
        .success()
        .stdout("7\n23\n7\n");
    command()
        .args(["wait", ids[0], "ffffffff"])
        .assert()
        .code(125)
        .stdout("");
    let failed = command()
        .args(["wait", ids[2], ids[0], ids[1]])
        .assert()
        .code(125)
        .stdout("23\n7\n");
    assert!(String::from_utf8_lossy(&failed.get_output().stderr).contains(ids[2]));
}
