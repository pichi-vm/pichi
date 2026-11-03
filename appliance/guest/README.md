# Build guest adapter

`pichi-build` requests build inputs on the named `dev.pichi.build` console,
then replaces itself with `/usr/bin/conglobate build`. The engine validates
the selected Git ref, context, plan, lock and optional provenance locator.
The console closes before the engine starts; its exit status reaches systemd.

The appliance mounts `repo`, `cache` and `output` at `/run/pichi/`. UID/GID
mappings are fixed at `100000:65536`. Networking and the resolver come from
the guest environment. The host controls neither these settings nor the
engine executable path.

This Linux executable has a separate Cargo workspace. Host builds only
consume the portable `pichi-launch` crate. From this directory, run
`cargo test --locked` and `cargo clippy --locked --all-targets -- -D warnings`.
