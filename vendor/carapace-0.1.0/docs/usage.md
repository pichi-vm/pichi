# carapace

Carapace composes cryptographically linked block-device scutes into one
integrity-protected, read-only device. Each **scute** contains a
persistent dm-snapshot exception store and a dm-verity hash volume. The top
scute's independently trusted root binds the entire chain.

Discovery and assembly require Linux. Optional image conversion supports Linux,
macOS, and Windows. The format is specified in [spec.md](spec.md).

## Library API

Discovery returns a `Chain`: an owned root and a list of `Scute { data, hash }`
open files, ordered base to top. Callers can construct scutes directly with
`Scute { data: File::open(data_path)?, hash: File::open(hash_path)? }`.
Discovery retains the files it inspected; assembly independently rereads and
validates their headers with positional I/O, preserving all input offsets.

`Chain` and `Scute` implement `Debug`; duplicating their file handles is explicit.
They retain two file descriptors per scute until dropped, which can prolong
backing-device lifetimes. Open handles preserve input identity across pathname
replacement; they do not make the contents immutable. Callers needing shared
ownership can use `Arc<Chain>`; explicit `File::try_clone()` duplicates a handle
but shares its file offset. Assembly returns an open block-device file.

```rust,no_run
use carapace::{Assemble, Chain, DevId, FromGpt};

fn mount(root: &[u8], directory: &std::path::Path) -> std::io::Result<()> {
    let chain = Chain::from_gpt(root)?;
    let device: std::fs::File = chain.assemble()?;
    let id = DevId::try_from(device.metadata()?)?;
    let status = std::process::Command::new("mount")
        .args(["-o", "ro"])
        .arg(format!("/dev/dm-{}", id.minor()))
        .arg(directory)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other("mount failed"));
    }
    // The mount now retains the stack; closing this file is safe.
    drop(device);
    Ok(())
}
```

Assembly returns a read-only `std::fs::File` at offset zero, with deferred
removal already scheduled. The file, its clones, mounts, and upper block-device
mappings can retain the device. Closing the last holder permits the kernel to
tear down the stack. Dropping a file does not forcibly unmount anything.

The file supports ordinary reads, seeks, metadata, and FD-based consumers.
`DevId::try_from(device.metadata()?)` obtains its device number without closing
it. Import `Assemble` to call the synchronous `assemble` trait method.

The library imposes no maximum number of scutes. Discovery still rejects
cycles, and assembly requires at least one scute. Resource limits are the
caller’s policy; operating-system resource constraints still apply.

## Async API

With `gpt` and `tokio`, `AsyncFromGpt` provides discovery through Tokio's
blocking pool. Assembly is synchronous; the caller chooses how to schedule it:

```rust,no_run
use carapace::{Assemble, AsyncFromGpt, Chain};

async fn attach(root: &[u8]) -> std::io::Result<std::fs::File> {
    let chain = Chain::from_gpt(root).await?;
    tokio::task::spawn_blocking(move || chain.assemble())
        .await
        .map_err(std::io::Error::other)?
}
```

The closure owns the chain and returns the same device file without reopening
it. Callers wanting Tokio file I/O can wrap it with `tokio::fs::File::from_std`
using Tokio's `fs` feature.

Dropping a Tokio blocking task's join handle does not stop work that has
started; runtime shutdown may wait for it to finish.

Async discovery requires a Tokio runtime. Work starts when its future is
polled and copies the root into the worker. Dropping the future does not
interrupt a running worker. IO errors retain their kind, while runtime and
worker failures are reported as contextual `std::io::Error`s. If both discovery
traits are imported, select `<Chain as AsyncFromGpt>::from_gpt(root)` explicitly.

## Writable snapshots

An assembled file can be the read-only origin of a writable snapshot, using
`devmap-linux` and `devmap-snapshot` directly. Writes go to a separate writable
COW block device. For example, given an assembled `origin`, prepared `cow`
block-device file, and a sector count no larger than the origin:

```rust,no_run
use devmap_core::{BlockSize, Span};
use devmap_linux::targets::Target;
use devmap_linux::{Control, DevId, Table};
use devmap_snapshot::{
    Discard, Parameters, Persistence, Persistent, Snapshot,
};
use std::{
    fs::{File, OpenOptions},
    io,
};

fn snapshot(origin: &File, cow: &File, name: &str, sectors: u64) -> io::Result<File> {
    let target = Snapshot::from(Parameters {
        origin: DevId::try_from(origin.metadata()?)?,
        cow: DevId::try_from(cow.metadata()?)?,
        persistence: Persistence::Persistent(Persistent::Standard),
        chunk_size: BlockSize::default(), // 4 KiB / 8 sectors
        discard: Discard::Disabled,
    });
    let control = Control::open()?;
    let id = control.create(name)?;
    // Immediate-remove on failure until deferred removal is armed.
    struct Cleanup<'a>(&'a Control, Option<DevId>);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            if let Some(id) = self.1.take() {
                let _ = self.0.remove(id, false);
            }
        }
    }
    let mut cleanup = Cleanup(&control, Some(id));
    let version = control.version(Snapshot::NAME)?;
    let table = Table::<4096>::default().add(Span::new(0, sectors), &target, version)?;
    control.load(id, table, false)?;
    control.suspend(id, false)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/dev/dm-{}", id.minor()))?;
    // Schedule deferred removal; the open file keeps the mapping alive.
    control.remove(id, true)?;
    cleanup.1 = None;
    Ok(file)
}
```

Keep the input files alive through setup. The returned writable file retains
the snapshot, which in turn retains the origin and COW device. A regular-file
COW needs loop attachment first; lodown provides that separately.

## Image conversion

Enable `image` to convert a raw image into a base-scute COW stream:

```rust,no_run
use carapace::image::{Plan, Planned};
use std::{fs::File, io};

let mut scute = Planned::<File>::plan(File::open("disk.raw")?)?;
let size = scute.size(); // Exact output length, including padding.
io::copy(&mut scute, &mut File::create("base.cow")?)?;
# Ok::<(), io::Error>(())
```

The source must be a nonempty regular file of at most 64 GiB. Keep its contents
unchanged and do not use duplicated handles during planning or reading.
Planning scans for nonzero chunks, skipping sparse holes where supported;
streaming rereads the selected chunks. With `tokio`, use `AsyncPlan` with a
`tokio::fs::File` and consume the result through `AsyncRead`.

## Features and dependencies

| Feature | Default | Purpose |
|---|---|---|
| `gpt` | On | `FromGpt` discovery through kernel-visible PARTUUIDs; CLI mounting |
| `loop` | Off | Read-only, autoclearing loop attachment of regular-file scutes |
| `image` | Off | Portable image-to-base-scute conversion |
| `tokio` | Off | Async discovery with `gpt`; async conversion with `image` |

The default build needs no async runtime. Rust 1.89 or newer is required.
`devmap-linux` handles device control (and exports `DevId`),
`devmap-core` shared geometry types (spans, block sizes), `devmap-snapshot` the
snapshot target, `devmap-verity` the standard header/target format, and optional
lodown loop attachment.

Validation failures use `InvalidData`; invalid caller inputs use `InvalidInput`.
Regular-file scutes without `loop` return `Unsupported`. The shared verity
header decoder rejects noncanonical nonzero padding, which older Carapace
versions accepted. Carapace additionally enforces its own parameter whitelist
and chain rules, including for directly constructed chains.

Assemblers coordinate publication with an advisory lock on
`/dev/mapper/control`. Mappings are reused only when their active, read-only
tables match. Names include root prefixes and backing device IDs; independent
loop attachments of identical files may create separate mappings. Existing
active tables are never reloaded during reuse.

## CLI

```sh
carapace mount <root-hex> <dir> [--type <fs>]
umount <dir>
```

The trusted root must match the chain's digest size: 32 bytes for SHA-256 or
64 bytes for SHA-512. For GPT discovery, expose the outer image's partitions
first with `losetup --partscan`. Discovery reads `/sys/class/block/*/uevent`;
it does not require udev. Mounting requires device-mapper privileges and
`mount(8)`.

## Building and testing

See [CONTRIBUTING.md](../CONTRIBUTING.md) for prerequisites, checks,
integration tests, and benchmarks.

## License

Apache-2.0. See [LICENSE](../LICENSE).
