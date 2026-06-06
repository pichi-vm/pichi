//! Memory placement: KVM memslots and `/memory@*` DTBO regions.
//!
//! `--memory N` is the operator's **total** RAM budget. The algorithm:
//!
//! 1. Each must-cover section is rounded outward to 2 MiB and merged
//!    with overlapping/adjacent siblings → "islands" (the minimum RAM
//!    we must back to honor the PMI's section GPAs).
//! 2. The remaining budget (= total − sum(islands)) is allocated as
//!    one contiguous "big chunk" at the lowest 2 MiB-aligned GPA that
//!    avoids every island and every device region.
//! 3. The final region set = islands ∪ {big_chunk}; both KVM memslots
//!    and DTBO `/memory@N` use this exact set.
//!
//! Invariant: `MemTotal == --memory` (modulo 2 MiB rounding of the
//! must-cover sections themselves).

#[cfg(not(target_os = "macos"))]
use dillo_platform::Platform;
use thiserror::Error;

const HUGE_PAGE: u64 = 2 << 20;

/// One contiguous region for either a memslot or a `/memory@N` node.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Region {
    pub gpa: u64,
    pub size: u64,
}

#[derive(Debug, Clone, Copy)]
struct Interval {
    start: u64,
    end: u64, // exclusive
}

/// The full memory plan: memslots == memory_nodes by construction.
#[derive(Debug)]
pub(crate) struct MemoryPlan {
    pub memslots: Vec<Region>,
    pub memory_nodes: Vec<Region>,
}

#[derive(Debug, Error)]
pub(crate) enum PlanError {
    #[error(
        "--memory {requested_mib} MiB is less than the {islands_mib} MiB required to cover \
         loaded sections + reset trampoline"
    )]
    TooLittleMemory {
        requested_mib: u64,
        islands_mib: u64,
    },
    #[error(
        "no contiguous {remaining_mib} MiB chunk fits below the 4 GiB identity-map ceiling \
         after carving out islands and device regions"
    )]
    NoSpaceForBigChunk { remaining_mib: u64 },
}

/// Identity-map ceiling on x86: 4 GiB. Big chunk must end at or below this
/// (so its GPAs are reachable from tatu's 4 GiB identity pgtable).
const IDENTITY_CEILING: u64 = 1u64 << 32;

#[cfg(not(target_os = "macos"))]
pub(crate) fn plan(
    must_cover: &[(u64, u64)],
    memory_mib: u32,
    platform: &Platform,
) -> Result<MemoryPlan, PlanError> {
    plan_around_regions(
        must_cover,
        memory_mib,
        platform.device_regions.iter().copied(),
    )
}

pub(crate) fn plan_around_regions<I>(
    must_cover: &[(u64, u64)],
    memory_mib: u32,
    device_regions: I,
) -> Result<MemoryPlan, PlanError>
where
    I: IntoIterator<Item = (u64, u64)>,
{
    let budget = round_up_2mib(u64::from(memory_mib) * (1 << 20));

    // ── 1. Islands ────────────────────────────────────────────────
    let mut islands: Vec<Interval> = must_cover
        .iter()
        .filter(|(_, s)| *s > 0)
        .map(|&(gpa, size)| Interval {
            start: gpa & !(HUGE_PAGE - 1),
            end: round_up_2mib(gpa.saturating_add(size)),
        })
        .collect();
    merge_intervals(&mut islands);

    let islands_total: u64 = islands.iter().map(|i| i.end - i.start).sum();

    log::info!(
        "placement: budget={} MiB, islands={} ({} MiB)",
        budget >> 20,
        islands.len(),
        islands_total >> 20,
    );
    for i in &islands {
        log::info!(
            "  island [{:#x}..{:#x}) ({} MiB)",
            i.start,
            i.end,
            (i.end - i.start) >> 20,
        );
    }

    if islands_total > budget {
        return Err(PlanError::TooLittleMemory {
            requested_mib: budget >> 20,
            islands_mib: islands_total >> 20,
        });
    }
    let remaining = budget - islands_total;

    // ── 2. Big chunk ─────────────────────────────────────────────
    let mut holes: Vec<Interval> = device_holes(device_regions);
    holes.extend(islands.iter().copied());
    merge_intervals(&mut holes);

    log::info!("placement: device+island holes ({} ranges):", holes.len());
    for h in &holes {
        log::info!("  hole [{:#x}..{:#x})", h.start, h.end);
    }

    let big_chunk = if remaining == 0 {
        None
    } else {
        Some(find_lowest_fit(remaining, &holes, IDENTITY_CEILING).ok_or(
            PlanError::NoSpaceForBigChunk {
                remaining_mib: remaining >> 20,
            },
        )?)
    };

    if let Some(c) = big_chunk {
        log::info!(
            "placement: big_chunk [{:#x}..{:#x}) ({} MiB)",
            c.start,
            c.end,
            (c.end - c.start) >> 20,
        );
    }

    // ── 3. Assemble final region set ─────────────────────────────
    let mut regions: Vec<Region> = islands.iter().map(to_region).collect();
    if let Some(c) = big_chunk {
        regions.push(to_region(&c));
    }
    regions.sort_by_key(|r| r.gpa);

    Ok(MemoryPlan {
        memslots: regions.clone(),
        memory_nodes: regions,
    })
}

/// Device MMIO regions, rounded outward to 2 MiB.
fn device_holes<I>(regions: I) -> Vec<Interval>
where
    I: IntoIterator<Item = (u64, u64)>,
{
    regions
        .into_iter()
        .filter(|(_, size)| *size > 0)
        .map(|(base, size)| Interval {
            start: base & !(HUGE_PAGE - 1),
            end: round_up_2mib(base.saturating_add(size)),
        })
        .collect()
}

fn to_region(i: &Interval) -> Region {
    Region {
        gpa: i.start,
        size: i.end - i.start,
    }
}

/// Merge overlapping or touching intervals in-place. Result is sorted.
fn merge_intervals(v: &mut Vec<Interval>) {
    if v.is_empty() {
        return;
    }
    v.sort_by_key(|i| i.start);
    let mut out: Vec<Interval> = Vec::with_capacity(v.len());
    out.push(v[0]);
    for cur in v.iter().skip(1) {
        let last = out.last_mut().expect("non-empty");
        if cur.start <= last.end {
            last.end = last.end.max(cur.end);
        } else {
            out.push(*cur);
        }
    }
    *v = out;
}

/// Find the lowest 2 MiB-aligned start such that `[start, start + size)`
/// is clear of every hole and ends at or below `ceiling`.
fn find_lowest_fit(size: u64, holes: &[Interval], ceiling: u64) -> Option<Interval> {
    let mut cursor: u64 = 0;
    for h in holes {
        let candidate_end = cursor.saturating_add(size);
        if candidate_end <= h.start && candidate_end <= ceiling {
            return Some(Interval {
                start: cursor,
                end: candidate_end,
            });
        }
        cursor = cursor.max(h.end);
        cursor = round_up_2mib(cursor);
    }
    let candidate_end = cursor.saturating_add(size);
    if candidate_end <= ceiling {
        Some(Interval {
            start: cursor,
            end: candidate_end,
        })
    } else {
        None
    }
}

fn round_up_2mib(n: u64) -> u64 {
    (n + HUGE_PAGE - 1) & !(HUGE_PAGE - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_intervals_basic() {
        let mut v = vec![
            Interval { start: 10, end: 20 },
            Interval { start: 30, end: 40 },
            Interval { start: 15, end: 32 },
        ];
        merge_intervals(&mut v);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].start, 10);
        assert_eq!(v[0].end, 40);
    }

    #[test]
    fn find_lowest_fit_skips_holes() {
        let holes = vec![
            Interval {
                start: 0x10_0000,
                end: 0x20_0000,
            },
            Interval {
                start: 0x40_0000,
                end: 0x60_0000,
            },
        ];
        let r = find_lowest_fit(0x10_0000, &holes, u64::MAX).unwrap();
        assert_eq!(r.start, 0);
        assert_eq!(r.end, 0x10_0000);

        let r = find_lowest_fit(0x20_0000, &holes, u64::MAX).unwrap();
        assert_eq!(r.start, 0x20_0000);
        assert_eq!(r.end, 0x40_0000);

        let r = find_lowest_fit(0x40_0000, &holes, u64::MAX).unwrap();
        assert_eq!(r.start, 0x60_0000);
    }

    #[test]
    fn find_lowest_fit_respects_ceiling() {
        let holes = vec![Interval {
            start: 0x10_0000,
            end: 0x20_0000,
        }];
        // Need 0x40_0000, ceiling 0x50_0000, hole at [1M, 2M)
        // → candidate at 0 (size 4M) ends at 4M, ≤ ceiling? No — overlaps hole.
        // → candidate at 2M ends at 6M > 5M → rejected.
        assert!(find_lowest_fit(0x40_0000, &holes, 0x50_0000).is_none());
    }

    #[test]
    fn plan_around_regions_uses_declared_holes() {
        let plan =
            plan_around_regions(&[(0, 0x20_0000)], 8, [(0x20_0000, 0x20_0000)]).expect("placement");

        assert_eq!(plan.memslots.len(), 2);
        assert_eq!(plan.memslots[0].gpa, 0);
        assert_eq!(plan.memslots[0].size, 0x20_0000);
        assert_eq!(plan.memslots[1].gpa, 0x40_0000);
        assert_eq!(plan.memslots[1].size, 0x60_0000);
    }
}
