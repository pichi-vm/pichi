# pichi-artifact

Shared OCI-compatible artifact formats for the pichi ecosystem. Both pichi and
conglobate depend on this library; it does not depend on either application.

The crate defines manifests, component descriptors, media types and annotations,
content digests, image references, and the versioned artifact config-blob schema.
Artifacts may contain a carapace, firmware (PMI/DTB), or both. Cache operations,
registry transport, image construction, and VM-launch policy belong to consumers.

Extracted from `pichi/deps/pichi-artifact` with its file history preserved.
The extraction preserves public type names, media types, and serialization.

## Development

```sh
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

Licensed under Apache-2.0.
