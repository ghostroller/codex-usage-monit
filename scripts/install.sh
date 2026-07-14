#!/bin/sh

set -eu

LC_ALL=C
export LC_ALL
umask 077

REPOSITORY="ghostroller/codex-usage-monit"
BINARY_NAME="codex-usage-monit"
PATH_MARKER="# codex-usage-monit installer"

VERSION=${CODEX_USAGE_MONIT_VERSION:-latest}
INSTALL_DIR=${CODEX_USAGE_MONIT_INSTALL_DIR:-${HOME:-}/.local/bin}
PROFILE_OVERRIDE=${CODEX_USAGE_MONIT_PROFILE:-}
MODIFY_PATH=1
case ${CODEX_USAGE_MONIT_NO_MODIFY_PATH:-0} in
    1 | true | yes) MODIFY_PATH=0 ;;
esac

usage() {
    cat <<'EOF'
Install codex-usage-monit from GitHub Releases.

Usage:
  install.sh [--version latest|[v]X.Y.Z]
             [--install-dir DIR]
             [--no-modify-path]
             [--help]

Environment overrides:
  CODEX_USAGE_MONIT_VERSION
  CODEX_USAGE_MONIT_INSTALL_DIR
  CODEX_USAGE_MONIT_PROFILE
  CODEX_USAGE_MONIT_NO_MODIFY_PATH=1
EOF
}

die() {
    printf 'codex-usage-monit installer: %s\n' "$*" >&2
    exit 1
}

info() {
    printf '%s\n' "$*"
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

require_value() {
    [ "$#" -ge 2 ] || die "missing value for $1"
    [ -n "$2" ] || die "empty value for $1"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            require_value "$@"
            VERSION=$2
            shift 2
            ;;
        --install-dir)
            require_value "$@"
            INSTALL_DIR=$2
            shift 2
            ;;
        --no-modify-path)
            MODIFY_PATH=0
            shift
            ;;
        --help | -h)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

[ -n "${HOME:-}" ] || die "HOME is not set"
[ -n "$INSTALL_DIR" ] || die "install directory is empty"

carriage_return=$(printf '\r')
case "$INSTALL_DIR" in
    *:*) die "install directory cannot contain ':' because it will be added to PATH" ;;
    *"$carriage_return"* | *'
'*) die "install directory cannot contain line breaks" ;;
esac
case "$PROFILE_OVERRIDE" in
    *"$carriage_return"* | *'
'*) die "profile path cannot contain line breaks" ;;
esac

case "$VERSION" in
    latest)
        RELEASE_PATH="releases/latest/download"
        ;;
    *)
        normalized_version=${VERSION#v}
        if ! printf '%s\n' "$normalized_version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
            die "version must be latest or [v]X.Y.Z"
        fi
        VERSION="v$normalized_version"
        RELEASE_PATH="releases/download/$VERSION"
        ;;
esac

os=$(uname -s)
arch=$(uname -m)
case "$os:$arch" in
    Darwin:x86_64 | Darwin:amd64)
        target="x86_64-apple-darwin"
        ;;
    Darwin:arm64 | Darwin:aarch64)
        target="aarch64-apple-darwin"
        ;;
    Linux:x86_64 | Linux:amd64)
        target="x86_64-unknown-linux-musl"
        ;;
    Linux:arm64 | Linux:aarch64)
        target="aarch64-unknown-linux-musl"
        ;;
    *)
        die "unsupported platform: $os $arch"
        ;;
esac

for command_name in awk cat chmod cp curl dirname grep mkdir mktemp mv pwd rm sed tar uname; do
    require_command "$command_name"
done
if command -v sha256sum >/dev/null 2>&1; then
    checksum_tool=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    checksum_tool=shasum
else
    die "required command not found: sha256sum or shasum"
fi

asset="$BINARY_NAME-$target.tar.gz"
base_url="https://github.com/$REPOSITORY/$RELEASE_PATH"
temp_root=${TMPDIR:-/tmp}
work_dir=
staged_binary=
profile_temp=

cleanup() {
    [ -z "$profile_temp" ] || rm -f "$profile_temp"
    [ -z "$staged_binary" ] || rm -f "$staged_binary"
    [ -z "$work_dir" ] || rm -rf "$work_dir"
}
trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

work_dir=$(mktemp -d "$temp_root/codex-usage-monit.XXXXXX")
archive="$work_dir/$asset"
checksums="$work_dir/SHA256SUMS"

download() {
    destination=$1
    url=$2
    curl \
        --proto '=https' \
        --tlsv1.2 \
        --fail \
        --location \
        --silent \
        --show-error \
        --retry 3 \
        --output "$destination" \
        "$url"
}

info "Downloading $asset from $REPOSITORY..."
download "$archive" "$base_url/$asset"
download "$checksums" "$base_url/SHA256SUMS"

if ! expected_checksum=$(awk -v name="$asset" '
    $2 == name {
        if (length($1) != 64 || $1 ~ /[^0-9A-Fa-f]/) exit 2
        count += 1
        digest = tolower($1)
    }
    END {
        if (count == 1) print digest
        else exit 1
    }
' "$checksums"); then
    die "SHA256SUMS does not contain exactly one valid checksum for $asset"
fi

case "$checksum_tool" in
    sha256sum)
        actual_checksum=$(sha256sum "$archive" | awk '{ print tolower($1) }')
        ;;
    shasum)
        actual_checksum=$(shasum -a 256 "$archive" | awk '{ print tolower($1) }')
        ;;
esac
[ "$actual_checksum" = "$expected_checksum" ] || die "checksum verification failed for $asset"

if ! archive_members=$(tar -tzf "$archive"); then
    die "could not read $asset"
fi
[ "$archive_members" = "$BINARY_NAME" ] || die "release archive has unexpected members"
archive_details=$(tar -tvzf "$archive") || die "could not inspect $asset"
case "$archive_details" in
    -*) ;;
    *) die "release archive member is not a regular file" ;;
esac

extract_dir="$work_dir/extract"
mkdir -p "$extract_dir"
tar -xzf "$archive" -C "$extract_dir" "$BINARY_NAME"
extracted_binary="$extract_dir/$BINARY_NAME"
[ -f "$extracted_binary" ] && [ ! -L "$extracted_binary" ] \
    || die "release archive did not contain a regular binary"
chmod 0755 "$extracted_binary"
"$extracted_binary" --version >/dev/null \
    || die "downloaded binary failed its version check"

case "$INSTALL_DIR" in
    /*) ;;
    *) INSTALL_DIR="$(pwd -P)/$INSTALL_DIR" ;;
esac
mkdir -p "$INSTALL_DIR"
INSTALL_DIR=$(cd "$INSTALL_DIR" && pwd -P)
destination="$INSTALL_DIR/$BINARY_NAME"
if [ -d "$destination" ]; then
    die "install destination is a directory: $destination"
fi
if [ -e "$destination" ] && [ ! -f "$destination" ] && [ ! -L "$destination" ]; then
    die "install destination is not a regular file: $destination"
fi
staged_binary=$(mktemp "$INSTALL_DIR/.codex-usage-monit.XXXXXX")
cp "$extracted_binary" "$staged_binary"
chmod 0755 "$staged_binary"
mv -f "$staged_binary" "$destination"
staged_binary=

quote_shell_word() {
    escaped=$(printf '%s' "$1" | sed "s/'/'\"'\"'/g")
    printf "'%s'" "$escaped"
}

path_export="export PATH=$(quote_shell_word "$INSTALL_DIR"):\$PATH  $PATH_MARKER"
manual_path_hint=0

if [ "$MODIFY_PATH" -eq 0 ]; then
    path_status="PATH was not modified (--no-modify-path)."
elif case ":${PATH:-}:" in *":$INSTALL_DIR:"*) true ;; *) false ;; esac; then
    path_status="$INSTALL_DIR is already in PATH."
else
    if [ -n "$PROFILE_OVERRIDE" ]; then
        profile=$PROFILE_OVERRIDE
    else
        shell_name=${SHELL:-}
        shell_name=${shell_name##*/}
        case "$shell_name" in
            zsh) profile="${ZDOTDIR:-$HOME}/.zshrc" ;;
            bash)
                if [ "$os" = Darwin ]; then
                    if [ -e "$HOME/.bash_profile" ]; then
                        profile="$HOME/.bash_profile"
                    elif [ -e "$HOME/.bash_login" ]; then
                        profile="$HOME/.bash_login"
                    elif [ -e "$HOME/.profile" ]; then
                        profile="$HOME/.profile"
                    else
                        profile="$HOME/.bash_profile"
                    fi
                else
                    profile="$HOME/.bashrc"
                fi
                ;;
            sh | dash | ksh) profile="$HOME/.profile" ;;
            *) profile= ;;
        esac
    fi
    if [ -z "$profile" ]; then
        shell_name=${SHELL:-unknown}
        shell_name=${shell_name##*/}
        path_status="PATH was not modified because shell '$shell_name' is not supported automatically."
        manual_path_hint=1
    else
        case "$profile" in
            /*) ;;
            *) profile="$(pwd -P)/$profile" ;;
        esac
        profile_parent=$(dirname "$profile")
        mkdir -p "$profile_parent"
        if [ -L "$profile" ]; then
            path_status="PATH was not modified because $profile is a symbolic link."
            manual_path_hint=1
        else
            [ ! -e "$profile" ] || [ -f "$profile" ] \
                || die "profile path is not a regular file: $profile"
            profile_temp=$(mktemp "$profile_parent/.codex-usage-monit-profile.XXXXXX")
            if [ -f "$profile" ]; then
                cp -p "$profile" "$profile_temp"
            fi
            marker_written=0
            {
                if [ -f "$profile" ]; then
                    while IFS= read -r line || [ -n "$line" ]; do
                        case "$line" in
                            *"$PATH_MARKER"*)
                                if [ "$marker_written" -eq 0 ]; then
                                    printf '%s\n' "$path_export"
                                    marker_written=1
                                fi
                                ;;
                            *)
                                printf '%s\n' "$line"
                                ;;
                        esac
                    done < "$profile"
                fi
                if [ "$marker_written" -eq 0 ]; then
                    printf '%s\n' "$path_export"
                fi
            } > "$profile_temp"
            mv -f "$profile_temp" "$profile"
            profile_temp=
            path_status="Added $INSTALL_DIR to PATH in $profile."
        fi
    fi
fi

info "Installed $BINARY_NAME to $destination"
info "$path_status"
if ! case ":${PATH:-}:" in *":$INSTALL_DIR:"*) true ;; *) false ;; esac; then
    if [ "$manual_path_hint" -eq 1 ]; then
        info "Add $INSTALL_DIR to your shell's PATH."
    else
        info "Restart your shell, or run: export PATH=$(quote_shell_word "$INSTALL_DIR"):\$PATH"
    fi
fi
