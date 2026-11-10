Name:           pichi
Version:        0.1.0~dev
Release:        1%{?buildtag}%{?dist}
Summary:        Front-end for Pichi VM images

License:        Apache-2.0 AND MIT AND Unicode-3.0 AND Zlib AND ISC AND BSD-3-Clause AND Unlicense AND BSL-1.0 AND CDLA-Permissive-2.0
URL:            https://github.com/pichi-vm/pichi
Source0:        %{name}-%{version}.tar.gz
ExclusiveArch:  x86_64 aarch64

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  git
BuildRequires:  cmake
BuildRequires:  gcc-c++
BuildRequires:  make
BuildRequires:  systemd-rpm-macros
Requires:       dillo-vm
Requires:       madrillo
Requires:       ca-certificates

%description
Front-end for Pichi VM images.

%package -n pichi-builder
Summary:        Pichi build appliance launcher
Requires:       conglobate
Requires:       systemd

%description -n pichi-builder
Build-appliance console protocol, fixed mounts and service orchestration.
Invokes Conglobate through its ordinary build command.

%prep
%autosetup

%build
export CARGO_HOME="$PWD/.cargo-home"
export RUSTC_BOOTSTRAP=1
cargo build --locked --release --bin pichi
cargo build --locked --release -p pichi-builder

%install
install -Dpm0755 target/release/pichi %{buildroot}%{_bindir}/pichi
install -Dpm0755 target/release/pichi-builder %{buildroot}%{_bindir}/pichi-builder
install -Dpm0644 appliance/image/pichi-builder.service %{buildroot}%{_unitdir}/pichi-builder.service

# Preserve license and notice files from the fetched dependencies.
find .cargo-home -type f \( -iname 'license*' -o -iname 'licence*' -o \
    -iname 'copying*' -o -iname 'notice*' -o -iname 'copyright*' \) \
    -exec install -Dm0644 '{}' '%{buildroot}%{_licensedir}/%{name}/dependencies/{}' \; \
    -exec install -Dm0644 '{}' '%{buildroot}%{_licensedir}/pichi-builder/dependencies/{}' \;

%files
%license %{_licensedir}/%{name}/dependencies/
%license LICENSE
%doc README.md
%{_bindir}/pichi

%files -n pichi-builder
%license %{_licensedir}/pichi-builder/dependencies/
%license LICENSE
%{_bindir}/pichi-builder
%{_unitdir}/pichi-builder.service
