#!/bin/sh

set -eu

ROOT=$(CDPATH= cd "$(dirname "$0")/.." && pwd -P)
INSTALLER="$ROOT/scripts/install.sh"
SYSTEM_PATH=$PATH
REAL_MV=$(command -v mv)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/codex-usage-monit-installer-test.XXXXXX")
TEST_ROOT=$(CDPATH= cd "$TEST_ROOT" && pwd -P)
trap 'rm -rf "$TEST_ROOT"' EXIT HUP INT TERM

unset CODEX_USAGE_MONIT_VERSION
unset CODEX_USAGE_MONIT_INSTALL_DIR
unset CODEX_USAGE_MONIT_PROFILE
unset CODEX_USAGE_MONIT_NO_MODIFY_PATH

fail() {
    printf 'installer test failed: %s\n' "$*" >&2
    exit 1
}

assert_file_contains() {
    grep -F "$2" "$1" >/dev/null 2>&1 \
        || fail "$1 does not contain: $2"
}

fixture_dir="$TEST_ROOT/fixture"
release_dir="$TEST_ROOT/release"
mock_bin="$TEST_ROOT/mock-bin"
temp_dir="$TEST_ROOT/tmp"
mkdir -p "$fixture_dir" "$release_dir" "$mock_bin" "$temp_dir"

cat > "$fixture_dir/codex-usage-monit" <<'EOF'
#!/bin/sh
if [ "${1:-}" = "--version" ]; then
    printf 'codex-usage-monit 0.1.0\n'
    exit 0
fi
printf 'fixture binary\n'
EOF
chmod 0755 "$fixture_dir/codex-usage-monit"

for target in \
    x86_64-apple-darwin \
    aarch64-apple-darwin \
    x86_64-unknown-linux-musl \
    aarch64-unknown-linux-musl
do
    tar -czf "$release_dir/codex-usage-monit-$target.tar.gz" \
        -C "$fixture_dir" codex-usage-monit
done

(
    cd "$release_dir"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum codex-usage-monit-*.tar.gz | sort -k 2 > SHA256SUMS
    else
        shasum -a 256 codex-usage-monit-*.tar.gz | sort -k 2 > SHA256SUMS
    fi
)

cat > "$mock_bin/uname" <<'EOF'
#!/bin/sh
if [ -n "${MOCK_ENV_LOG:-}" ]; then
    printf 'ZDOTDIR=%s\nENV=%s\nBASH_ENV=%s\n' \
        "${ZDOTDIR:-}" "${ENV:-}" "${BASH_ENV:-}" >> "$MOCK_ENV_LOG"
fi
case "${1:-}" in
    -s) printf '%s\n' "$MOCK_UNAME_S" ;;
    -m) printf '%s\n' "$MOCK_UNAME_M" ;;
    *) exit 2 ;;
esac
EOF

cat > "$mock_bin/curl" <<'EOF'
#!/bin/sh
output=
url=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output)
            output=$2
            shift 2
            ;;
        https://*)
            url=$1
            shift
            ;;
        *)
            shift
            ;;
    esac
done
[ -n "$output" ] && [ -n "$url" ] || exit 2
asset=${url##*/}
[ -f "$MOCK_RELEASE_DIR/$asset" ] || exit 22
cp "$MOCK_RELEASE_DIR/$asset" "$output"
printf '%s\n' "$url" >> "$MOCK_CURL_LOG"
if [ -n "${MOCK_SIGNAL_ASSET:-}" ] && [ "$asset" = "$MOCK_SIGNAL_ASSET" ]; then
    kill -TERM "$PPID"
    sleep 1
fi
EOF

cat > "$mock_bin/mv" <<'EOF'
#!/bin/sh
destination=
for argument in "$@"; do
    destination=$argument
done
if [ -n "${MOCK_MV_FAIL_DEST:-}" ] && [ "$destination" = "$MOCK_MV_FAIL_DEST" ]; then
    exit 73
fi
exec "$REAL_MV" "$@"
EOF
chmod 0755 "$mock_bin/uname" "$mock_bin/curl" "$mock_bin/mv"

curl_log="$TEST_ROOT/curl.log"
: > "$curl_log"
mock_release_dir=$release_dir
mock_uname_s=Darwin
mock_uname_m=arm64
mock_shell=/bin/zsh
mock_signal_asset=
mock_mv_fail_dest=
mock_env_log=

run_installer() {
    test_home=$1
    shift
    mkdir -p "$test_home"
    HOME="$test_home" \
    ZDOTDIR="$test_home" \
    ENV= \
    BASH_ENV= \
    SHELL="$mock_shell" \
    TMPDIR="$temp_dir" \
    PATH="$mock_bin:$SYSTEM_PATH" \
    MOCK_RELEASE_DIR="$mock_release_dir" \
    MOCK_CURL_LOG="$curl_log" \
    MOCK_UNAME_S="$mock_uname_s" \
    MOCK_UNAME_M="$mock_uname_m" \
    MOCK_SIGNAL_ASSET="$mock_signal_asset" \
    MOCK_MV_FAIL_DEST="$mock_mv_fail_dest" \
    MOCK_ENV_LOG="$mock_env_log" \
    REAL_MV="$REAL_MV" \
        sh "$INSTALLER" "$@"
}

outside_zdotdir="$TEST_ROOT/outside-zdotdir"
isolated_home="$TEST_ROOT/home-isolated-environment"
inherited_shell_hook="$TEST_ROOT/inherited-shell-hook"
environment_log="$TEST_ROOT/installer-environment.log"
mkdir -p "$outside_zdotdir"
printf 'export KEEP_REAL_PROFILE=yes\n' > "$outside_zdotdir/.zshrc"
printf 'exit 97\n' > "$inherited_shell_hook"
: > "$environment_log"
mock_env_log=$environment_log
ZDOTDIR="$outside_zdotdir" \
ENV="$inherited_shell_hook" \
BASH_ENV="$inherited_shell_hook" \
    run_installer "$isolated_home" >/dev/null
mock_env_log=
assert_file_contains "$isolated_home/.zshrc" "# codex-usage-monit installer"
[ "$(cat "$outside_zdotdir/.zshrc")" = "export KEEP_REAL_PROFILE=yes" ] \
    || fail "inherited ZDOTDIR changed a profile outside the test home"
assert_file_contains "$environment_log" "ZDOTDIR=$isolated_home"
if grep -F "$inherited_shell_hook" "$environment_log" >/dev/null 2>&1; then
    fail "inherited ENV or BASH_ENV reached the installer"
fi

home="$TEST_ROOT/home-default"
mkdir -p "$home"
printf 'export KEEP_ME=yes\n' > "$home/.zshrc"
run_installer "$home" >/dev/null
[ -x "$home/.local/bin/codex-usage-monit" ] || fail "default install is missing"
assert_file_contains "$home/.zshrc" "export KEEP_ME=yes"
assert_file_contains "$home/.zshrc" "# codex-usage-monit installer"
assert_file_contains "$curl_log" "/releases/latest/download/codex-usage-monit-aarch64-apple-darwin.tar.gz"
run_installer "$home" >/dev/null
[ "$(grep -c '# codex-usage-monit installer' "$home/.zshrc")" -eq 1 ] \
    || fail "PATH marker was duplicated"

custom_dir="$home/tools with 'quote"
run_installer "$home" --install-dir "$custom_dir" >/dev/null
[ -x "$custom_dir/codex-usage-monit" ] || fail "custom install is missing"
[ "$(grep -c '# codex-usage-monit installer' "$home/.zshrc")" -eq 1 ] \
    || fail "changed install directory duplicated PATH marker"
resolved_command=$(
    HOME="$home" PATH="$SYSTEM_PATH" /bin/sh -c \
        '. "$HOME/.zshrc"; command -v codex-usage-monit'
)
[ "$resolved_command" = "$custom_dir/codex-usage-monit" ] \
    || fail "profile PATH entry is not valid shell syntax"

bash_home="$TEST_ROOT/home-bash"
mock_shell=/bin/bash
run_installer "$bash_home" >/dev/null
assert_file_contains "$bash_home/.bash_profile" "# codex-usage-monit installer"
[ ! -e "$bash_home/.bashrc" ] || fail "macOS bash unexpectedly changed .bashrc"
mock_shell=/bin/zsh

linux_bash_home="$TEST_ROOT/home-linux-bash"
mock_shell=/bin/bash
mock_uname_s=Linux
mock_uname_m=x86_64
run_installer "$linux_bash_home" >/dev/null
assert_file_contains "$linux_bash_home/.bashrc" "# codex-usage-monit installer"
[ ! -e "$linux_bash_home/.bash_profile" ] \
    || fail "Linux bash unexpectedly changed .bash_profile"
mock_shell=/bin/zsh
mock_uname_s=Darwin
mock_uname_m=arm64

unsupported_home="$TEST_ROOT/home-unsupported-shell"
mock_shell=/usr/local/bin/fish
unsupported_output=$(run_installer "$unsupported_home")
[ -x "$unsupported_home/.local/bin/codex-usage-monit" ] \
    || fail "unsupported shell prevented binary installation"
[ ! -e "$unsupported_home/.profile" ] || fail "unsupported shell changed .profile"
printf '%s\n' "$unsupported_output" | grep -F "not supported automatically" >/dev/null \
    || fail "unsupported shell did not report manual PATH setup"
mock_shell=/bin/zsh

symlink_home="$TEST_ROOT/home-symlink-profile"
mkdir -p "$symlink_home"
printf 'export FROM_DOTFILES=yes\n' > "$symlink_home/dotfiles.zshrc"
ln -s dotfiles.zshrc "$symlink_home/.zshrc"
symlink_output=$(run_installer "$symlink_home")
[ -L "$symlink_home/.zshrc" ] || fail "profile symlink was replaced"
[ "$(cat "$symlink_home/dotfiles.zshrc")" = "export FROM_DOTFILES=yes" ] \
    || fail "profile symlink target was changed"
printf '%s\n' "$symlink_output" | grep -F "is a symbolic link" >/dev/null \
    || fail "profile symlink did not report manual PATH setup"

atomic_home="$TEST_ROOT/home-atomic-profile"
mkdir -p "$atomic_home"
printf 'export ORIGINAL_PROFILE=yes\n' > "$atomic_home/.zshrc"
mock_mv_fail_dest="$atomic_home/.zshrc"
if run_installer "$atomic_home" >/dev/null 2>&1; then
    fail "profile replacement failure unexpectedly succeeded"
fi
[ "$(cat "$atomic_home/.zshrc")" = "export ORIGINAL_PROFILE=yes" ] \
    || fail "failed profile replacement damaged the original"
mock_mv_fail_dest=

version_home="$TEST_ROOT/home-version"
run_installer "$version_home" \
    --version 0.1.0 \
    --install-dir "$version_home/bin" \
    --no-modify-path >/dev/null
[ ! -e "$version_home/.zshrc" ] || fail "--no-modify-path changed a profile"
assert_file_contains "$curl_log" "/releases/download/v0.1.0/codex-usage-monit-aarch64-apple-darwin.tar.gz"

directory_home="$TEST_ROOT/home-directory-destination"
mkdir -p "$directory_home/.local/bin/codex-usage-monit"
if run_installer "$directory_home" --no-modify-path >/dev/null 2>&1; then
    fail "directory install destination unexpectedly succeeded"
fi
[ -d "$directory_home/.local/bin/codex-usage-monit" ] \
    || fail "directory install destination was replaced"

signal_home="$TEST_ROOT/home-signal"
mock_signal_asset=SHA256SUMS
if run_installer "$signal_home" --no-modify-path >/dev/null 2>&1; then
    fail "terminated installer unexpectedly succeeded"
fi
[ ! -e "$signal_home/.local/bin/codex-usage-monit" ] \
    || fail "terminated installer continued to install the binary"
mock_signal_asset=

for platform in \
    "Darwin x86_64 x86_64-apple-darwin" \
    "Linux x86_64 x86_64-unknown-linux-musl" \
    "Linux aarch64 aarch64-unknown-linux-musl"
do
    set -- $platform
    mock_uname_s=$1
    mock_uname_m=$2
    expected_target=$3
    platform_home="$TEST_ROOT/home-$expected_target"
    run_installer "$platform_home" --no-modify-path >/dev/null
    [ -x "$platform_home/.local/bin/codex-usage-monit" ] \
        || fail "install is missing for $expected_target"
    assert_file_contains "$curl_log" "codex-usage-monit-$expected_target.tar.gz"
done

unsafe_fixture="$TEST_ROOT/unsafe-fixture"
unsafe_release="$TEST_ROOT/unsafe-release"
mkdir -p "$unsafe_fixture" "$unsafe_release"
cp "$fixture_dir/codex-usage-monit" "$unsafe_fixture/codex-usage-monit"
printf 'unexpected member\n' > "$unsafe_fixture/extra.txt"
tar -czf "$unsafe_release/codex-usage-monit-aarch64-apple-darwin.tar.gz" \
    -C "$unsafe_fixture" codex-usage-monit extra.txt
(
    cd "$unsafe_release"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum codex-usage-monit-aarch64-apple-darwin.tar.gz > SHA256SUMS
    else
        shasum -a 256 codex-usage-monit-aarch64-apple-darwin.tar.gz > SHA256SUMS
    fi
)
mock_release_dir=$unsafe_release
mock_uname_s=Darwin
mock_uname_m=arm64
unsafe_home="$TEST_ROOT/home-unsafe-archive"
if run_installer "$unsafe_home" --no-modify-path >/dev/null 2>&1; then
    fail "archive with unexpected members unexpectedly succeeded"
fi
[ ! -e "$unsafe_home/.local/bin/codex-usage-monit" ] \
    || fail "unsafe archive installed a binary"

bad_release="$TEST_ROOT/bad-release"
mkdir -p "$bad_release"
cp "$release_dir"/*.tar.gz "$bad_release/"
awk -v name="codex-usage-monit-x86_64-unknown-linux-musl.tar.gz" '
    $2 == name { $1 = "0000000000000000000000000000000000000000000000000000000000000000" }
    { print $1 "  " $2 }
' "$release_dir/SHA256SUMS" > "$bad_release/SHA256SUMS"
mock_release_dir=$bad_release
mock_uname_s=Linux
mock_uname_m=x86_64
preserve_home="$TEST_ROOT/home-preserve"
mkdir -p "$preserve_home/.local/bin"
printf 'existing binary\n' > "$preserve_home/.local/bin/codex-usage-monit"
if run_installer "$preserve_home" --no-modify-path >/dev/null 2>&1; then
    fail "checksum mismatch unexpectedly succeeded"
fi
[ "$(cat "$preserve_home/.local/bin/codex-usage-monit")" = "existing binary" ] \
    || fail "failed install replaced the existing binary"

mock_release_dir=$release_dir
mock_uname_s=Plan9
mock_uname_m=mips
if run_installer "$TEST_ROOT/home-unsupported" --no-modify-path >/dev/null 2>&1; then
    fail "unsupported platform unexpectedly succeeded"
fi

printf 'installer tests passed\n'
