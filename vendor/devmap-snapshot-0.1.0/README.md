# devmap-snapshot

Typed Linux device-mapper targets for copy-on-write snapshots:

- `Origin`: preserves snapshot contents when the origin is written.
- `Snapshot`: describes a snapshot backed by a copy-on-write device.
- `Merge`: describes a persistent snapshot's merge back into its origin.

These implement `devmap_linux::targets::Target`. Use `devmap_linux::Control` to query
target versions, load tables, and manage mappings. Parse table and runtime
responses through `Row::parse`; snapshot runtime state is represented by `Info`.

`Header` and `Exception` encode and decode persistent snapshot metadata on disk.
These codecs and `Constraint` are available on all platforms.
They operate on individual records; applications handle storage I/O and validate
the surrounding store.

Target descriptions and device control are Linux-only. Applications manage device lifetimes and the
[kernel's snapshot handover procedure](https://docs.kernel.org/admin-guide/device-mapper/snapshot.html).
Constructing these Rust values does not create or change a mapping.

## Construct a snapshot table

Build a table from an existing origin and a dedicated copy-on-write store.
This example queries the target version but does not create or activate a mapping.

```no_run
# #[cfg(target_os = "linux")]
# mod example {
use std::io;
use devmap_core::{BlockSize, Span};
use devmap_linux::{Control, DevId, Table, targets::Target};
use devmap_snapshot::{Discard, Parameters, Persistence, Persistent, Snapshot};

fn snapshot_table(
    control: &Control,
    origin: DevId,
    cow: DevId,
    sectors: u64,
) -> io::Result<Table> {
    let snapshot = Snapshot::from(Parameters {
        origin,
        cow,
        persistence: Persistence::Persistent(Persistent::Standard),
        chunk_size: BlockSize::default(), // 4 KiB.
        discard: Discard::Disabled,
    });
    Table::default().add(
        Span::new(0, sectors),
        &snapshot,
        control.version(Snapshot::NAME)?,
    )
}
# }
```

The `cow` store must be disposable storage reserved for snapshot metadata and
changed chunks. Before using a snapshot, coordinate origin writes through a
`Origin` mapping and the required suspend/resume sequence; constructing
this table alone does not make a live origin safe to snapshot.
