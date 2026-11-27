#!/usr/bin/env bash
# Builds niox's packages into target/dist/.
#
#   ./build.sh          the RPM, then the .deb
#   ./build.sh rpm      RPM + SRPM with rpmbuild (needs rpm-build, cargo, rust,
#                       firewalld-filesystem). For a clean-chroot build, run
#                       mock on the SRPM:  mock -r fedora-45-x86_64 target/dist/niox-*.src.rpm
#   ./build.sh deb      .deb, built in a debian:trixie container with podman
#                       (Debian's rustc is older than this workspace needs, and
#                       binaries built here would need Fedora's newer glibc)
#
# Both start from one source tarball of the working tree (tracked and
# untracked-but-not-ignored files). The RPM also gets the
# crates.io dependencies vendored, so rpmbuild and mock build offline.
#
# The version comes from the workspace Cargo.toml; debian/changelog's newest
# entry must match it.
set -euo pipefail

cd "$(dirname "$0")"
root=$PWD
what=${1:-all}

version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
deb_version=$(sed -n '1s/^niox (\([^)-]*\)-[^)]*).*/\1/p' debian/changelog)
if [[ -z $version || $version != "$deb_version" ]]; then
    echo "version mismatch: Cargo.toml has '$version', debian/changelog has '$deb_version'" >&2
    exit 1
fi

out=$root/target/dist
work=$root/target/dist-work
rm -rf "$work"
mkdir -p "$out" "$work"

tarball=$work/niox-$version.tar.gz
git ls-files -z --cached --others --exclude-standard -- . |
    xargs -0 sh -c 'for f; do [ -e "$f" ] || [ -L "$f" ] && printf "%s\0" "$f"; done' sh |
    tar --null --files-from=- --transform "s,^,niox-$version/,S" -czf "$tarball"

build_rpm() {
    local top=$work/rpmbuild
    mkdir -p "$top/SOURCES"
    cp "$tarball" "$top/SOURCES/"
    (cd "$work" && cargo vendor --locked --manifest-path "$root/Cargo.toml" vendor >/dev/null &&
        tar -czf "$top/SOURCES/niox-$version-vendor.tar.gz" vendor)
    # The version goes into the spec itself, so the SRPM rebuilds as is.
    sed "s/^%{!?niox_version: %global niox_version 0}\$/%global niox_version $version/" \
        rpm/niox.spec >"$work/niox.spec"
    grep -q "^%global niox_version $version\$" "$work/niox.spec"
    rpmbuild -ba "$work/niox.spec" --define "_topdir $top"
    find "$top/RPMS" "$top/SRPMS" -name '*.rpm' -exec cp {} "$out/" \;
}

build_deb() {
    # rustup's toolchain and cargo's registry live in named volumes, so
    # rebuilds don't download them again.
    podman run --rm \
        -v "$tarball:/src/niox.tar.gz:ro,Z" \
        -v "$out:/out:Z" \
        -v niox-deb-rustup:/root/.rustup \
        -v niox-deb-cargo:/root/.cargo \
        docker.io/library/debian:trixie sh -euc "
            mkdir /build && cd /build
            tar -xzf /src/niox.tar.gz
            cd niox-$version
            export DEBIAN_FRONTEND=noninteractive
            apt-get update -qq
            apt-get install -qq -y --no-install-recommends \
                build-essential ca-certificates curl >/dev/null
            # debian/control's Build-Depends.
            apt-get build-dep -qq -y --no-install-recommends ./ >/dev/null
            if [ ! -x /root/.cargo/bin/rustup ]; then
                curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain none
            fi
            export PATH=/root/.cargo/bin:\$PATH
            dpkg-buildpackage -us -uc -b
            cp ../niox_*.deb ../niox-session_*.deb /out/
        "
}

case $what in
    rpm) build_rpm ;;
    deb) build_deb ;;
    all) build_rpm; build_deb ;;
    *) echo "usage: $0 [rpm|deb]" >&2; exit 2 ;;
esac

echo
echo "Packages in $out:"
ls -1 "$out"
