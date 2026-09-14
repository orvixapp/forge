# Forge shell integration for bash.
#
# Loaded automatically through `--rcfile` when Forge starts bash (see
# `terminal.shell_integration`), or manually with
#   [ -n "$FORGE_SHELL_INTEGRATION" ] && source "$FORGE_SHELL_INTEGRATION/bash/forge.bash"
#
# Emits OSC 133 prompt marks (A: prompt start, B: command start, C: command
# output start, D: command end + exit status) and OSC 7 (current directory).

# When injected via --rcfile, bash skipped the user's own rc files: run them.
if [ -n "${FORGE_BASH_INJECTED:-}" ]; then
    unset FORGE_BASH_INJECTED
    if shopt -q login_shell; then
        [ -r /etc/profile ] && . /etc/profile
        for _forge_rc in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
            if [ -r "$_forge_rc" ]; then . "$_forge_rc"; break; fi
        done
    else
        [ -r /etc/bash.bashrc ] && . /etc/bash.bashrc
        [ -r "$HOME/.bashrc" ] && . "$HOME/.bashrc"
    fi
    unset _forge_rc
fi

# Idempotent: sourcing twice must not double the marks.
if [ -z "${_forge_integrated:-}" ]; then
    _forge_integrated=1
    _forge_last_status=0

    __forge_precmd() {
        _forge_last_status=$?
        # D: end of the previous command, with its exit status.
        if [ -n "${_forge_command_running:-}" ]; then
            printf '\e]133;D;%s\a' "$_forge_last_status"
            unset _forge_command_running
        fi
        printf '\e]7;file://%s%s\a' "${HOSTNAME:-localhost}" "$PWD"
        # A: the prompt starts here; B is appended to PS1 so it lands right
        # where the user types.
        printf '\e]133;A\a'
        return $_forge_last_status
    }

    __forge_preexec() {
        # C: command output starts (PS0 runs after the command is read).
        _forge_command_running=1
        printf '\e]133;C\a'
    }

    if [[ "${PROMPT_COMMAND[*]:-}" != *__forge_precmd* ]]; then
        if [ "${#PROMPT_COMMAND[@]}" -gt 1 ]; then
            PROMPT_COMMAND=(__forge_precmd "${PROMPT_COMMAND[@]}")
        else
            PROMPT_COMMAND="__forge_precmd${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
        fi
    fi
    PS0='$(__forge_preexec)'"${PS0:-}"
    PS1="${PS1}"'\[\e]133;B\a\]'
fi
