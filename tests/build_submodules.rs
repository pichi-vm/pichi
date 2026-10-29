// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Exercise committed submodule stores through the published build appliance
//! (Conglobate's build machine image), using KVM (Linux's VM acceleration).
//! Fixture Git operations allow local files only; no module can be fetched
//! from the network. Four boots share the pulled image and tiny source disk.

#![cfg(all(feature = "vm-tests", target_os = "linux", target_arch = "x86_64"))]

mod common;

use common::submodules::SubmoduleFixture;

#[tokio::test]
async fn pichi_build_checks_committed_submodule_stores() {
    if !common::kvm_available() {
        eprintln!("skip: no usable /dev/kvm");
        return;
    }
    let Some(fixture) = SubmoduleFixture::new() else {
        return;
    };
    let source = fixture.source("source", "RECORDED_SUBMODULE_PAYLOAD");
    let recorded = source.git(&["rev-parse", "HEAD"]);
    let staged = fixture.repo.add_module(&source, "staged", "deps/staged");
    let named_source = fixture.source("named-source", "NAMED_SUBMODULE_PAYLOAD");
    fixture
        .repo
        .add_module(&named_source, "logical-name", "vendor/renamed");
    let depth9 = fixture.nested(9);
    let depth8 = depth9.rsplit_once('/').unwrap().0;
    fixture.missing(&recorded);
    fixture.plan(
        "success.yaml",
        &[
            "deps/staged/payload.txt",
            "vendor/renamed/payload.txt",
            &format!("{depth8}/payload.txt"),
        ],
        false,
    );
    fixture.plan("missing.yaml", &["absent/input/payload.txt"], true);
    fixture.plan("depth.yaml", &[&format!("{depth9}/payload.txt")], true);
    fixture.plan("alternates.yaml", &["vendor/renamed/payload.txt"], true);
    fixture.repo.commit();
    assert!(
        fixture
            .repo
            .git(&["ls-tree", "HEAD", "absent/input"])
            .starts_with("160000 ")
    );

    // Keep the parent's gitlink unchanged while the checked-out module moves
    // to a later commit and then gains different uncommitted bytes as well.
    staged.write("payload.txt", "LATER_SUBMODULE_PAYLOAD");
    assert_ne!(staged.commit(), recorded);
    staged.write("payload.txt", "DIRTY_SUBMODULE_PAYLOAD");
    assert!(
        fixture
            .repo
            .git(&["ls-tree", "HEAD", "deps/staged"])
            .contains(&recorded)
    );

    let built = fixture.build("success.yaml", "submodule-success");
    common::assert_pichi_ok(
        "recorded, named, depth-8 and unused-missing modules",
        &built,
    );
    fixture
        .assert_payloads(
            "submodule-success",
            &[
                "RECORDED_SUBMODULE_PAYLOAD",
                "NAMED_SUBMODULE_PAYLOAD",
                "NESTED_DEPTH_8_PAYLOAD",
            ],
            &[
                "LATER_SUBMODULE_PAYLOAD",
                "DIRTY_SUBMODULE_PAYLOAD",
                "NESTED_DEPTH_9_PAYLOAD",
            ],
        )
        .await;
    assert!(
        !fixture
            .repo
            .path
            .join(".git/modules/missing-store")
            .exists()
    );

    fixture.reject(
        "missing.yaml",
        &[
            "submodule \"absent/input\"",
            "needed store",
            ".git/modules/missing-store",
            "is unavailable",
        ],
    );
    fixture.reject("depth.yaml", &["submodule depth exceeds cap of 8"]);

    // Alternates redirect object reads to another store. Even a valid local
    // alternate inside the shared repository must be refused for a used module.
    let alternates = fixture
        .repo
        .path
        .join(".git/modules/logical-name/objects/info/alternates");
    std::fs::write(alternates, "../../../objects\n").unwrap();
    fixture.reject(
        "alternates.yaml",
        &["host object alternates are unsupported"],
    );
    assert!(
        !fixture
            .repo
            .path
            .join(".git/modules/missing-store")
            .exists()
    );
}
