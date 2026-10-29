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
# Reject any new package pin before fetching. Cargo cannot merge
# the two pinned carapace sources at the same version, so vendor
# the host CLI and Conglobate graphs separately, then combine
# their source replacements. cargo vendor writes
# packaging/cargo-vendor-config.toml into that copy; the spec
# appends it at build time.
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
		--exclude vendor-conglobate \
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
	cp Cargo.toml "$$workdir/Cargo.toml"; \
	cp Cargo.lock "$$workdir/Cargo.lock"; \
	sed '/^carapace.workspace = true$$/d' "$$workdir/Cargo.toml" > Cargo.toml; \
	cargo metadata --offline --format-version 1 > /dev/null; \
	cargo vendor --locked --versioned-dirs vendor-conglobate > "$$workdir/conglobate.toml"; \
	sed -i 's/vendored-sources/vendored-conglobate/g' "$$workdir/conglobate.toml"; \
	cp "$$workdir/Cargo.lock" Cargo.lock; \
	sed '/^conglobate.workspace = true$$/d' "$$workdir/Cargo.toml" > Cargo.toml; \
	cargo metadata --offline --format-version 1 > /dev/null; \
	cargo vendor --locked --versioned-dirs vendor > "$$workdir/pichi.toml"; \
	cp "$$workdir/Cargo.toml" Cargo.toml; \
	cp "$$workdir/Cargo.lock" Cargo.lock; \
	tar -C vendor-conglobate --exclude './carapace-0.1.0' -cf - . | \
		tar -C vendor -xf - --skip-old-files; \
	awk '/^\[/ { key = $$0; tables[key] = "" } \
		{ tables[key] = tables[key] $$0 ORS } \
		END { for (key in tables) printf "%s", tables[key] }' \
		"$$workdir/conglobate.toml" "$$workdir/pichi.toml" > packaging/cargo-vendor-config.toml; \
	tar -C "$$workdir/src" -czf "$$workdir/$$name-$$version.tar.gz" \
		--transform "s,^\./,$$name-$$version/," \
		--exclude './vendor' \
		--exclude './vendor-conglobate' \
		.; \
	tar -C "$$workdir/src" -cJf "$$workdir/$$name-$$version-vendor.tar.xz" vendor vendor-conglobate; \
	rpmbuild -bs \
		--define "_sourcedir $$workdir" \
		--define "_srcrpmdir $(outdir)" \
		--define "_disable_source_fetch 0" \
		"$$specfile"
