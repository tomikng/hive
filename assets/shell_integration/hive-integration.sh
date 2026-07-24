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

    autoload -Uz add-zsh-hook 2>/dev/null
    if typeset -f add-zsh-hook >/dev/null 2>&1; then
        add-zsh-hook precmd __hive_precmd
        add-zsh-hook preexec __hive_preexec
    else
        # add-zsh-hook unavailable for some reason: chain manually so we
        # don't clobber hooks the user's rc already registered.
        typeset -ga precmd_functions preexec_functions
        precmd_functions+=(__hive_precmd)
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
    __hive_preexec() {
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
