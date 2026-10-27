# devmap-core

`devmap-core` provides spans, block sizes, and geometry shared by the other
devmap crates. Use it when an on-disk format needs to describe an extent.

A span is a starting offset and length whose unit is established by its caller.
A block size is a nonzero power of two in bytes. Geometry pairs that size with
a nonzero number of blocks. For example, an 8 MiB extent with 4 KiB blocks has
2,048 blocks.

## Detecting a whole-file extent

If a format covers an entire file, detection can supply its geometry:

```rust,no_run
use std::fs::File;
use devmap_core::{Detect as _, General, Geometry};

# fn main() -> std::io::Result<()> {
let data = File::open("data.img")?;
let geometry = Geometry::<General>::detect(&data)?;
println!("{} blocks", geometry.count);
# Ok(())
# }
```

Detection uses the file's complete length, not its current cursor position.
For a regular file, the block size comes from the chosen constraint's default;
it is not a property discovered from the filesystem. The file must contain a
nonzero whole number of those blocks. On Linux, a block device supplies its
logical block size and byte extent instead.

Detection is synchronous even if the file will later be used for asynchronous
I/O. If the format covers only part of the storage, or requires a different
block size, construct the geometry explicitly instead of detecting the whole
extent.
