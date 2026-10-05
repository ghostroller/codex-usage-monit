#!/bin/sh

# Validate request order without sleeps or external commands. Invalid requests
# close stdout so the caller reports a protocol failure instead of timing out.
expect_request() {
    IFS= read -r request || exit 1
    case "$request" in
        *"\"method\":\"$1\""*) ;;
        *) printf '%s\n' 'unexpected fixture request method' >&2; exit 2 ;;
    esac
    if [ -n "$2" ]; then
        case "$request" in
            *"\"id\":$2,"*|*"\"id\":$2}"*) ;;
            *) printf '%s\n' 'unexpected fixture request id' >&2; exit 3 ;;
        esac
    else
        case "$request" in
            *'"id":'*) printf '%s\n' 'fixture notification has an id' >&2; exit 4 ;;
        esac
    fi
}

expect_request initialize 1
printf '%s\n' '{"id":1,"result":{}}'
expect_request initialized ''
expect_request account/rateLimits/read 2
printf '%s\n' '{"id":2,"result":{"rateLimits":{"limitId":"independent-deadline-fixture","primary":null}}}'
expect_request account/usage/read 3
printf '%s\n' '{"id":3,"result":{"summary":{},"dailyUsageBuckets":[]}}'
