#!/usr/bin/env bash
# Package every target, serve the archives over file://, and run the real
# install.sh against each platform with uname and cygpath faked. Checks that a
# corrupt archive is refused and installs nothing.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/mobile-chat-install-test.XXXXXX")
trap 'rm -rf -- "$temporary_dir"' EXIT

release_dir="$temporary_dir/release"
binary="$temporary_dir/mobile-chat-extension"
fake_bin="$temporary_dir/bin"
mkdir -p "$release_dir" "$fake_bin"
printf '#!/usr/bin/env sh\nprintf "test binary\\n"\n' >"$binary"
chmod 0755 "$binary"

for target in linux-x86_64 linux-aarch64 windows-x86_64 macos-aarch64; do
    python3 "$repo_root/scripts/package-chan-extension.py" \
        --target "$target" \
        --binary "$binary" \
        --output-dir "$release_dir" >/dev/null
done
(
    cd "$release_dir"
    sha256sum mobile-chat-* >SHA256SUMS
)

# shellcheck disable=SC2016  # The generated helper expands these variables.
printf '%s\n' \
    '#!/usr/bin/env bash' \
    'case "${1:-}" in' \
    '    -s) printf "%s\n" "${MOBILE_CHAT_TEST_SYSTEM:?}" ;;' \
    '    -m) printf "%s\n" "${MOBILE_CHAT_TEST_MACHINE:?}" ;;' \
    '    *) exit 2 ;;' \
    'esac' >"$fake_bin/uname"
printf '%s\n' \
    '#!/usr/bin/env bash' \
    'printf "%s\n" "C:\\mobile-chat\\mobile-chat-extension.exe"' >"$fake_bin/cygpath"
chmod 0755 "$fake_bin/uname" "$fake_bin/cygpath"

run_install() {
    local label=$1
    local system=$2
    local machine=$3
    local executable=$4
    local install_root="$temporary_dir/$label/.local/lib/mobile-chat"
    local chan_home="$temporary_dir/$label/.chan"

    PATH="$fake_bin:$PATH" \
        MOBILE_CHAT_TEST_SYSTEM="$system" \
        MOBILE_CHAT_TEST_MACHINE="$machine" \
        MOBILE_CHAT_RELEASE_BASE_URL="file://$release_dir" \
        MOBILE_CHAT_INSTALL_ROOT="$install_root" \
        CHAN_HOME="$chan_home" \
        "$repo_root/install.sh" >/dev/null

    cmp "$binary" "$install_root/$executable"
    [[ -x "$install_root/$executable" ]]
    [[ -f "$install_root/licenses/LICENSE-APACHE" ]]
    grep -Fq 'name = "Mobile Chat"' "$chan_home/extensions/mobile-chat.toml"
    grep -Fq 'capabilities = ["session-context"]' "$chan_home/extensions/mobile-chat.toml"
}

run_install linux-x86_64 Linux x86_64 mobile-chat-extension
run_install linux-aarch64 Linux aarch64 mobile-chat-extension
run_install macos-aarch64 Darwin arm64 mobile-chat-extension
run_install windows-x86_64 MINGW64_NT-10.0 x86_64 mobile-chat-extension.exe

# The declaration must carry an absolute command, in the target platform's own
# path syntax, because Chan spawns it with the extensions directory as cwd.
grep -Fq \
    "command = \"$temporary_dir/linux-x86_64/.local/lib/mobile-chat/mobile-chat-extension\"" \
    "$temporary_dir/linux-x86_64/.chan/extensions/mobile-chat.toml"
grep -Fq \
    'command = "C:\\mobile-chat\\mobile-chat-extension.exe"' \
    "$temporary_dir/windows-x86_64/.chan/extensions/mobile-chat.toml"

# An unsupported platform is refused rather than guessed at.
if PATH="$fake_bin:$PATH" \
    MOBILE_CHAT_TEST_SYSTEM=Linux \
    MOBILE_CHAT_TEST_MACHINE=riscv64 \
    MOBILE_CHAT_RELEASE_BASE_URL="file://$release_dir" \
    MOBILE_CHAT_INSTALL_ROOT="$temporary_dir/riscv-install" \
    CHAN_HOME="$temporary_dir/riscv-chan" \
    "$repo_root/install.sh" >/dev/null 2>"$temporary_dir/riscv.stderr"; then
    printf 'unsupported architecture unexpectedly installed\n' >&2
    exit 1
fi
grep -Fq 'unsupported Linux architecture' "$temporary_dir/riscv.stderr"

# A tampered archive must fail the checksum and leave nothing behind.
printf 'corrupt' >>"$release_dir/mobile-chat-linux-x86_64.tar.gz"
if PATH="$fake_bin:$PATH" \
    MOBILE_CHAT_TEST_SYSTEM=Linux \
    MOBILE_CHAT_TEST_MACHINE=x86_64 \
    MOBILE_CHAT_RELEASE_BASE_URL="file://$release_dir" \
    MOBILE_CHAT_INSTALL_ROOT="$temporary_dir/bad-install" \
    CHAN_HOME="$temporary_dir/bad-chan" \
    "$repo_root/install.sh" >"$temporary_dir/bad.stdout" 2>"$temporary_dir/bad.stderr"; then
    printf 'corrupt archive unexpectedly installed\n' >&2
    exit 1
fi
grep -Fq 'checksum mismatch' "$temporary_dir/bad.stderr"
[[ ! -e "$temporary_dir/bad-install/mobile-chat-extension" ]]

printf 'install-release: all platforms installed, bad inputs refused\n'
