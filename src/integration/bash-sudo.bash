# shell-integration-features: sudo — keep the terminal's env under sudo.
sudo() { command sudo TERM="$TERM" "$@"; }
