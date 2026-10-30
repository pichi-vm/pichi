// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

#[test]
fn rpm_manifest_keeps_runtime_dependencies_and_removes_test_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    // RPM builds use repository text with LF endings. Windows checkouts may
    // convert both inputs to CRLF, which is not valid unified-patch syntax.
    let source = include_str!("../Cargo.toml").replace("\r\n", "\n");
    std::fs::write(directory.path().join("Cargo.toml"), &source).unwrap();
    let patch = directory.path().join("rpm-manifest.patch");
    std::fs::write(
        &patch,
        include_str!("../packaging/rpm-manifest.patch").replace("\r\n", "\n"),
    )
    .unwrap();
    let result = Command::new("git")
        .current_dir(directory.path())
        .arg("apply")
        .arg(patch)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let original: toml::Value = toml::from_str(&source).unwrap();
    let text = std::fs::read_to_string(directory.path().join("Cargo.toml")).unwrap();
    let packaged: toml::Value = toml::from_str(&text).expect("RPM patch must produce valid TOML");
    assert_eq!(packaged["package"], original["package"]);
    assert_eq!(packaged["workspace"], original["workspace"]);
    assert!(packaged.get("dev-dependencies").is_none());
    assert!(
        packaged
            .get("features")
            .and_then(|v| v.get("vm-tests"))
            .is_none()
    );
    for (name, value) in original["dependencies"].as_table().unwrap() {
        if value.get("artifact").is_none() {
            assert_eq!(&packaged["dependencies"][name], value, "{name}");
        } else {
            assert!(packaged["dependencies"].get(name).is_none(), "{name}");
        }
    }
    for target in packaged["target"].as_table().unwrap().values() {
        for dependency in target["dependencies"].as_table().unwrap().values() {
            assert!(dependency.get("artifact").is_none());
        }
    }
}
