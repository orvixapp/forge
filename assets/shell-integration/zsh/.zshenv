# Forge shell integration bootstrap for zsh.
#
# Forge starts zsh with ZDOTDIR pointing here; this file restores the user's
# ZDOTDIR, runs their real .zshenv and then registers the prompt hooks. The
# user's .zshrc still loads normally afterwards.

if [[ -n "${FORGE_ZDOTDIR_ORIGINAL:-}" ]]; then
    export ZDOTDIR="$FORGE_ZDOTDIR_ORIGINAL"
    unset FORGE_ZDOTDIR_ORIGINAL
else
    unset ZDOTDIR
fi
[[ -r "${ZDOTDIR:-$HOME}/.zshenv" ]] && source "${ZDOTDIR:-$HOME}/.zshenv"
[[ -n "${FORGE_SHELL_INTEGRATION:-}" ]] && source "$FORGE_SHELL_INTEGRATION/zsh/forge.zsh"
