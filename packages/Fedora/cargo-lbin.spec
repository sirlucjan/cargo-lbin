%define _disable_source_fetch 0

Name:           cargo-lbin
Version:        0.11.0
Release:        1%{?dist}
Summary:        Cargo-powered application manager for crates.io command-line binaries

License:        MIT AND Apache-2.0
URL:            https://github.com/sirlucjan/cargo-lbin
Source0:        %{URL}/archive/refs/tags/%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
Requires: cargo
Requires: rust

%description
Cargo-powered application manager for crates.io command-line binaries

%prep
%autosetup -n cargo-lbin-%{version}


%build
export CARGO_HOME=%{_builddir}/.cargo
cargo fetch --locked
cargo build --release --frozen

%install

# Install all built executables (skip .so and .d files)
find target/release \
    -maxdepth 1 -type f -executable ! -name '*.so' \
    -exec install -Dm755 -t %{buildroot}%{_bindir} {} +

%files

# Binaries
%{_bindir}/cargo-lbin
