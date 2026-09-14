#!/usr/bin/env bash
set -euo pipefail

# User-scoped installation makes development builds launched through Cargo
# resolve to the same desktop ID and icon as a packaged build.
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(cd -- "${script_dir}/.." && pwd)
data_dir="${XDG_DATA_HOME:-${HOME}/.local/share}"
app_id="dev.forge.Forge"

install -Dm644 "${project_dir}/assets/linux/${app_id}.desktop" \
  "${data_dir}/applications/${app_id}.desktop"
install -Dm644 "${project_dir}/assets/linux/${app_id}.svg" \
  "${data_dir}/icons/hicolor/scalable/apps/${app_id}.svg"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "${data_dir}/applications"
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -f -t "${data_dir}/icons/hicolor" || true
fi

# KDE keeps rendered icons in its own cache keyed by name, so a redesigned
# SVG under the same name keeps showing the old artwork until the cache is
# dropped and the shell reloads it.
cache_dir="${XDG_CACHE_HOME:-${HOME}/.cache}"
rm -f "${cache_dir}/icon-cache.kcache"
if command -v kbuildsycoca6 >/dev/null 2>&1; then
  kbuildsycoca6 --noincremental >/dev/null 2>&1 || true
elif command -v kbuildsycoca5 >/dev/null 2>&1; then
  kbuildsycoca5 --noincremental >/dev/null 2>&1 || true
fi
if [ -n "${KDE_SESSION_VERSION:-}" ] || pgrep -x plasmashell >/dev/null 2>&1; then
  printf 'Plasma: run `plasmashell --replace &` (or log out and in) to reload the icon.\n'
fi

printf 'Installed Forge desktop integration in %s\n' "${data_dir}"
