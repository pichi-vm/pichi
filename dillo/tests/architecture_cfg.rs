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
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut failures = Vec::new();

    visit_rust_files(&src, &mut |path| {
        if path
            .file_name()
            .is_some_and(|name| name == "machine_select.rs")
        {
            return;
        }
        let text = fs::read_to_string(path).expect("read source file");
        for (line_idx, line) in text.lines().enumerate() {
            if DISALLOWED_PATTERNS
                .iter()
                .any(|pattern| line.contains(pattern))
            {
                let rel = path.strip_prefix(&src).unwrap_or(path);
                failures.push(format!("{}:{}", rel.display(), line_idx + 1));
            }
        }
    });

    assert!(
        failures.is_empty(),
        "target cfg must stay in src/machine_select.rs; found {}",
        failures.join(", ")
    );
}

fn visit_rust_files(dir: &Path, f: &mut impl FnMut(&Path)) {
    for entry in fs::read_dir(dir).expect("read source directory") {
        let entry = entry.expect("read directory entry");
        let path = entry.path();
        if path.is_dir() {
            visit_rust_files(&path, f);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            f(&path);
        }
    }
}
