#!/usr/bin/env bash
# Forge installer for Linux (x86_64): downloads the latest build from GitHub
# Releases, verifies it and installs it. No clone or Rust toolchain needed.
#
#   curl -fsSL https://raw.githubusercontent.com/orvixapp/forge/main/install.sh | bash
#
# Options (after `bash -s --`):
#   --version TAG   release to install: a `vX.Y.Z` tag or `dev` (default: the
#                   latest stable release, else the rolling `dev` build)
#   --user          install for the current user only (~/.local), never the .deb
#   --prefix DIR    install the tarball into DIR (implies --user semantics)
#   --deb           force the Debian package (needs sudo/apt)
#   --uninstall     remove a previous installation
#   -h, --help
set -euo pipefail

repo=${FORGE_REPO:-orvixapp/forge}
version=${FORGE_VERSION:-}
mode=auto
prefix=""
uninstall=0

usage() { sed -n '2,16p' "$0" 2>/dev/null || true; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) version=$2; shift 2 ;;
    --version=*) version=${1#--version=}; shift ;;
    --user) mode=tar; shift ;;
    --prefix) prefix=$2; mode=tar; shift 2 ;;
    --prefix=*) prefix=${1#--prefix=}; mode=tar; shift ;;
    --deb) mode=deb; shift ;;
    --uninstall) uninstall=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "install.sh: unknown option $1" >&2; exit 2 ;;
  esac
done

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

as_root() {
  if [ "$(id -u)" -eq 0 ]; then "$@"
  elif command -v sudo >/dev/null 2>&1; then sudo "$@"
  else die "this step needs root and sudo is not available; rerun with --user for a ~/.local install"
  fi
}

have_apt() { command -v dpkg >/dev/null 2>&1 && command -v apt-get >/dev/null 2>&1; }

# ----- uninstall --------------------------------------------------------------
if [ "$uninstall" -eq 1 ]; then
  removed=0
  if have_apt && dpkg -s forge >/dev/null 2>&1; then
    say "Removing the forge package"
    as_root apt-get remove -y forge
    removed=1
  fi
  for dir in "${prefix:-}" "${HOME}/.local" /usr/local; do
    [ -n "$dir" ] || continue
    if [ -d "$dir/lib/forge" ]; then
      say "Removing $dir/lib/forge"
      rm_cmd=(rm -rf "$dir/lib/forge" "$dir/bin/forge" "$dir/bin/forge-gui" \
        "$dir/share/applications/dev.forge.Forge.desktop" \
        "$dir/share/icons/hicolor/scalable/apps/dev.forge.Forge.svg")
      if [ -w "$dir/lib" ]; then "${rm_cmd[@]}"; else as_root "${rm_cmd[@]}"; fi
      command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$dir/share/applications" 2>/dev/null || true
      removed=1
    fi
  done
  [ "$removed" -eq 1 ] && say "Forge removed" || say "Nothing to remove"
  exit 0
fi

# ----- preflight ----------------------------------------------------------------
[ "$(uname -s)" = Linux ] || die "only Linux is supported for now"
arch=$(uname -m)
[ "$arch" = x86_64 ] || die "no build for $arch yet (x86_64 only)"

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
  probe() { curl -fsSLI -o /dev/null "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
  probe() { wget -q --spider "$1"; }
else
  die "curl or wget is required"
fi
command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required (coreutils)"
command -v tar >/dev/null 2>&1 || die "tar is required"

if [ "$mode" = auto ]; then
  if have_apt && { [ "$(id -u)" -eq 0 ] || command -v sudo >/dev/null 2>&1; }; then
    mode=deb
  else
    mode=tar
  fi
fi
if [ "$mode" = deb ] && ! have_apt; then
  die "--deb needs dpkg and apt-get; use --user on this distribution"
fi

# ----- which release -----------------------------------------------------------
if [ -n "${FORGE_RELEASE_URL:-}" ]; then
  # A mirror or a local directory served over HTTP (used by the tests).
  base=${FORGE_RELEASE_URL%/}
  version=${version:-custom}
elif [ -n "$version" ]; then
  base="https://github.com/$repo/releases/download/$version"
elif probe "https://github.com/$repo/releases/latest/download/SHA256SUMS.txt" 2>/dev/null; then
  base="https://github.com/$repo/releases/latest/download"
  version=latest
else
  base="https://github.com/$repo/releases/download/dev"
  version=dev
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/forge-install.XXXXXX")
chmod 755 "$tmp"   # apt's sandbox user must be able to read the package
trap 'rm -rf "$tmp"' EXIT

if [ "$mode" = deb ]; then asset="forge_amd64.deb"; else asset="forge-linux-$arch.tar.gz"; fi
say "Downloading Forge ($version) from $base"
fetch "$base/SHA256SUMS.txt" "$tmp/SHA256SUMS.txt" \
  || die "release '$version' not found at $base (check https://github.com/$repo/releases)"
fetch "$base/$asset" "$tmp/$asset" || die "could not download $asset"
(cd "$tmp" && grep " $asset\$" SHA256SUMS.txt | sha256sum -c --quiet -) \
  || die "checksum mismatch for $asset"

# ----- install -------------------------------------------------------------------
if [ "$mode" = deb ]; then
  say "Installing the Debian package (sudo may ask for your password)"
  as_root apt-get install -y "$tmp/$asset"
  say "Forge installed: run 'forge' or open it from the applications menu"
else
  tar -xzf "$tmp/$asset" -C "$tmp"
  unpacked=$(find "$tmp" -mindepth 1 -maxdepth 1 -type d -name 'forge-*' | head -n 1)
  [ -n "$unpacked" ] || die "unexpected tarball layout"
  if [ -n "$prefix" ]; then
    if [ -w "$prefix" ] || { [ ! -e "$prefix" ] && [ -w "$(dirname "$prefix")" ]; }; then
      "$unpacked/install.sh" --prefix "$prefix"
    else
      as_root "$unpacked/install.sh" --prefix "$prefix"
    fi
  else
    "$unpacked/install.sh"
  fi
fi

if ! command -v vulkaninfo >/dev/null 2>&1 && [ ! -d /usr/share/vulkan/icd.d ]; then
  say "Note: the GPU renderer needs Vulkan drivers (Debian/Ubuntu: mesa-vulkan-drivers)"
fi
