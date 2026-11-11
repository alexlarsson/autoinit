%global debug_package %{nil}
%global rust_target %{_target_cpu}-unknown-linux-gnu

Name:           autoinit
Version:        0.1.0
Release:        1%{?dist}
Summary:        Minimal initramfs for native composefs bootc systems

# Includes the licenses of statically linked Rust, OpenSSL, zstd and libc code.
License:        (MIT OR Apache-2.0) AND Apache-2.0 AND BSD-3-Clause AND BSL-1.0 AND LGPL-2.1-or-later AND MIT AND Zlib
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.gz
ExclusiveArch:  x86_64 aarch64

BuildRequires:  cargo >= 1.89
BuildRequires:  rust >= 1.89
BuildRequires:  gcc
BuildRequires:  glibc-static
BuildRequires:  make
BuildRequires:  perl-interpreter
BuildRequires:  perl(lib)
BuildRequires:  perl(IPC::Cmd)
BuildRequires:  perl(File::Compare)
BuildRequires:  perl(File::Copy)
BuildRequires:  perl(FindBin)
BuildRequires:  perl(Text::ParseWords)
BuildRequires:  perl(Time::Piece)
BuildRequires:  python3

Requires:       python3
Recommends:     zstd
Recommends:     glibc-common
Recommends:     coreutils
Recommends:     util-linux-core

%description
Statically linked Rust init binaries for local-disk, native composefs bootc
systems, and a Python tool for building their initramfs. The builder selects
the variant with embedded nmstate when early networking is configured.
Modules are selected explicitly when creating the initramfs.

%prep
%autosetup -a1
cat .cargo/vendor-config.toml >> .cargo/config.toml

%build
%make_build CARGO_TARGET=%{rust_target} CARGO_BUILD_FLAGS=--frozen

# Preserve dependency notices, including those in bundled native libraries.
mkdir -p licenses
find vendor -type f \( -iname 'license*' -o -iname 'copying*' -o -iname 'notice*' -o -iname 'copyright*' \) \
    -exec cp --parents -t licenses '{}' +
cp -a %{_datadir}/licenses/glibc licenses/glibc

%install
%make_install SKIP_BUILD=1 CARGO_TARGET=%{rust_target} \
    prefix=%{_prefix} bindir=%{_bindir} libexecdir=%{_libexecdir}

%check
mkdir -p build/test-tmp
export TMPDIR="$PWD/build/test-tmp"
%make_build check CARGO_TARGET=%{rust_target} CARGO_BUILD_FLAGS=--frozen
python3 -m py_compile mkautoinitrd

%files
%license LICENSE-MIT LICENSE-APACHE licenses
%doc README.md
%{_bindir}/mkautoinitrd
%{_libexecdir}/autoinit
%{_libexecdir}/autoinit-nmstate

%changelog
* Wed Nov 11 2026 Alexander Larsson <alexl@redhat.com> - 0.1.0-1
- Initial package
