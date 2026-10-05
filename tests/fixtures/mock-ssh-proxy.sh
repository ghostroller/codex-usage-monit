#!/bin/sh

# The transport canonicalizes this executable after saved-PATH lookup. Its
# helper is deliberately resolved by the same PATH and retains its test link.
exec ssh-proxy-helper "$@"
