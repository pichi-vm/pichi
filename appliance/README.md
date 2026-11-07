# Pichi build appliance

[The recipe](image/pichi.yaml) extends Fedora 43 and installs Conglobate and
the LSM label helper from recorded submodules. The VM's [request handler](guest)
receives `pichi build` inputs and executes `conglobate build` with fixed mount
paths and identity mappings. Conglobate validates the inputs and builds the image.

[The CI workflow](../.github/workflows/appliance.yml) builds the image once on
each native Linux architecture, then uses it to build Fedora. Pichi's existing
VM tests also run against these images.

After all main-branch checks pass, CI publishes both architectures as
`ghcr.io/pichi-vm/pichi-build:<Pichi version>`. Select this image with
`pichi build --build-image`; the shipping default remains unchanged.
