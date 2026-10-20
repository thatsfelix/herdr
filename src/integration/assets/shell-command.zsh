# Explicit opt-in only. Source this file in an interactive Herdr zsh.
# Captures PTY output for a command time window, NOT process-isolated stdout.
# System notifications can expose sensitive output. No argv is transmitted.
# HERDR_COMMAND_NOTIFY_THRESHOLD_MS: integer 0..86400000, default 10000.
# Run herdr_command_notify_disable to unregister these hooks.

[[ -o interactive && -n ${HERDR_PANE_ID-} ]] || return 0
# A named precmd runs before precmd_functions and may replace the exit status.
# Do not wrap or overwrite it: decline support instead.
if (( $+functions[precmd] )); then
  print -u2 -- 'Herdr command notifications not enabled: existing precmd function'
  return 0
fi

_herdr_command_notify_preexec() {
  # Background commands are unsupported. This deliberately also skips && and
  # quoted ampersands rather than guessing at zsh syntax or recording argv.
  if [[ $1 == *'&'* ]]; then
    builtin printf '\033]1337;HerdrCommand;cancel\007'
    return 0
  fi
  # Refuse later named precmd additions and invalid configuration.
  if (( $+functions[precmd] )); then
    builtin printf '\033]1337;HerdrCommand;disable\007'
    unset _herdr_command_notify_enabled
    return 0
  fi
  local threshold=${HERDR_COMMAND_NOTIFY_THRESHOLD_MS:-10000}
  if [[ $threshold != <-> ]] || (( ${#threshold} > 8 )); then
    builtin printf '\033]1337;HerdrCommand;disable\007'
    unset _herdr_command_notify_enabled
    return 0
  fi
  threshold=$(( 10#$threshold ))
  if (( threshold > 86400000 )); then
    builtin printf '\033]1337;HerdrCommand;disable\007'
    unset _herdr_command_notify_enabled
    return 0
  fi
  (( ++_herdr_command_notify_sequence ))
  builtin printf '\033]1337;HerdrCommand;start;%s;%s\007' "$_herdr_command_notify_sequence" "$threshold"
  return 0
}

_herdr_command_notify_precmd() {
  local command_status=$?
  if (( $+functions[precmd] )); then
    builtin printf '\033]1337;HerdrCommand;disable\007'
    unset _herdr_command_notify_enabled
    return 0
  fi
  builtin printf '\033]1337;HerdrCommand;end;%s;%s\007' "$_herdr_command_notify_sequence" "$command_status"
  return 0
}

herdr_command_notify_disable() {
  preexec_functions=(${preexec_functions:#_herdr_command_notify_preexec})
  precmd_functions=(${precmd_functions:#_herdr_command_notify_precmd})
  builtin printf '\033]1337;HerdrCommand;disable\007'
  unset _herdr_command_notify_enabled
}

if [[ ${_herdr_command_notify_enabled-} != 1 ]]; then
  typeset -g _herdr_command_notify_enabled=1
  typeset -gi _herdr_command_notify_sequence=0
  typeset -ga preexec_functions precmd_functions
  preexec_functions=(_herdr_command_notify_preexec ${preexec_functions:#_herdr_command_notify_preexec})
  precmd_functions=(_herdr_command_notify_precmd ${precmd_functions:#_herdr_command_notify_precmd})
  builtin printf '\033]1337;HerdrCommand;enable\007'
fi
