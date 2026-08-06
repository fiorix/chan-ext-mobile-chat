#!/usr/bin/env bash
set -euo pipefail

: "${HOME:?HOME must be set}"

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
install_root=${MOBILE_CHAT_INSTALL_ROOT:-"${HOME}/.local/lib/mobile-chat"}
chan_home=${CHAN_HOME:-"${HOME}/.chan"}
binary_path="$install_root/mobile-chat-extension"
licenses_dir="$install_root/licenses"
config_dir="$chan_home/extensions"
config_path="$config_dir/mobile-chat.toml"

cd "$repo_root"
cargo build --locked --release -p mobile-chat-extension

install -d "$install_root" "$licenses_dir" "$config_dir"
install -m 0755 target/release/mobile-chat-extension "$binary_path"
install -m 0644 LICENSE-APACHE "$licenses_dir/LICENSE-APACHE"

toml_command=${binary_path//\\/\\\\}
toml_command=${toml_command//\"/\\\"}
config_tmp=$(mktemp "$config_path.tmp.XXXXXX")
trap 'rm -f -- "$config_tmp"' EXIT
printf 'name = "Mobile Chat"\ncommand = "%s"\nargs = []\ncapabilities = ["session-context"]\n' \
    "$toml_command" >"$config_tmp"
chmod 0644 "$config_tmp"
mv -f -- "$config_tmp" "$config_path"
trap - EXIT

printf 'Installed Mobile Chat at %s\nWrote the Chan declaration at %s\nRestart Chan to discover it.\n' \
    "$install_root" "$config_path"
