# Copr make_srpm entry point. Copr cds into the clone and runs
# `make -f .copr/Makefile srpm` with spec= and outdir=.
#
# make_srpm has no chroot package list in the Copr UI, so this
# target installs what it needs with dnf (cargo, rust, rpm-build,
# tar, xz, gzip). Do not require git: Copr already provides the
# tree as cwd. Override DNF (e.g. DNF=true) when the host already
# has those tools, such as GitHub Actions with apt + rustup.
#
# Source0 is this tree tarball. Source1 is a vendor tarball
# produced here. vendor/ is not committed. RUSTC_BOOTSTRAP=1
# lets cargo vendor accept [unstable] bindeps. Before vendoring,
# a copy of the tree gets host-manifest.patch, then trim-cargo-lock removes
# unused vm-tests entries and rejects new package pins before fetching.
# Conglobate's host library disables the engine, so this graph has only one source
# for each package/version. cargo vendor writes
# packaging/cargo-vendor-config.toml into that copy; the spec
# loads it as a separate Cargo configuration at build time.
SHELL := /bin/bash
DNF ?= dnf

srpm:
	$(DNF) -y install cargo rust rpm-build tar xz gzip
	set -euo pipefail; \
	root=$$(pwd); \
	workdir=$$(mktemp -d); \
	trap 'rm -rf "$$workdir"' EXIT; \
	specfile="$(spec)"; \
	if [[ "$$specfile" != /* ]]; then specfile="$$root/$$specfile"; fi; \
	version=$$(awk '/^Version:/ { print $$2; exit }' "$$specfile"); \
	name=$$(awk '/^Name:/ { print $$2; exit }' "$$specfile"); \
	mkdir -p "$(outdir)" "$$workdir/src"; \
	tar -C "$$root" -cf - \
		--exclude-vcs \
		--exclude target \
		--exclude vendor \
		. | tar -C "$$workdir/src" -xf -; \
	cd "$$workdir/src"; \
	if [[ -f packaging/host-manifest.patch ]]; then patch -p1 -i packaging/host-manifest.patch; fi; \
	mkdir -p .cargo; \
	export RUSTC_BOOTSTRAP=1; \
	packaging/trim-cargo-lock; \
	cargo fetch --locked; \
	cargo vendor --locked --versioned-dirs vendor > packaging/cargo-vendor-config.toml; \
	tar -C "$$workdir/src" -czf "$$workdir/$$name-$$version.tar.gz" \
		--transform "s,^\./,$$name-$$version/," \
		--exclude './vendor' \
		.; \
	tar -C "$$workdir/src" -cJf "$$workdir/$$name-$$version-vendor.tar.xz" vendor; \
	rpmbuild -bs \
		--define "_sourcedir $$workdir" \
		--define "_srcrpmdir $(outdir)" \
		--define "_disable_source_fetch 0" \
		"$$specfile"
