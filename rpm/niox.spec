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

Requires:       firewalld-filesystem
Requires(post): firewalld-filesystem

%description
niox shares one machine's keyboard and mouse with other Linux machines
over an encrypted UDP transport, switching between them with a hotkey.
nioxd runs as a system service from boot, so it works at the login screen
and on a host with nobody logged in. nioxctl shows and controls it.

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
cargo build --release --frozen --offline -p nioxd -p nioxctl

%install
install -Dm755 target/release/nioxd %{buildroot}%{_bindir}/nioxd
install -Dm755 target/release/nioxctl %{buildroot}%{_bindir}/nioxctl
install -Dm644 dist/nioxd.service %{buildroot}%{_unitdir}/nioxd.service
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

%files
%license LICENSE
%doc README.md CHANGELOG.md
%{_bindir}/nioxd
%{_bindir}/nioxctl
%{_unitdir}/nioxd.service
%{_sysusersdir}/niox.conf
%{_prefix}/lib/firewalld/services/niox.xml
%dir %{_sysconfdir}/niox

%changelog
* Thu Nov 26 2026 Nicholas Yoder <nicholasyoder@proton.me> - 0.1.0-1
- First packaged release: keyboard/mouse sharing with nioxd and nioxctl.
