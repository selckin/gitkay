Name:           gitkay
Version:        0.0.5
Release:        1%{?dist}
Summary:        A fast, native Wayland git history viewer
License:        MIT
URL:            https://github.com/selckin/gitkay
Source0:        %{name}-%{version}.tar.gz

# Edition 2024 needs rust >= 1.85, let-chains >= 1.88, and a const
# Duration::from_hours >= 1.91.
BuildRequires:  rust >= 1.91
BuildRequires:  cargo
BuildRequires:  gcc
BuildRequires:  pkg-config
# No GTK/graphene/OpenSSL: nothing in the dependency tree links them (git2 is
# built with default-features = [], so there is no openssl-sys), and
# libgit2-sys compiles the bundled libgit2 with cc, not cmake.

# rpm's auto-requires reads the ELF, and winit/glutin/wayland-sys DLOPEN
# everything windowing-related — nothing reading the ELF can see a dlopen. So the
# sonames the binary names (strings target/release/gitkay) are stated here, or the
# package installs cleanly on a minimal Wayland desktop and then aborts at launch.
# SONAME form rather than package names, which differ per distro
# (libwayland-client on Fedora, libwayland-client0 on openSUSE) while every rpm
# distro's auto-PROVIDES emits the soname. Wayland + EGL + xkbcommon are required;
# the X11 set is the fallback backend, so it is Recommends and a Wayland-only
# system is not made to pull it in. The release workflow's binary repack carries
# the same split — keep the two lists in step.
#
# The `()(64bit)` suffix is part of the provide's NAME on a 64-bit build (a
# 32-bit one provides the bare soname, a different string), so it cannot be
# dropped and there is no macro that renders it correctly on both. Hardcoded,
# because gitkay is 64-bit-only in practice: the release workflow builds x86_64
# and aarch64 and nothing else.
Requires:       libwayland-client.so.0()(64bit)
Requires:       libwayland-egl.so.1()(64bit)
Requires:       libxkbcommon.so.0()(64bit)
Requires:       libEGL.so.1()(64bit)
Recommends:     libX11.so.6()(64bit)
Recommends:     libX11-xcb.so.1()(64bit)
Recommends:     libxcb.so.1()(64bit)
Recommends:     libxkbcommon-x11.so.0()(64bit)
Recommends:     libXcursor.so.1()(64bit)
Recommends:     libXi.so.6()(64bit)
Recommends:     libvulkan.so.1()(64bit)

%description
gitkay is a native Wayland git history viewer — gitk, but okay.
Features a commit graph with colored branch lanes, syntax-highlighted
diffs, file list sidebar, search, and Catppuccin Mocha dark theme.
Built with Rust + egui for fast startup and smooth scrolling.

%prep
%autosetup

%build
cargo build --release --locked

%check
cargo test --release --locked

%install
install -Dm755 target/release/gitkay %{buildroot}%{_bindir}/gitkay

%files
%license LICENSE
%doc README.md
%{_bindir}/gitkay

%changelog
* Sun Aug 02 2026 Thomas Matthijs <github@selckin.be> - 0.0.5-1
- This fork numbers its own releases from 0.0.1, below the original project's
  1.x line; the versions below are the original project's.
- Packaging corrected: build dependencies now match the dependency tree, and
  the declared Rust version matches edition 2024 + let-chains.

* Tue Mar 25 2026 Marenz <marenz@supradigital.org> - 1.2.0-1
- Any keypress focuses search bar instantly
- Graph auto-scrolls to search matches
- Search match highlight rework (yellow accent bar)
- Branch dimming suppressed during active search

* Sat Mar 22 2026 Marenz <marenz@supradigital.org> - 1.0.0-1
- Initial release
- Commit graph with colored lanes and merge visualization
- Syntax-highlighted diff viewer with file list sidebar
- Search by SHA, author, message, branch, tag
- Native Wayland, Catppuccin Mocha theme
