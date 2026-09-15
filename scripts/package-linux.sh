#!/usr/bin/env bash
# Builds the Linux installers from an existing release build:
#   dist/forge_<version>_amd64.deb            (Debian/Ubuntu, `sudo apt install ./…`)
#   dist/forge-<version>-linux-x86_64.tar.gz  (any distro; bundled install.sh)
#
# Prerequisites (both done by .github/workflows/release.yml):
#   ./scripts/bootstrap-ghostty.sh
#   cargo build --release -p forge-gui -p proto-termd
#
# Layout inside the package: everything private lives in <prefix>/lib/forge
# (forge-gui, proto-termd, libghostty-vt.so, shell-integration/), which is
# where the binaries look for their siblings; <prefix>/bin/forge is a tiny
# launcher and the desktop entry/icon go under <prefix>/share.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(cd -- "${script_dir}/.." && pwd)
cd "${project_dir}"

version=${1:-${FORGE_VERSION:-}}
if [ -z "${version}" ]; then
  cargo_version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
  describe=$(git describe --tags --always --dirty 2>/dev/null || true)
  case "${describe}" in
    v"${cargo_version}") version="${cargo_version}" ;;
    "") version="${cargo_version}" ;;
    *) version="${cargo_version}+git$(git rev-parse --short HEAD)" ;;
  esac
fi
version=${version#v}

arch=$(uname -m)
case "${arch}" in
  x86_64) deb_arch=amd64 ;;
  aarch64) deb_arch=arm64 ;;
  *) echo "unsupported architecture ${arch}" >&2; exit 2 ;;
esac

gui=target/release/forge-gui
termd=target/release/proto-termd
ghostty=${FORGE_GHOSTTY_LIB:-target/ghostty/lib/libghostty-vt.so}
for file in "${gui}" "${termd}" "${ghostty}"; do
  if [ ! -f "${file}" ]; then
    echo "missing ${file}; run scripts/bootstrap-ghostty.sh and cargo build --release -p forge-gui -p proto-termd" >&2
    exit 2
  fi
done

app_id=dev.forge.Forge
stage=target/package
dist=dist
rm -rf "${stage}"
mkdir -p "${dist}"

# ----- common tree ----------------------------------------------------------
tree="${stage}/tree"
install -Dm755 "${gui}" "${tree}/lib/forge/forge-gui"
install -Dm755 "${termd}" "${tree}/lib/forge/proto-termd"
# Resolve the symlink chain so the package carries the real library.
install -Dm755 "$(readlink -f "${ghostty}")" "${tree}/lib/forge/libghostty-vt.so"
mkdir -p "${tree}/lib/forge/shell-integration"
cp -R assets/shell-integration/. "${tree}/lib/forge/shell-integration/"
find "${tree}/lib/forge/shell-integration" -type f -exec chmod 644 {} +
mkdir -p "${tree}/bin"
cat > "${tree}/bin/forge" <<'LAUNCHER'
#!/bin/sh
# Forge launcher: the real binaries live next to each other in lib/forge.
here=$(dirname -- "$(readlink -f -- "$0")")
exec "${here}/../lib/forge/forge-gui" "$@"
LAUNCHER
chmod 755 "${tree}/bin/forge"
ln -s forge "${tree}/bin/forge-gui"
install -Dm644 "assets/linux/${app_id}.svg" \
  "${tree}/share/icons/hicolor/scalable/apps/${app_id}.svg"
mkdir -p "${tree}/share/applications"
sed 's/^Exec=.*/Exec=forge %F/' "assets/linux/${app_id}.desktop" \
  > "${tree}/share/applications/${app_id}.desktop"
chmod 644 "${tree}/share/applications/${app_id}.desktop"
install -Dm644 README.md "${tree}/share/doc/forge/README.md"

strip --strip-unneeded "${tree}/lib/forge/forge-gui" "${tree}/lib/forge/proto-termd" 2>/dev/null || true
# dpkg wants directories without group write permission.
find "${tree}" -type d -exec chmod 755 {} +

# The glibc the binaries really need, so apt refuses the package on an older
# distribution instead of failing at launch with "GLIBC_x.y not found".
glibc=$(for file in "${gui}" "${termd}" "${ghostty}"; do
  objdump -T "${file}" 2>/dev/null | grep -o 'GLIBC_[0-9.]*' || true
done | sed 's/^GLIBC_//' | sort -uV | tail -n 1)
glibc=${glibc:-2.34}

# ----- .deb -----------------------------------------------------------------
deb_root="${stage}/deb"
mkdir -p "${deb_root}/usr" "${deb_root}/DEBIAN"
chmod 755 "${deb_root}" "${deb_root}/usr"
cp -R "${tree}/." "${deb_root}/usr/"
installed_size=$(du -sk "${deb_root}/usr" | cut -f1)
cat > "${deb_root}/DEBIAN/control" <<CONTROL
Package: forge
Version: ${version}
Section: devel
Priority: optional
Architecture: ${deb_arch}
Installed-Size: ${installed_size}
Maintainer: Orvix <developert@orvixapp.com>
Homepage: https://github.com/orvixapp/forge
Depends: libc6 (>= ${glibc}), libgcc-s1, libxkbcommon0, libxkbcommon-x11-0, libxcb1, libxcb-xkb1, libxau6, libxdmcp6, libvulkan1
Recommends: mesa-vulkan-drivers, libwayland-client0, fonts-dejavu-core, bash | zsh | fish
Description: Terminal-first development environment
 Forge combines a GPU-rendered terminal (libghostty-vt), a code editor with
 tree-sitter highlighting and language-server support, and ACP/MCP agent
 sessions in one window. Run it as \`forge\`.
CONTROL
deb="${dist}/forge_${version}_${deb_arch}.deb"
rm -f "${deb}"
dpkg-deb --build --root-owner-group "${deb_root}" "${deb}" >/dev/null
echo "${deb}"

# ----- tarball with install.sh ---------------------------------------------
name="forge-${version}-linux-${arch}"
tar_root="${stage}/${name}"
mkdir -p "${tar_root}"
cp -R "${tree}/." "${tar_root}/"
cat > "${tar_root}/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Installs Forge for the current user (default: ~/.local, no root needed)
# or system-wide: sudo ./install.sh --prefix /usr/local
set -euo pipefail
prefix="${XDG_DATA_HOME:+${XDG_DATA_HOME%/share}}"
prefix="${prefix:-${HOME}/.local}"
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) prefix=$2; shift 2 ;;
    --prefix=*) prefix=${1#--prefix=}; shift ;;
    -h|--help) echo "usage: $0 [--prefix DIR]   (default: ${prefix})"; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
mkdir -p "${prefix}/bin" "${prefix}/lib" "${prefix}/share/applications" \
  "${prefix}/share/icons/hicolor/scalable/apps"
rm -rf "${prefix}/lib/forge"
cp -R "${here}/lib/forge" "${prefix}/lib/forge"
cp "${here}/bin/forge" "${prefix}/bin/forge"
chmod 755 "${prefix}/bin/forge"
ln -sfn forge "${prefix}/bin/forge-gui"
cp "${here}/share/applications/dev.forge.Forge.desktop" "${prefix}/share/applications/"
cp "${here}/share/icons/hicolor/scalable/apps/dev.forge.Forge.svg" \
  "${prefix}/share/icons/hicolor/scalable/apps/"
# Menus find the launcher through its absolute path even when bin is not in PATH.
sed -i "s|^Exec=.*|Exec=${prefix}/bin/forge %F|" "${prefix}/share/applications/dev.forge.Forge.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "${prefix}/share/applications" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -f -t "${prefix}/share/icons/hicolor" 2>/dev/null || true
echo "Forge installed: ${prefix}/bin/forge"
case ":${PATH}:" in
  *":${prefix}/bin:"*) ;;
  *) echo "note: ${prefix}/bin is not in PATH; the desktop menu entry works regardless" ;;
esac
INSTALL
chmod 755 "${tar_root}/install.sh"
cat > "${tar_root}/uninstall.sh" <<'UNINSTALL'
#!/usr/bin/env bash
# Removes what install.sh put under the same prefix.
set -euo pipefail
prefix="${HOME}/.local"
case "${1:-}" in
  --prefix) prefix=$2 ;;
  --prefix=*) prefix=${1#--prefix=} ;;
esac
rm -rf "${prefix}/lib/forge"
rm -f "${prefix}/bin/forge" "${prefix}/bin/forge-gui" \
  "${prefix}/share/applications/dev.forge.Forge.desktop" \
  "${prefix}/share/icons/hicolor/scalable/apps/dev.forge.Forge.svg"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "${prefix}/share/applications" || true
echo "Forge removed from ${prefix}"
UNINSTALL
chmod 755 "${tar_root}/uninstall.sh"
tarball="${dist}/${name}.tar.gz"
rm -f "${tarball}"
tar -C "${stage}" -czf "${tarball}" --owner=0 --group=0 "${name}"
echo "${tarball}"

# Unversioned copies give install.sh stable download URLs on every release.
cp "${deb}" "${dist}/forge_${deb_arch}.deb"
cp "${tarball}" "${dist}/forge-linux-${arch}.tar.gz"
(
  cd "${dist}"
  sha256sum "$(basename "${deb}")" "$(basename "${tarball}")" \
    "forge_${deb_arch}.deb" "forge-linux-${arch}.tar.gz" > "SHA256SUMS-${version}.txt"
  cp "SHA256SUMS-${version}.txt" SHA256SUMS.txt
)
echo "${dist}/SHA256SUMS-${version}.txt"
