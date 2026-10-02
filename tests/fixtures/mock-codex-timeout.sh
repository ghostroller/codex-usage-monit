#!/bin/sh

# Keep stdin/stdout open until the parent closes stdin after the RPC timeout.
while IFS= read -r request; do :; done

# EOF must not race process-group cleanup by letting the fixture exit itself.
# Keep the same PID and process group until the parent terminates this process.
exec sleep 3600
