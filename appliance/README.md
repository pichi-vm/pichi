# Pichi build appliance

[The recipe](image/pichi.yaml) installs signed Conglobate and Pichi LSM packages
from COPR. Its package owns the service and mount units; the recipe supplies network configuration,
policy-service ordering and service enablement.

[The platform workflow](../.github/workflows/platforms.yml) conditionally builds
the image on each native runner and uses that local candidate for build/boot
compatibility tests. This avoids downloading large candidate artifacts on Avrea.
There is no separate Fedora qualification build. See [the CI contract](../BUILD.md#10-continuous-integration).

After required main-branch checks pass, CI publishes both architectures as
`ghcr.io/pichi-vm/pichi-build:latest`, the default for `pichi build`.
Compatibility failures are reported but do not block publication, including
intentional breaking changes. Pull requests never publish.

## Cached-input builds

The optional [offline recipe](offline/pichi.yaml) uses a dedicated build
service without network or clock bootstrap. It retains the repository,
cache and output mounts, Pichi LSM, and the normal build protocol. Internet
sockets are denied, including for steps that request network access. Use it
when every build input is already available in the host cache and no step
needs the network. The default appliance continues to support networked
builds.

From a committed checkout of this repository, build the offline appliance:

```sh
pichi build . --ref HEAD --plan appliance/offline/pichi.yaml \
  --lock appliance/offline/pichi.lock -t pichi-build:offline \
  --memory 4096 --cpus 2
```

Select it explicitly for a workload build:

```sh
pichi build /path/to/project --ref HEAD -t example:built \
  --build-image pichi-build:offline --memory 4096 --cpus 2
```

The lock pins the published base appliance. Refresh it deliberately with
`pichi update . --plan appliance/offline/pichi.yaml --lock appliance/offline/pichi.lock`
and commit the updated lock before rebuilding.
