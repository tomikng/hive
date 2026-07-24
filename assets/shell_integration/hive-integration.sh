#!/bin/sh
# Hive shell integration: emits OSC 133 semantic-prompt sequences so Hive can
# detect command boundaries in the terminal (Warp-style command blocks).
#
# Protocol (standard, shared with iTerm2/VS Code/WezTerm shell integration):
#   ESC ] 133 ; A ST   prompt start
#   ESC ] 133 ; B ST   prompt end / command input start
#   ESC ] 133 ; C ST   command output start
#   ESC ] 133 ; D ; <exit-code> ST   command finished
# where ESC = \033 and ST here is BEL (\007), matching the iTerm2 variant of
# the protocol that Hive's parser expects.
#
# This file is sourced automatically by Hive-spawned interactive shells (see
# crates/terminal/src/shell_integration.rs for the injection mechanism). It
# only EMITS the sequences - detecting/rendering them is handled elsewhere.
#
# This script only supports bash and zsh. Other shells are left untouched.

# Guard against being sourced twice in the same shell session.
if [ -n "${HIVE_SHELL_INTEGRATION:-}" ]; then
    return 0 2>/dev/null || exit 0
fi

# Only install hooks in interactive shells.
case "$-" in
    *i*) ;;
    *) return 0 2>/dev/null || exit 0 ;;
esac

HIVE_SHELL_INTEGRATION=1
export HIVE_SHELL_INTEGRATION

__hive_osc133_a() { printf '\033]133;A\007'; }
__hive_osc133_b() { printf '\033]133;B\007'; }
__hive_osc133_c() { printf '\033]133;C\007'; }
__hive_osc133_d() { printf '\033]133;D;%s\007' "$1"; }

# .zshenv/.zprofile/.zlogin are intentionally not sourced: zsh looks them up
# in $ZDOTDIR at process start, which points at Hive's generated temp dir
# (not the user's real dotfiles) until this script's .zshrc restores it.
if [ -n "${ZSH_VERSION:-}" ]; then
    __hive_precmd() {
        # Must be the very first statement so $? reflects the command that
        # just finished, not anything this function does.
        local hive_exit_code=$?
        if [ "${__hive_prompt_shown:-0}" = "1" ]; then
            __hive_osc133_d "$hive_exit_code"
        fi
        __hive_prompt_shown=1
        __hive_osc133_a
        __hive_osc133_b
    }
    __hive_preexec() {
        __hive_osc133_c
    }

    typeset -ga precmd_functions preexec_functions

    # precmd_functions must run __hive_precmd FIRST, not last: this script is
    # sourced after the user's .zshrc, so any precmd hook a framework
    # (oh-my-zsh, powerlevel10k, starship, prezto, ...) already registered
    # would otherwise run before Hive's and reset $? first. add-zsh-hook
    # always appends, so prepend manually instead.
    if [[ ${precmd_functions[(I)__hive_precmd]} -eq 0 ]]; then
        precmd_functions=(__hive_precmd "${precmd_functions[@]}")
    fi

    autoload -Uz add-zsh-hook 2>/dev/null
    if typeset -f add-zsh-hook >/dev/null 2>&1; then
        add-zsh-hook preexec __hive_preexec
    else
        # add-zsh-hook unavailable for some reason: chain manually so we
        # don't clobber hooks the user's rc already registered.
        preexec_functions+=(__hive_preexec)
    fi
elif [ -n "${BASH_VERSION:-}" ]; then
    __hive_precmd() {
        local hive_exit_code=$?
        if [ "${__hive_prompt_shown:-0}" = "1" ]; then
            __hive_osc133_d "$hive_exit_code"
        fi
        __hive_prompt_shown=1
        __hive_preexec_fired=0
        __hive_osc133_a
        __hive_osc133_b
    }
    # Capture any DEBUG trap the user's rc already installed (bash-preexec,
    # command timers, ...) so we can chain onto it below instead of
    # clobbering it, mirroring the PROMPT_COMMAND chaining just below.
    #
    # Prefer $__HIVE_PREV_DEBUG_TRAP if the generated rcfile already set it
    # (see crates/terminal/src/shell_integration.rs): on bash 3.2 (macOS's
    # system /bin/bash), `trap -p DEBUG` reports nothing when queried from
    # here, since this script is normally sourced from a file that is
    # itself sourced (rcfile -> this script) - a known bash 3.2 trap
    # reporting bug at nested source depth. Fall back to self-capture for
    # when this script is sourced directly (not through Hive's rcfile),
    # where that nesting bug doesn't apply.
    if [ "${__HIVE_PREV_DEBUG_TRAP+set}" = "set" ]; then
        __hive_prev_debug_trap=$__HIVE_PREV_DEBUG_TRAP
        unset __HIVE_PREV_DEBUG_TRAP
    else
        # This also deliberately avoids `$(trap -p DEBUG)` / any command
        # that requires a subshell: forking while a DEBUG trap is installed
        # corrupts bash 3.2's trap bookkeeping so the DEBUG trap silently
        # stops firing for the rest of the session. Only builtins
        # (redirection + read) are used to sidestep that.
        __hive_debug_trap_capture_file="${TMPDIR:-/tmp}/hive-debug-trap.$$"
        trap -p DEBUG >"$__hive_debug_trap_capture_file" 2>/dev/null
        IFS= read -r __hive_prev_debug_trap < "$__hive_debug_trap_capture_file"
        rm -f "$__hive_debug_trap_capture_file"
        unset __hive_debug_trap_capture_file
    fi
    __hive_prev_debug_trap=${__hive_prev_debug_trap#"trap -- '"}
    __hive_prev_debug_trap=${__hive_prev_debug_trap%"' DEBUG"}

    __hive_preexec() {
        # Run the previous DEBUG trap (if any) unconditionally, before the
        # dedup below, so we never suppress it.
        if [ -n "${__hive_prev_debug_trap:-}" ]; then
            eval "$__hive_prev_debug_trap"
        fi
        # Skip completion machinery and anything after the first command
        # already reported for this prompt cycle (DEBUG fires per simple
        # command, but OSC 133 C means "the whole command line started").
        [ -n "${COMP_LINE:-}" ] && return
        [ "${__hive_preexec_fired:-0}" = "1" ] && return
        __hive_preexec_fired=1
        __hive_osc133_c
    }

    # Chain onto any PROMPT_COMMAND the user's rc already set, rather than
    # clobbering it.
    case ";${PROMPT_COMMAND:-};" in
        *";__hive_precmd;"*) ;;
        *) PROMPT_COMMAND="__hive_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
    esac

    trap '__hive_preexec' DEBUG
fi
