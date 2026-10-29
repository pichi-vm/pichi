# devmap-verity

`devmap-verity` prepares and describes read-only dm-verity volumes. Dm-verity
protects a *data volume* by checking each block against a hash tree stored on
a separate *hash volume*. The tree has one *root digest*. A trusted copy of
that digest is what makes verification meaningful.

The crate supports three tasks: read a hash-volume header, format a new hash
volume, and construct a Linux verity target for activation. A header records
the format parameters, but it is optional: a target can also receive those
parameters from elsewhere.

## Read an existing header

The standard header is a 512-byte record at the start of a formatted hash
volume. Reading it tells you the hash algorithm, block sizes, data-block
count, and other format parameters. It does **not** verify either volume.

```rust,no_run
use std::{fs::File, io::Read as _};
use devmap_verity::Header;

# fn main() -> std::io::Result<()> {
let mut hash = File::open("hash.img")?;
let mut record = [0; 512];
hash.read_exact(&mut record)?;
let header = Header::decode(record)?;
println!("{} data blocks", header.parameters.data.count);
# Ok(())
# }
```

If the header is embedded in a larger volume, seek to its location first.
Decoding reads only the record; the caller handles the remaining padding up
to the hash-block boundary. In particular, the header's UUID is an identifier,
not proof that the data is authentic.

## Format a new hash volume

Formatting needs a header that describes the intended data extent and hash
algorithm. The formatter reads that extent from the data stream, writes the
header and tree to the hash stream, and returns the root digest. Enable a
hash-family Cargo feature, such as `sha2`, to use it.

```rust,no_run
# #[cfg(feature = "sha2")]
# fn main() -> std::io::Result<()> {
use std::fs::{File, OpenOptions};
use devmap_core::{BlockSize, Detect as _, Geometry};
use devmap_verity::{Algorithm, Constraint, Format as _, HashType, Header, Parameters};

let data = File::open("data.img")?;
let hash = OpenOptions::new().write(true).create_new(true).open("hash.img")?;

let header = Header {
    uuid: [7; 16],
    parameters: Parameters {
        hash_type: HashType::default(),
        algorithm: Algorithm::Sha256,
        salt: Default::default(),
        data: Geometry::<Constraint>::detect(&data)?,
        hash: BlockSize::<Constraint>::detect(&hash)?,
    },
};

let root = header.format(&data, &hash)?;
hash.sync_all()?;
// Store `root` in an independently trusted place before using the volume.
# Ok(())
# }
# #[cfg(not(feature = "sha2"))]
# fn main() {}
```

Detection is convenient when the *entire* data file is protected. The
formatter itself does not compare the declared geometry with storage geometry
or preflight the hash volume's capacity. It consumes exactly the declared
data extent, reports an early end of input, and leaves extra input unread.
The streams begin at their current positions. A write error or asynchronous
cancellation can leave a partial hash volume; formatting does not roll back.

The returned root digest must be stored separately from the data and hash
volumes. A digest supplied by the untrusted hash volume cannot authenticate
that same volume. The example synchronizes the hash file, but callers must
also arrange appropriate durability for their trusted copy of the digest.

## Activate a Linux mapping

Activation combines the data device, hash device, format parameters, and the
independently trusted root digest. If the hash volume has a header, decode it
first and use it to start the target builder. The tree begins after that
header's hash block. If the parameters come from elsewhere, start a headerless
builder and specify the tree offset yourself when it is not block zero.

This example assumes `data` and `hash` are open block devices, and
`trusted_root` came from trusted storage. It creates a read-only mapping and
returns an open file that keeps the mapping alive until its last holder closes.
The process needs device-mapper access.

```rust,no_run
# #[cfg(target_os = "linux")]
# fn activate(data: &std::fs::File, hash: &std::fs::File, trusted_root: &[u8]) -> std::io::Result<std::fs::File> {
use std::{fs::OpenOptions, io::Read as _};
use devmap_linux::{Control, DevId};
use devmap_verity::Header;

let mut record = [0; 512];
hash.try_clone()?.read_exact(&mut record)?;
let header = Header::decode(record)?;

let data_id = DevId::try_from(data.metadata()?)?;
let hash_id = DevId::try_from(hash.metadata()?)?;
let target = header.builder(data_id, hash_id, trusted_root)?.build();

let dm = Control::open()?;
let version = dm.version("verity")?;
let table = devmap_linux::Table::<4096>::default()
    .add(devmap_core::Span::new(0, target.data_sectors()), &target, version)?;
let id = dm.create("verified-data")?;
let result = (|| -> std::io::Result<std::fs::File> {
    dm.load(id, table, true)?;
    dm.suspend(id, false)?;
    let file = OpenOptions::new().read(true)
        .open(format!("/dev/dm-{}", id.minor()))?;
    dm.remove(id, true)?;
    Ok(file)
})();
if result.is_err() {
    let _ = dm.remove(id, true); // Best-effort cleanup; preserve the original error.
}
result
# }
# fn main() {}
```

Choose a mapping name that will not collide with an existing device. Loading
the table validates its parameters, but neither parsing a header nor building
a target verifies data. The kernel checks data as it is read through the
active mapping. The root digest must therefore be trusted *before* activation.
