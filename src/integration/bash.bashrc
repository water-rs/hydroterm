# hydroterm shell integration (auto-generated)
# Emits OSC 133 marks (`A` before PS1, `B` at PS1 end, `C` pre-exec via
# PS0, `D` post-exec) and OSC 7 cwd on every prompt.
[ -f /etc/bash.bashrc ] && . /etc/bash.bashrc
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"
__hydro_osc() {
  local s=$?
  printf '\e]133;D;%s\e\\\e]7;file://%s%s\e\\\e]133;A\e\\' "$s" "$HOSTNAME" "$PWD"
}
case ";$PROMPT_COMMAND;" in
  *__hydro_osc*) ;;
  *) PROMPT_COMMAND="__hydro_osc${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
esac
PS0='\[\e]133;C\e\\\]'
PS1='\[\e]133;B\e\\\]'"$PS1"
