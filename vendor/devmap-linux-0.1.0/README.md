# devmap-linux

A Rust interface for creating and inspecting Linux device-mapper devices.
Device-mapper builds virtual block devices from tables of *targets*, each
describing how a range of sectors handles I/O. Use this crate to manage
those mappings directly from your application.

The crate provides typed control operations and the linear, error, and zero
targets. Companion crates `devmap-snapshot` and `devmap-verity` provide
snapshot and verity targets; applications can also implement their own
targets.

## List existing mappings

This example prints device names and major:minor identifiers without
changing any mappings.

```rust,no_run
use devmap_linux::{Control, records::Records};

fn main() -> std::io::Result<()> {
    let control = Control::open()?;
    let mut devices = control.devices::<4096>()?;

    while let Some(device) = devices.next() {
        let (name, id) = device?;
        println!("{name}: {id}");
    }

    Ok(())
}
```

## Operating requirements

Device-mapper operations require Linux and access to `/dev/mapper/control`,
normally with `CAP_SYS_ADMIN`. Operations are synchronous. Applications
manage device lifetimes explicitly: dropping a Rust value does not remove
a mapping, and failed setup can leave a device requiring cleanup.
