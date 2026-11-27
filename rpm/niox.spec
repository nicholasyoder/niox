# Built by build.sh, which fills in the version (from the workspace
# Cargo.toml) and makes both sources: the tree, and its crates.io
# dependencies vendored by `cargo vendor`, so the build runs offline (e.g.
# in mock).
%{!?niox_version: %global niox_version 0}

# Rust release builds carry no debug info to split out.
%global debug_package %{nil}

Name:           niox
Version:        %{niox_version}
Release:        1%{?dist}
Summary:        Share one keyboard and mouse across Linux machines over the LAN
License:        MPL-2.0
URL:            https://github.com/nicholasyoder/niox
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.gz

ExclusiveArch:  %{rust_arches}
BuildRequires:  cargo >= 1.100
BuildRequires:  rust >= 1.100
BuildRequires:  systemd-rpm-macros
BuildRequires:  firewalld-filesystem
# niox-session's audio: libroc and libpipewire, with bindgen (libclang)
# generating libroc's bindings.
BuildRequires:  pkgconfig(roc) >= 0.4
BuildRequires:  pkgconfig(libpipewire-0.3)
BuildRequires:  clang-devel

Requires:       firewalld-filesystem
Requires(post): firewalld-filesystem

%description
niox shares one machine's keyboard and mouse with other Linux machines
over an encrypted UDP transport, switching between them with a hotkey.
nioxd runs as a system service from boot, so it works at the login screen
and on a host with nobody logged in. nioxctl shows and controls it.

%package session
Summary:        niox audio: the optional per-user service
Requires:       %{name}%{?_isa} = %{version}-%{release}
# The PipeWire server and its session manager: libpipewire alone isn't
# enough. (Fedora's desktops have them already.)
Requires:       pipewire
Requires:       wireplumber
Requires:       pipewire-pulseaudio

%description session
niox-session is the optional, unprivileged half of niox, run per user
under systemctl --user. It carries audio over nioxd's encrypted
transport: a client's system audio plays on its host, mixed with the
host's other clients or following keyboard/mouse focus. It's a separate
package so that keyboard/mouse-only installs don't need libroc or
libpipewire. The user unit is enabled for every user and starts for
members of the niox group.

%prep
%autosetup -n %{name}-%{version}
tar -xzf %{SOURCE1}
mkdir -p .cargo
cat > .cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
EOF

%build
cargo build --release --frozen --offline -p nioxd -p nioxctl -p niox-session

%install
install -Dm755 target/release/nioxd %{buildroot}%{_bindir}/nioxd
install -Dm755 target/release/nioxctl %{buildroot}%{_bindir}/nioxctl
install -Dm755 target/release/niox-session %{buildroot}%{_bindir}/niox-session
install -Dm644 dist/nioxd.service %{buildroot}%{_unitdir}/nioxd.service
install -Dm644 dist/niox-session.service %{buildroot}%{_userunitdir}/niox-session.service
install -Dm644 dist/niox.sysusers %{buildroot}%{_sysusersdir}/niox.conf
install -Dm644 dist/niox.firewalld.xml %{buildroot}%{_prefix}/lib/firewalld/services/niox.xml
# nioxd writes niox.toml (with a fresh keypair) on first start. It's not a
# packaged file, so upgrades and removal never touch it.
install -dm755 %{buildroot}%{_sysconfdir}/niox

%post
%firewalld_reload
# Enabled on install regardless of presets (like NKMS): KM is the point of
# installing it, and it has to be running at the login screen.
if [ $1 -eq 1 ]; then
    systemctl daemon-reload >/dev/null 2>&1 || :
    systemctl enable --now nioxd.service >/dev/null 2>&1 || :
fi

%preun
%systemd_preun nioxd.service

%postun
%systemd_postun_with_restart nioxd.service

%post session
# Enabled for every user on install regardless of presets, as nioxd is:
# installing it is how audio is opted into. ConditionGroup=niox keeps it
# from starting for users outside the niox group.
if [ $1 -eq 1 ]; then
    systemctl --global enable niox-session.service >/dev/null 2>&1 || :
fi

%preun session
%systemd_user_preun niox-session.service

%postun session
%systemd_user_postun_with_restart niox-session.service

%files
%license LICENSE
%doc README.md CHANGELOG.md
%{_bindir}/nioxd
%{_bindir}/nioxctl
%{_unitdir}/nioxd.service
%{_sysusersdir}/niox.conf
%{_prefix}/lib/firewalld/services/niox.xml
%dir %{_sysconfdir}/niox

%files session
%license LICENSE
%{_bindir}/niox-session
%{_userunitdir}/niox-session.service

%changelog
* Fri Nov 27 2026 Nicholas Yoder <nicholasyoder@proton.me> - 0.2.1-1
- Lip sync: players on a client hold video back by the audio's delay to the
  host, the host's own output path included.

* Fri Nov 27 2026 Nicholas Yoder <nicholasyoder@proton.me> - 0.2.0-1
- Audio: clients' system audio plays on the host (niox-session subpackage).

* Thu Nov 26 2026 Nicholas Yoder <nicholasyoder@proton.me> - 0.1.0-1
- First packaged release: keyboard/mouse sharing with nioxd and nioxctl.
