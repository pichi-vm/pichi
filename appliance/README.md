# Pichi build appliance

[The recipe](image/pichi.yaml) installs signed Conglobate and Pichi LSM packages
from COPR. The packages supply their services; the recipe supplies the virtiofs
mounts, network configuration, policy-service ordering and service enablement.

[The platform workflow](../.github/workflows/platforms.yml) conditionally builds
the image on each native runner and uses that local candidate for build/boot
compatibility tests. This avoids downloading large candidate artifacts on Avrea.
There is no separate Fedora qualification build. See [the CI contract](../BUILD.md#10-continuous-integration).

After required main-branch checks pass, CI publishes both architectures as
`ghcr.io/pichi-vm/pichi-build:latest`, the default for `pichi build`.
Compatibility failures are reported but do not block publication, including
intentional breaking changes. Pull requests never publish.
