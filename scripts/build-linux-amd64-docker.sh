#!/bin/sh
set -eu
script_directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
# Builds a GNU/glibc x86_64 executable. This does not replace native tests or
# the release workflow's separate musl builds.
exec bash "$script_directory/docker-linux.sh" build --platform linux/amd64 "$@"
