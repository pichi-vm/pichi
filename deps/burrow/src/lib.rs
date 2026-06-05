//! Tiny caching URL downloader for tests.
//!
//! [`fetch`] downloads a URL once into a cache directory under the cargo
//! `target/` tree (so `cargo clean` reclaims it) and returns the local
//! path; later calls reuse it. It is keyed by URL, not content — a cache,
//! not a verifier; callers that need integrity pin the URL to an immutable
//! artifact. Concurrent callers (threads, or separate test processes) are
//! serialized per URL with an advisory file lock.
//!
//! Cache location: `$BURROW_CACHE` if set, else `<OUT_DIR>/cache` (baked at
//! build time, always under `target/`). CI can point `$BURROW_CACHE` at a
//! stable directory it restores across runs.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use fs2::FileExt;

/// Cache directory: `$BURROW_CACHE` overrides; otherwise `<OUT_DIR>/cache`.
fn cache_dir() -> PathBuf {
    match std::env::var_os("BURROW_CACHE") {
        Some(p) => PathBuf::from(p),
        // OUT_DIR is `<target>/<profile>/build/burrow-<hash>/out` — under
        // target/, so `cargo clean` removes the cache with it.
        None => Path::new(env!("OUT_DIR")).join("cache"),
    }
}

/// Stable, dependency-free cache key for a URL: 64-bit FNV-1a (hex) plus a
/// sanitized basename for human-readability.
fn url_key(url: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in url.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let base = url
        .rsplit(['/', '?', '#'])
        .find(|s| !s.is_empty())
        .unwrap_or("download");
    let base: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .take(64)
        .collect();
    format!("{hash:016x}-{base}")
}

/// Download `url` (caching under `target/`) and return the local path.
/// Re-uses the cached copy on subsequent calls.
pub fn fetch(url: &str) -> io::Result<PathBuf> {
    let dir = cache_dir();
    fs::create_dir_all(&dir)?;
    let dest = dir.join(url_key(url));

    // Hold an exclusive lock for the whole check-then-download so parallel
    // callers don't both download (the lock auto-releases if a process dies).
    let lock = File::create(dest.with_extension("lock"))?;
    lock.lock_exclusive()?;
    let result = fetch_locked(url, &dest);
    let _ = FileExt::unlock(&lock);
    result
}

fn fetch_locked(url: &str, dest: &Path) -> io::Result<PathBuf> {
    if dest.exists() {
        return Ok(dest.to_path_buf());
    }
    let tmp = dest.with_extension("tmp");
    let resp = ureq::get(url)
        .call()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("GET {url}: {e}")))?;
    let mut reader = resp.into_reader();
    let mut file = File::create(&tmp)?;
    io::copy(&mut reader, &mut file)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, dest)?; // atomic publish
    Ok(dest.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{fetch, url_key};

    #[test]
    fn key_is_stable_and_distinct() {
        let a = url_key("https://example.com/a/vmlinuz-virt");
        assert_eq!(a, url_key("https://example.com/a/vmlinuz-virt"));
        assert!(a.ends_with("-vmlinuz-virt"));
        assert_ne!(a, url_key("https://example.com/b/vmlinuz-virt"));
    }

    #[test]
    fn key_has_no_separators_and_tracks_full_url() {
        let k = url_key("https://host/path/k.bin?token=x/y/z");
        assert!(!k.contains(['/', '?', '#']));
        // The whole URL feeds the hash, so a different query → different key.
        assert_ne!(k, url_key("https://host/path/k.bin?token=other"));
    }

    /// Network smoke test (offline-skipped). Verifies a real HTTPS download
    /// and that the second call is a cache hit returning the same path.
    #[test]
    #[ignore = "requires network"]
    fn fetches_and_caches() {
        let url = "https://dl-cdn.alpinelinux.org/alpine/MIRRORS.txt";
        let a = fetch(url).expect("fetch");
        assert!(std::fs::metadata(&a).unwrap().len() > 0);
        assert_eq!(a, fetch(url).expect("cache hit"));
    }
}
