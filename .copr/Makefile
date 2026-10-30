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
# a copy of the tree gets rpm-manifest.patch, then Cargo prunes
# the unused vm-tests dependencies while keeping existing pins.
# Reject any new package pin before fetching. Conglobate's host
# library disables the engine, so this graph has only one source
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
	if [[ -f packaging/rpm-manifest.patch ]]; then patch -p1 -i packaging/rpm-manifest.patch; fi; \
	mkdir -p .cargo; \
	export RUSTC_BOOTSTRAP=1; \
	cp Cargo.lock "$$workdir/original.lock"; \
	cargo update --workspace; \
	awk -v RS='' -F '\n' '/^\[\[package\]\]/ { \
		pin = ""; \
		for (i = 1; i <= NF; i++) \
			if ($$i ~ /^(name|version|source|checksum) = /) pin = pin $$i ORS; \
		if (FILENAME == ARGV[1]) pins[pin] = 1; \
		else if (!(pin in pins)) { \
			print "RPM lock introduced a new package pin:\n" pin > "/dev/stderr"; \
			exit 1; \
		} \
	}' "$$workdir/original.lock" Cargo.lock; \
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
