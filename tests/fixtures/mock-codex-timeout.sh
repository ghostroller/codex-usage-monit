#!/bin/sh

# Keep stdin/stdout open until the parent closes stdin or kills the process.
# A finite sleep can finish before initialization on a busy runner.
while IFS= read -r request; do :; done
