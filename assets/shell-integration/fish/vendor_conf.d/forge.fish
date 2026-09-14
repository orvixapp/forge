# Forge shell integration for fish: OSC 133 prompt marks and OSC 7 cwd.
# Loaded via XDG_DATA_DIRS when Forge starts fish; harmless elsewhere.

if set -q _forge_integrated
    exit
end
set -g _forge_integrated 1

function __forge_precmd --on-event fish_prompt
    set -l last_status $status
    if set -q _forge_command_running
        printf '\e]133;D;%s\a' $last_status
        set -e _forge_command_running
    end
    printf '\e]7;file://%s%s\a' (hostname) "$PWD"
    printf '\e]133;A\a'
end

function __forge_preexec --on-event fish_preexec
    set -g _forge_command_running 1
    printf '\e]133;C\a'
end

# B: command input starts after the prompt is printed.
functions -q fish_prompt; and functions -c fish_prompt __forge_original_prompt
function fish_prompt
    __forge_original_prompt
    printf '\e]133;B\a'
end
