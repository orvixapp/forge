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

printf 'Installed Forge desktop integration in %s\n' "${data_dir}"
