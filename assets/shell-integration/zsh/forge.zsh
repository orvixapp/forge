# Forge shell integration for zsh: OSC 133 prompt marks and OSC 7 cwd.
# Manual use: source "$FORGE_SHELL_INTEGRATION/zsh/forge.zsh" from .zshrc.

if [[ -z "${_forge_integrated:-}" ]]; then
    _forge_integrated=1
    autoload -Uz add-zsh-hook

    __forge_precmd() {
        local status=$?
        if [[ -n "${_forge_command_running:-}" ]]; then
            printf '\e]133;D;%s\a' "$status"
            unset _forge_command_running
        fi
        printf '\e]7;file://%s%s\a' "${HOST:-localhost}" "$PWD"
        printf '\e]133;A\a'
    }

    __forge_preexec() {
        _forge_command_running=1
        printf '\e]133;C\a'
    }

    add-zsh-hook precmd __forge_precmd
    add-zsh-hook preexec __forge_preexec
    # B: prompt end / command input start, appended to the prompt itself.
    PS1="${PS1}%{$(printf '\e]133;B\a')%}"
fi
