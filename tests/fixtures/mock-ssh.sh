#!/bin/sh

# Tests link this immutable executable into their own temporary directory.
# Keep state paths quoted and use shell expansion instead of dirname/basename:
# the saved-PATH test deliberately excludes ordinary system commands from PATH.
fixture_dir=${0%/*}
fixture_name=${0##*/}

start_descendant() {
    /bin/sleep 30 &
    printf '%s\n' "$!" > "$fixture_dir/$1"
}

start_escaped_holder() {
    perl -MPOSIX -e 'POSIX::setsid(); open(my $f, q(>), $ARGV[0]) or die $!; print $f "$$\n"; close($f); sleep 30' "$fixture_dir/holder.pid" &
    while [ ! -s "$fixture_dir/holder.pid" ]; do /bin/sleep 0.01; done
}

case "$fixture_name" in
    fake-ssh)
        /bin/cat >/dev/null
        start_descendant normal-descendant.pid
        /bin/cat "$fixture_dir/response.frame"
        ;;
    ssh-proxy-helper)
        /bin/cat >/dev/null
        /bin/cat "$fixture_dir/../response.frame"
        ;;
    fake-ssh-delta)
        /bin/cat >/dev/null
        /bin/cat "$fixture_dir/delta-response.frame"
        ;;
    fake-ssh-stdout-overflow)
        /bin/cat >/dev/null
        start_descendant stdout-descendant.pid
        while :; do printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'; done
        ;;
    fake-ssh-fast-stdout-overflow)
        /bin/cat >/dev/null
        i=0
        while [ "$i" -lt 200 ]; do
            printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'
            i=$((i + 1))
        done
        ;;
    fake-ssh-stderr-overflow)
        /bin/cat >/dev/null
        start_descendant stderr-descendant.pid
        while :; do printf '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef' >&2; done
        ;;
    fake-ssh-hang)
        /bin/cat >/dev/null
        start_descendant timeout-descendant.pid
        wait
        ;;
    fake-ssh-timeout)
        /bin/cat >/dev/null
        /bin/sleep 30
        ;;
    fake-ssh-cancel)
        start_descendant descendant.pid
        /bin/cat >/dev/null
        wait
        ;;
    fake-ssh-escaped-holder)
        /bin/cat >/dev/null
        start_escaped_holder
        exit 0
        ;;
    fake-ssh-cancel-escaped-holder)
        start_escaped_holder
        /bin/cat >/dev/null
        wait
        ;;
    *)
        printf '%s\n' 'unknown SSH fixture alias' >&2
        exit 64
        ;;
esac
