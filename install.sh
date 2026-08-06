#!/usr/bin/env bash
set -euo pipefail

: "${HOME:?HOME must be set}"

repository="fiorix/chan-ext-mobile-chat"
release_version=${MOBILE_CHAT_VERSION:-latest}
install_root=${MOBILE_CHAT_INSTALL_ROOT:-"${HOME}/.local/lib/mobile-chat"}
chan_home=${CHAN_HOME:-"${HOME}/.chan"}
verbose=0
temporary_dir=""
pending_file=""
config_tmp=""

usage() {
    cat <<'EOF'
Install the Mobile Chat Chan extension from a GitHub release.

Usage: install.sh [options]

Options:
  --version VERSION    Install a release tag instead of the latest release
  --install-root PATH  Install Mobile Chat files under PATH
  --chan-home PATH     Write the extension declaration under PATH/extensions
  -v, --verbose        Show download and installation details
  -h, --help           Show this help

Environment:
  MOBILE_CHAT_VERSION           Release tag, or "latest"
  MOBILE_CHAT_INSTALL_ROOT      Mobile Chat installation root
  CHAN_HOME                     Chan home directory
  MOBILE_CHAT_RELEASE_BASE_URL  Override the release download base URL
EOF
}

log() {
    if ((verbose)); then
        printf 'mobile-chat-install: %s\n' "$*" >&2
    fi
}

die() {
    printf 'mobile-chat-install: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [[ -n "$config_tmp" && -f "$config_tmp" ]]; then
        rm -f -- "$config_tmp"
    fi
    if [[ -n "$pending_file" && -f "$pending_file" ]]; then
        rm -f -- "$pending_file"
    fi
    if [[ -n "$temporary_dir" && -d "$temporary_dir" ]]; then
        rm -rf -- "$temporary_dir"
    fi
}
trap cleanup EXIT

while (($#)); do
    case "$1" in
        --version)
            (($# >= 2)) || die "--version requires a value"
            release_version=$2
            shift 2
            ;;
        --install-root)
            (($# >= 2)) || die "--install-root requires a value"
            install_root=$2
            shift 2
            ;;
        --chan-home)
            (($# >= 2)) || die "--chan-home requires a value"
            chan_home=$2
            shift 2
            ;;
        -v | --verbose)
            verbose=1
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

command -v curl >/dev/null 2>&1 || die "curl is required"

system=$(uname -s)
machine=$(uname -m)
archive_type=tar
executable_name=mobile-chat-extension
windows=0

case "$system" in
    Linux)
        case "$machine" in
            x86_64 | amd64)
                archive_name=mobile-chat-linux-x86_64.tar.gz
                ;;
            aarch64 | arm64)
                archive_name=mobile-chat-linux-aarch64.tar.gz
                ;;
            *)
                die "unsupported Linux architecture: $machine"
                ;;
        esac
        ;;
    Darwin)
        case "$machine" in
            aarch64 | arm64)
                archive_name=mobile-chat-macos-aarch64.tar.gz
                ;;
            *)
                die "unsupported macOS architecture: $machine"
                ;;
        esac
        ;;
    MINGW* | MSYS* | CYGWIN*)
        case "$machine" in
            x86_64 | amd64)
                archive_name=mobile-chat-windows-x86_64.zip
                archive_type=zip
                executable_name=mobile-chat-extension.exe
                windows=1
                ;;
            *)
                die "unsupported Windows architecture: $machine"
                ;;
        esac
        ;;
    *)
        die "unsupported operating system: $system"
        ;;
esac

if [[ -n "${MOBILE_CHAT_RELEASE_BASE_URL:-}" ]]; then
    release_base=${MOBILE_CHAT_RELEASE_BASE_URL%/}
elif [[ "$release_version" == latest ]]; then
    release_base="https://github.com/${repository}/releases/latest/download"
else
    if [[ ! "$release_version" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
        die "invalid release version: $release_version"
    fi
    release_tag=${release_version#v}
    release_base="https://github.com/${repository}/releases/download/v${release_tag}"
fi

temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/mobile-chat-install.XXXXXX")
archive_path="$temporary_dir/$archive_name"
checksums_path="$temporary_dir/SHA256SUMS"
extract_dir="$temporary_dir/extract"
mkdir -p "$extract_dir"

curl_args=(--fail --location --proto '=https,file' --proto-redir '=https')
if ((verbose)); then
    curl_args+=(--verbose)
else
    curl_args+=(--silent --show-error)
fi

download() {
    local name=$1
    local destination=$2
    log "downloading ${release_base}/${name}"
    curl "${curl_args[@]}" --output "$destination" "${release_base}/${name}"
}

download SHA256SUMS "$checksums_path"
download "$archive_name" "$archive_path"

checksum_count=$(
    awk -v name="$archive_name" \
        '$2 == name || $2 == "*" name { count++ } END { print count + 0 }' \
        "$checksums_path"
)
[[ "$checksum_count" == 1 ]] ||
    die "SHA256SUMS does not contain exactly one entry for $archive_name"
expected_checksum=$(
    awk -v name="$archive_name" \
        '$2 == name || $2 == "*" name { print $1 }' \
        "$checksums_path"
)
[[ "$expected_checksum" =~ ^[0-9A-Fa-f]{64}$ ]] || die "invalid checksum for $archive_name"

if command -v sha256sum >/dev/null 2>&1; then
    actual_checksum=$(sha256sum "$archive_path" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
    actual_checksum=$(shasum -a 256 "$archive_path" | awk '{ print $1 }')
elif ((windows)) &&
    command -v powershell.exe >/dev/null 2>&1 &&
    command -v cygpath >/dev/null 2>&1; then
    windows_archive_path=$(cygpath -w "$archive_path")
    # shellcheck disable=SC2016  # PowerShell expands its own environment variable.
    powershell_hash='(Get-FileHash -Algorithm SHA256 -LiteralPath $env:MOBILE_CHAT_HASH_FILE).Hash.ToLowerInvariant()'
    actual_checksum=$(
        MOBILE_CHAT_HASH_FILE="$windows_archive_path" \
            powershell.exe -NoProfile -Command "$powershell_hash" | tr -d '\r'
    )
else
    die "sha256sum or shasum is required"
fi

expected_checksum=$(printf '%s' "$expected_checksum" | tr '[:upper:]' '[:lower:]')
actual_checksum=$(printf '%s' "$actual_checksum" | tr '[:upper:]' '[:lower:]')
[[ "$actual_checksum" == "$expected_checksum" ]] || die "checksum mismatch for $archive_name"
log "verified $archive_name"

if [[ "$archive_type" == tar ]]; then
    tar -xzf "$archive_path" -C "$extract_dir"
elif command -v unzip >/dev/null 2>&1; then
    unzip -q "$archive_path" -d "$extract_dir"
elif command -v powershell.exe >/dev/null 2>&1 &&
    command -v cygpath >/dev/null 2>&1; then
    windows_archive_path=$(cygpath -w "$archive_path")
    windows_extract_dir=$(cygpath -w "$extract_dir")
    # shellcheck disable=SC2016  # PowerShell expands its own environment variables.
    powershell_expand='Expand-Archive -LiteralPath $env:MOBILE_CHAT_ARCHIVE -DestinationPath $env:MOBILE_CHAT_DESTINATION -Force'
    MOBILE_CHAT_ARCHIVE="$windows_archive_path" MOBILE_CHAT_DESTINATION="$windows_extract_dir" \
        powershell.exe -NoProfile -Command "$powershell_expand"
else
    die "unzip or PowerShell is required to extract $archive_name"
fi

payload="$extract_dir/mobile-chat"
for required in \
    "$payload/$executable_name" \
    "$payload/mobile-chat.toml" \
    "$payload/licenses/LICENSE-APACHE"; do
    [[ -f "$required" ]] || die "release archive is missing ${required#"$payload/"}"
done

copy_atomic() {
    local source=$1
    local destination=$2
    local mode=$3
    local destination_dir
    destination_dir=$(dirname "$destination")
    mkdir -p "$destination_dir"
    pending_file=$(mktemp "$destination_dir/.mobile-chat-install.XXXXXX")
    cp -- "$source" "$pending_file"
    chmod "$mode" "$pending_file"
    mv -f -- "$pending_file" "$destination"
    pending_file=""
}

binary_path="$install_root/$executable_name"
copy_atomic "$payload/$executable_name" "$binary_path" 0755
copy_atomic "$payload/licenses/LICENSE-APACHE" "$install_root/licenses/LICENSE-APACHE" 0644

config_dir="$chan_home/extensions"
config_path="$config_dir/mobile-chat.toml"
mkdir -p "$config_dir"
command_path=$binary_path
if ((windows)); then
    command -v cygpath >/dev/null 2>&1 || die "cygpath is required on Windows"
    command_path=$(cygpath -w "$binary_path")
fi
toml_command=${command_path//\\/\\\\}
toml_command=${toml_command//\"/\\\"}
config_tmp=$(mktemp "$config_dir/.mobile-chat.toml.XXXXXX")
{
    printf 'name = "Mobile Chat"\n'
    printf 'command = "%s"\n' "$toml_command"
    printf 'args = []\n'
    printf 'capabilities = ["session-context"]\n'
} >"$config_tmp"
chmod 0644 "$config_tmp"
mv -f -- "$config_tmp" "$config_path"
config_tmp=""

printf 'Installed Mobile Chat at %s\n' "$install_root"
printf 'Wrote the Chan declaration at %s\n' "$config_path"
printf 'Restart Chan to discover the extension.\n'
