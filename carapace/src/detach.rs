//! `detach` — best-effort tear down of a previously-attached chain.

use crate::dm::{list_devices_with_prefix, remove_by_name};
use crate::name::validate_dm_name;
use crate::CarapaceError;

/// Linux errno 6 — "No such device or address." `dm-ioctl` returns
/// this (not `ENOENT`) for `DM_DEV_REMOVE` against a missing name; see
/// `drivers/md/dm-ioctl.c`. `std::io::ErrorKind` has no variant for
/// it, so it lands in `Uncategorized` and we have to compare the raw
/// errno. The numeric value is part of the Linux kernel ABI and has
/// been stable since 2.0; carapace is Linux-only.
const ENXIO: i32 = 6;

/// Best-effort teardown of every dm device prefixed `<name>`, in kernel-safe
/// order. Returns the residual per-device problems as operator-facing strings
/// (empty == clean teardown). `Err` only for a hard failure *before* teardown
/// (the device enumeration); individual device-removal failures are collected,
/// not propagated — detach is best-effort by design, like `dmsetup remove -f`.
///
/// `name` is validated here; see [`validate_dm_name`].
pub fn detach(name: &str) -> Result<Vec<String>, CarapaceError> {
    validate_dm_name(name)?;

    // Discovery: enumerate dm devices with our `<name>` prefix via
    // DM_LIST_DEVICES — the kernel's authoritative inventory. Loops are NOT
    // touched — carapace attach no longer creates them; the caller (operator,
    // initrd, or test harness) owns any losetup lifecycle.
    let devices = list_devices_with_prefix(name)?;
    let ordered = sort_for_teardown(name, devices);

    let mut errors: Vec<String> = Vec::new();
    for dev in &ordered {
        let _ = remove_by_name_tolerant(dev, &mut errors);
    }
    Ok(errors)
}

/// Sort the carapace dm devices into kernel-safe teardown order, and
/// **drop any device whose name doesn't match a known carapace shape**.
///
/// Layout produced by attach (see src/assemble.rs):
///   `<base>`        — top alias (dm-linear over top snapshot)
///   `<base>-sN`     — per-scute snapshot
///   `<base>-vN`     — per-scute dm-verity
///   `<base>-z0`     — base dm-zero
///
/// Dependencies: `<base>` holds open `-s{top}`; `-sN` holds open
/// `-vN` (its CoW) and either `-s{N-1}` (its origin) or `-z0` for
/// scute 0. Removal must reverse-build:
///   1. top alias `<base>`
///   2. for i = max..0: `-sI` then `-vI`
///   3. `-z0`
///
/// `list_devices_with_prefix` already enforces the `base` / `base-…`
/// boundary, but a same-prefix device with an unknown suffix
/// (e.g. `<base>-foo` from a future spec extension or a manual
/// `dmsetup` poke) is excluded here as defense-in-depth — we never
/// remove a device whose role we don't recognize.
fn sort_for_teardown(base: &str, names: Vec<String>) -> Vec<String> {
    /// Sort key:
    ///   (0, 0, 0)         — top alias `<base>`
    ///   (1, !N, 0)        — `<base>-sN`  (higher N first via inversion)
    ///   (1, !N, 1)        — `<base>-vN`  (paired with same scute)
    ///   (2, 0, 0)         — `<base>-z0`
    ///
    /// Returns `None` for unrecognized shapes (caller drops them).
    fn rank(base: &str, name: &str) -> Option<(u8, u32, u8)> {
        if name == base {
            return Some((0, 0, 0));
        }
        let suffix = name.strip_prefix(base).and_then(|s| s.strip_prefix('-'))?;
        if let Some(idx_str) = suffix.strip_prefix('s') {
            if let Ok(i) = idx_str.parse::<u32>() {
                return Some((1, !i, 0));
            }
        }
        if let Some(idx_str) = suffix.strip_prefix('v') {
            if let Ok(i) = idx_str.parse::<u32>() {
                return Some((1, !i, 1));
            }
        }
        if suffix == "z0" {
            return Some((2, 0, 0));
        }
        None
    }

    let mut keyed: Vec<(_, String)> = names
        .into_iter()
        .filter_map(|n| rank(base, &n).map(|k| (k, n)))
        .collect();
    keyed.sort_by_key(|(k, _)| *k);
    keyed.into_iter().map(|(_, n)| n).collect()
}

fn remove_by_name_tolerant(name: &str, errors: &mut Vec<String>) -> bool {
    match remove_by_name(name) {
        Ok(()) => true,
        Err(CarapaceError::DmIoctl { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound
                || source.raw_os_error() == Some(ENXIO) =>
        {
            // Device disappeared between enumeration and remove — fine.
            // (Concurrent detach by another process; or kernel auto-
            // cleanup raced our remove.) Also covers the case where
            // a foreign-prefix device was removed externally.
            false
        }
        Err(e) => {
            errors.push(format!("{name}: {e}"));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sort_for_teardown;

    #[test]
    fn teardown_order_is_alias_then_top_to_base_then_zero() {
        let base = "carapace-test";
        // Mixed input order — the function must sort regardless.
        let names = vec![
            format!("{base}-z0"),
            format!("{base}-v0"),
            format!("{base}-s2"),
            format!("{base}"),
            format!("{base}-v2"),
            format!("{base}-s0"),
            format!("{base}-v1"),
            format!("{base}-s1"),
        ];
        let ordered = sort_for_teardown(base, names);
        assert_eq!(
            ordered,
            vec![
                format!("{base}"),
                format!("{base}-s2"),
                format!("{base}-v2"),
                format!("{base}-s1"),
                format!("{base}-v1"),
                format!("{base}-s0"),
                format!("{base}-v0"),
                format!("{base}-z0"),
            ]
        );
    }

    #[test]
    fn teardown_drops_unknown_suffixes() {
        // Defense in depth: even if list_devices_with_prefix were to
        // misbehave and pass us `<base>-foo` (unrecognized suffix) or
        // `unrelated` (different stack entirely), sort_for_teardown
        // must NOT include them in the removal queue. We never want
        // to DM_DEV_REMOVE a device whose role we don't recognize.
        let base = "x";
        let names = vec![
            format!("{base}-foo"),
            format!("{base}"),
            format!("{base}-z0"),
            "unrelated".to_string(),
        ];
        let ordered = sort_for_teardown(base, names);
        assert_eq!(ordered, vec![format!("{base}"), format!("{base}-z0")]);
    }

    #[test]
    fn teardown_handles_empty_input() {
        assert!(sort_for_teardown("anything", vec![]).is_empty());
    }
}
