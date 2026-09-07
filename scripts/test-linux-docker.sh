#!/bin/sh
set -eu
script_directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec bash "$script_directory/docker-linux.sh" verify "$@"
