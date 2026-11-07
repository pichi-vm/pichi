# Pichi build appliance

[The recipe](image/pichi.yaml) extends Fedora 43 and installs the `conglobate` and
`pichi-lsm` packages from COPR. Their packages own the service and mount
units; this recipe supplies network configuration, orders policy startup before
the build service through a systemd drop-in, and enables the appliance services.
The resulting VM runs
`conglobate appliance` to execute `pichi build` requests.

[The CI workflow](../.github/workflows/appliance.yml) builds the image once on
each native Linux architecture, then uses it to build Fedora. Pichi's existing
VM tests also run against these images.

After all main-branch checks pass, CI publishes both architectures as
`ghcr.io/pichi-vm/pichi-build:<Pichi version>`. Select this image with
`pichi build --build-image`; the shipping default remains unchanged.
