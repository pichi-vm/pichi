# Pichi build appliance

[The recipe](image/pichi.yaml) installs signed Conglobate and Pichi LSM packages
from COPR. The recipe supplies the build service, virtiofs mounts, network
configuration, clock bootstrap and service enablement. `conglobate run` handles the console
protocol with the repository, cache, output and ID mappings
provided by the service. Network services remain enabled, but network readiness
and clock synchronization are skipped when the host supplies no network adapter.
The appliance loads `virtio_net` before testing for a hardware-backed interface;
this covers the fixed devices present at VM startup, not later hotplug. Resolver
startup remains required so the build service can bind its resolver file.

The recipe requires a published Conglobate package with the `run` command.
Publish this replacement appliance before removing Conglobate's legacy appliance
entry point and package units.

[The platform workflow](../.github/workflows/platforms.yml) conditionally builds
the image on each native runner and uses that local candidate for build/boot
compatibility tests. This avoids downloading large candidate artifacts on Avrea.
There is no separate Fedora qualification build. See [the CI contract](../BUILD.md#10-continuous-integration).

After required main-branch checks pass, CI publishes both architectures as
`ghcr.io/pichi-vm/pichi-build:latest`, the default for `pichi build`.
Compatibility failures are reported but do not block publication, including
intentional breaking changes. Pull requests never publish.
