// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Cross-project acceptance against a newly built appliance. Requires KVM;
//! the absence of virtualization is a failure, never a successful skip.
#![cfg(all(
    feature = "vm-tests",
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

mod common;
#[path = "staged_images/disk.rs"]
mod disk;

use common::stages::{Builds, Image};
use std::fs;
use std::path::Path;

struct Matrix {
    builds: Builds,
    root: (u64, u64),
}

impl Matrix {
    fn assert_boot(&self, name: &str, image: &Image, markers: &[&str]) {
        let text = self
            .builds
            .boot(&format!("qualification.local/{name}:1"), name);
        for marker in markers {
            assert!(text.contains(marker), "missing {marker:?} in {name} boot");
        }
        let config = text
            .split("PICHI_BOOTCONFIG_BEGIN\r\n")
            .nth(1)
            .expect("root init printed bootconfig")
            .split("PICHI_BOOTCONFIG_END")
            .next()
            .unwrap();
        // These fixtures emit flat canonical lines. Linux displays bare keys
        // as empty strings and omits the native text's semicolons.
        let expected: std::collections::BTreeSet<_> = image
            .boot_config()
            .unwrap()
            .lines()
            .map(|line| {
                let line = line.trim_end_matches(';');
                if line.contains(" = ") {
                    line.to_owned()
                } else {
                    format!("{line} = \"\"")
                }
            })
            .collect();
        let actual: std::collections::BTreeSet<_> = config
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect();
        assert_eq!(actual, expected);
        assert!(
            text.contains("root partition selected"),
            "Corium partition trace missing"
        );
        assert!(
            text.contains(" linear "),
            "actual root device-mapper table missing"
        );
        let table = text
            .lines()
            .find(|line| line.starts_with("0 ") && line.contains(" linear "))
            .unwrap();
        let fields: Vec<_> = table.split_whitespace().collect();
        assert_eq!(fields[1].parse::<u64>().unwrap(), self.root.1);
        assert_eq!(fields[4].parse::<u64>().unwrap(), self.root.0);
        assert!(text.contains(&format!("start={}", self.root.0)));
        assert!(text.contains(&format!("sectors={}", self.root.1)));
    }
}

#[test]
fn staged_images_create_extend_and_compose() {
    let builds = Builds::new("staged-images-");
    let host = builds.directory.join("host.txt");
    fs::write(&host, "host context\n").unwrap();
    let base = builds.build(
        "base",
        include_str!("staged_images/base.yaml"),
        &[("host.txt", &host)],
    );
    assert_eq!(base.layers("scute").len(), 1);
    assert!(base.bytes("pmi").is_none());
    assert!(base.boot_config().is_none());
    let root = disk::Disk::from(&base).verify(&builds.directory.join("base-gpt.raw"));
    fs::write(
        builds.directory.join("root-sectors.json"),
        serde_json::to_vec(&root).unwrap(),
    )
    .unwrap();
    let matrix = Matrix { builds, root };
    let builds = &matrix.builds;

    let arma = builds.guest_arma();
    let corium = builds.guest_binary("corium", Path::new(common::CORIUM_BIN));
    let firmware = [("arma", arma.as_path()), ("corium", corium.as_path())];
    let bootable = builds.build(
        "bootable",
        include_str!("staged_images/firmware.yaml"),
        &firmware,
    );
    assert_eq!(
        bootable.layers("scute"),
        base.layers("scute"),
        "firmware root must not become ancestry"
    );
    assert!(
        bootable
            .boot_config()
            .unwrap()
            .contains(&format!("corium.root = \"{}\";", base.root()))
    );
    matrix.assert_boot("bootable", &bootable, &["base root"]);

    let extended = builds.build(
        "extended",
        include_str!("staged_images/extend.yaml"),
        &[("host.txt", &host)],
    );
    assert_eq!(extended.bytes("pmi"), bootable.bytes("pmi"));
    assert_eq!(
        extended.layers("scute").len(),
        base.layers("scute").len() + 2
    );
    assert_eq!(
        extended.layers("scute")[..base.layers("scute").len()],
        base.layers("scute")
    );
    assert_ne!(extended.root(), base.root());
    let config = extended.boot_config().unwrap();
    assert!(config.contains(&format!("corium.root = \"{}\";", extended.root())));
    assert!(config.contains("example.keep = \"original\";"));
    matrix.assert_boot(
        "extended",
        &extended,
        &["base root", "extended root", "local stage"],
    );

    let overlay = builds.build(
        "overlay",
        r#"
image:
  root: qualification.local/extended:1
  boot:
    inherit: qualification.local/extended:1
    extend:
      example.empty: ""
      example.bare: []
      example.list: [one, two]
      example.delete: null
"#,
        &[],
    );
    assert_eq!(overlay.layers("scute"), extended.layers("scute"));
    assert_eq!(overlay.bytes("pmi"), extended.bytes("pmi"));
    let config = overlay.boot_config().unwrap();
    assert!(config.contains(&format!("corium.root = \"{}\";", extended.root())));
    assert!(config.contains("example.empty = \"\";"));
    assert!(config.lines().any(|line| line == "example.bare;"));
    assert!(config.contains("example.list = \"one\", \"two\";"));
    assert!(!config.contains("example.delete"));
    matrix.assert_boot("overlay", &overlay, &["local stage"]);

    let replacement = builds.build(
        "replacement",
        r#"
image:
  root: qualification.local/extended:1
  boot:
    inherit: qualification.local/bootable:1
    define:
      corium.root: ""
      corium.log: debug
      kernel.ro: []
      kernel.console: hvc0
      kernel.systemd.volatile: state
      kernel.systemd.show_status: "false"
"#,
        &[],
    );
    assert_eq!(replacement.layers("scute"), extended.layers("scute"));
    assert_eq!(replacement.bytes("pmi"), extended.bytes("pmi"));
    assert!(!replacement.boot_config().unwrap().contains("example.keep"));
    assert!(
        replacement
            .boot_config()
            .unwrap()
            .contains(&format!("corium.root = \"{}\";", extended.root()))
    );
    matrix.assert_boot("replacement", &replacement, &["local stage"]);

    let replacement_corium = std::env::var_os("PICHI_TEST_REPLACEMENT_CORIUM")
        .expect("set PICHI_TEST_REPLACEMENT_CORIUM to a newly built static release Corium");
    let replacement_corium =
        builds.guest_binary("replacement-corium", Path::new(&replacement_corium));
    assert_ne!(
        fs::read(&corium).unwrap(),
        fs::read(&replacement_corium).unwrap(),
        "replacement must exercise different Corium executable bytes"
    );
    let replacement_plan = include_str!("staged_images/firmware.yaml").replace(
        "qualification.local/base:1",
        "qualification.local/extended:1",
    );
    let new_firmware = builds.build(
        "new-firmware",
        &replacement_plan,
        &[("arma", &arma), ("corium", &replacement_corium)],
    );
    assert_eq!(new_firmware.layers("scute"), extended.layers("scute"));
    assert_ne!(new_firmware.bytes("pmi"), extended.bytes("pmi"));
    assert_eq!(new_firmware.boot_config(), extended.boot_config());
    matrix.assert_boot("new-firmware", &new_firmware, &["local stage"]);

    for (name, yaml, counts) in [
        (
            "root-only",
            "image: {root: qualification.local/base:1}\n",
            (1, 0, 0),
        ),
        (
            "boot-only",
            "image: {boot: {inherit: qualification.local/bootable:1, define: {}}}\n",
            (0, 1, 0),
        ),
        (
            "root-boot",
            "image: {root: qualification.local/base:1, boot: {inherit: qualification.local/bootable:1, define: {}}}\n",
            (1, 1, 0),
        ),
        (
            "boot-config",
            "image: {boot: qualification.local/bootable:1}\n",
            (0, 1, 1),
        ),
        (
            "shortcut",
            "image: qualification.local/bootable:1\n",
            (1, 1, 1),
        ),
    ] {
        let output = builds.build(name, yaml, &[]);
        assert_eq!(
            (
                output.layers("scute").len(),
                output.layers("pmi").len(),
                output.layers("bootconfig").len()
            ),
            counts,
            "{name}"
        );
        if counts.1 != 0 {
            assert_eq!(output.bytes("pmi"), bootable.bytes("pmi"));
        }
        if counts.0 != 0 {
            assert_eq!(output.layers("scute"), base.layers("scute"));
        }
    }

    let joined = builds.build(
        "joined",
        r#"
image:
  root: qualification.local/root-only:1
  boot:
    inherit: qualification.local/boot-only:1
    define: {corium.root: ""}
"#,
        &[],
    );
    assert_eq!(joined.layers("scute"), base.layers("scute"));
    assert_eq!(joined.bytes("pmi"), bootable.bytes("pmi"));
    assert!(
        joined
            .boot_config()
            .unwrap()
            .contains(&format!("corium.root = \"{}\";", base.root()))
    );

    let deleted = builds.build(
        "deleted-root",
        r#"
stages:
  changed:
    extend: qualification.local/overlay:1
    steps: [exec: {command: "printf changed > /changed"}]
image:
  root: changed
  boot:
    inherit: changed
    extend: {corium.root: null, example.list: [replacement]}
"#,
        &[],
    );
    assert_ne!(deleted.root(), base.root());
    assert!(!deleted.boot_config().unwrap().contains("corium.root"));
    assert!(
        deleted
            .boot_config()
            .unwrap()
            .contains("example.list = \"replacement\";")
    );

    let unchanged = builds.build(
        "unchanged-root",
        r#"
image:
  root: qualification.local/bootable:1
  boot:
    inherit: qualification.local/bootable:1
    extend: {corium.root: deliberately-unchanged}
"#,
        &[],
    );
    assert_eq!(unchanged.layers("scute"), base.layers("scute"));
    assert!(
        unchanged
            .boot_config()
            .unwrap()
            .contains("corium.root = \"deliberately-unchanged\";")
    );

    builds.build(
        "unchanged-parent",
        r#"
stages:
  checked:
    extend: qualification.local/bootable:1
    mounts: {/boot/efi: esp}
    steps:
      - exec:
          command: |
            test "$(cat /boot/efi/marker)" = 'created ESP'
            test "$(cat /root-marker)" = 'base root'
            test ! -e /etc/from-sibling
image: checked
"#,
        &[],
    );

    let hidden = builds.plan(
        "hidden-copy",
        r#"
stages:
  source:
    extend: qualification.local/base:1
    mounts: {/tmp: tmpfs}
    steps: [exec: {command: "printf hidden > /tmp/transient"}]
  target:
    extend: qualification.local/base:1
    steps: [copy: {from: source, path: /tmp/transient, into: /hidden}]
image: target
"#,
        &[],
    );
    assert!(!builds.execute(&hidden, "qualification.local/hidden-copy:1"));
    assert!(
        !common::pichi(
            &builds.cache,
            &[],
            &["inspect", "qualification.local/hidden-copy:1"]
        )
        .status
        .success()
    );

    for (name, command) in [("failed-command", "exit 23"), ("timeout", "sleep 30")] {
        let yaml = format!(
            "stages:\n  failed:\n    extend: qualification.local/base:1\n    steps: [{{exec: {{command: '{command}', timeout: 1s}}}}]\nimage: failed\n"
        );
        let repo = builds.plan(name, &yaml, &[]);
        let tag = format!("qualification.local/{name}:1");
        assert!(!builds.execute(&repo, &tag));
        assert!(
            !common::pichi(&builds.cache, &[], &["inspect", &tag])
                .status
                .success()
        );
    }

    for (name, yaml) in [
        ("unknown-stage", "image: missing\n"),
        (
            "extension-format",
            "stages: {a: {extend: qualification.local/tools:1, format: {}}}\nimage: a\n",
        ),
        (
            "cycle",
            "stages: {a: {extend: b, steps: [{exec: {command: ':'}}]}, b: {extend: a, steps: [{exec: {command: ':'}}]}}\nimage: a\n",
        ),
        (
            "config-without-pmi",
            "image: {bootconfig: {values: {example: value}}}\n",
        ),
        ("empty-image", "image: {}\n"),
    ] {
        let repo = builds.capture(name, yaml, &[]);
        let output = common::pichi(
            &builds.cache,
            &[],
            &["update", repo.to_str().unwrap(), "--pull=never"],
        );
        fs::write(
            repo.join("update.log"),
            [output.stdout.as_slice(), output.stderr.as_slice()].concat(),
        )
        .unwrap();
        assert!(!output.status.success(), "invalid {name} was accepted");
        assert!(
            !repo.join("pichi.lock").exists(),
            "invalid {name} published a lock"
        );
    }

    fs::write(builds.directory.join("PASS"), "Fresh Fedora root, generated PMI, real root init, extensions, overlays, component selections, and rejected derives passed.\n").unwrap();
}
