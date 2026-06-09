use std::fs;
use std::path::{Path, PathBuf};

const DISALLOWED_PATTERNS: &[&str] = &[
    "cfg(target_os",
    "cfg(target_arch",
    "cfg(target_env",
    "cfg(target_family",
    "cfg_attr(target_os",
    "cfg_attr(target_arch",
    "cfg_attr(target_env",
    "cfg_attr(target_family",
];

#[test]
fn target_cfg_is_confined_to_machine_selection() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let scanner = SourceScan::new(manifest);
    let mut failures = Vec::new();

    scanner.scan_dillo_sources(&mut failures);
    scanner.scan_portable_core_crates(&mut failures);

    assert!(
        failures.is_empty(),
        "target cfg must stay out of portable code; found {}",
        failures.join(", ")
    );
}

struct SourceScan {
    manifest: PathBuf,
}

impl SourceScan {
    fn new(manifest: PathBuf) -> Self {
        Self { manifest }
    }

    fn scan_dillo_sources(&self, failures: &mut Vec<String>) {
        let src = self.manifest.join("src");
        self.visit_rust_files(&src, &mut |path| {
            if path
                .file_name()
                .is_some_and(|name| name == "machine_select.rs")
            {
                return;
            }
            self.scan_file(&src, path, failures);
        });
    }

    fn scan_portable_core_crates(&self, failures: &mut Vec<String>) {
        for crate_dir in [
            "deps/dillo-mmio",
            "deps/dillo-mmio-uart",
            "deps/dillo-mmio-virtio",
            "deps/dillo-pci",
            "deps/dillo-pci-virtio",
            "deps/dillo-virtio-console",
            "deps/virtio",
        ] {
            let root = self.manifest.join(crate_dir);
            self.visit_rust_files(&root.join("src"), &mut |path| {
                self.scan_file(&root, path, failures);
            });
        }
    }

    fn scan_file(&self, root: &Path, path: &Path, failures: &mut Vec<String>) {
        let text = fs::read_to_string(path).expect("read source file");
        for (line_idx, line) in text.lines().enumerate() {
            if DISALLOWED_PATTERNS
                .iter()
                .any(|pattern| line.contains(pattern))
            {
                let rel = path.strip_prefix(root).unwrap_or(path);
                failures.push(format!("{}:{}", rel.display(), line_idx + 1));
            }
        }
    }

    fn visit_rust_files(&self, dir: &Path, f: &mut impl FnMut(&Path)) {
        let _ = &self.manifest;
        for entry in fs::read_dir(dir).expect("read source directory") {
            let entry = entry.expect("read directory entry");
            let path = entry.path();
            if path.is_dir() {
                self.visit_rust_files(&path, f);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                f(&path);
            }
        }
    }
}
