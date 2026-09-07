#!/bin/zsh
# Attach a Windows ARM64 installer ISO to an existing UTM VM through AppleScript.

set -euo pipefail

readonly SCRIPT_NAME="${0:t}"

vm_name="codex-usage-monit-windows"
iso_path=""
guest_tools_iso="/Volumes/Drive/codex-usage-monit-windows/iso/utm-guest-tools-latest.iso"
start_after_attach=false

usage() {
    print "Usage: $SCRIPT_NAME --iso PATH [options]"
    print
    print "Attach a Windows 11 ARM64 installer ISO to a stopped UTM VM."
    print
    print "Options:"
    print "  --iso PATH      Windows 11 ARM64 ISO (required)"
    print "  --guest-tools-iso PATH  UTM Windows Guest Tools ISO (default: $guest_tools_iso)"
    print "  --vm-name NAME  UTM VM name (default: $vm_name)"
    print "  --start         Start the VM after attaching the ISO"
    print "  -h, --help      Show this help"
}

fail() {
    print -u2 -- "error: $*"
    exit 1
}

while (( $# > 0 )); do
    case "$1" in
        --iso)
            (( $# >= 2 )) || fail "--iso requires a path."
            iso_path="$2"
            shift 2
            ;;
        --vm-name)
            (( $# >= 2 )) || fail "--vm-name requires a name."
            vm_name="$2"
            shift 2
            ;;
        --guest-tools-iso)
            (( $# >= 2 )) || fail "--guest-tools-iso requires a path."
            guest_tools_iso="$2"
            shift 2
            ;;
        --start)
            start_after_attach=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown option: $1"
            ;;
    esac
done

[[ -n "$iso_path" ]] || fail "--iso is required."
[[ -f "$iso_path" ]] || fail "Windows installer ISO does not exist: $iso_path"
[[ -f "$guest_tools_iso" ]] || fail "UTM Windows Guest Tools ISO does not exist: $guest_tools_iso"
[[ -x "/Applications/UTM.app/Contents/MacOS/UTM" ]] || fail "UTM.app was not found in /Applications."
[[ -n "$vm_name" ]] || fail "--vm-name must not be empty."

iso_path="${iso_path:A}"
guest_tools_iso="${guest_tools_iso:A}"

/usr/bin/osascript - "$vm_name" "$iso_path" "$guest_tools_iso" <<'APPLESCRIPT'
on run argv
    set vmName to item 1 of argv
    set isoPath to item 2 of argv
    set guestToolsPath to item 3 of argv
    set installerISO to POSIX file isoPath
    set guestToolsISO to POSIX file guestToolsPath

    tell application id "com.utmapp.UTM"
        set targetVM to first virtual machine whose name is vmName
        if status of targetVM is not stopped then error "Stop the VM before changing its installer ISO."

        set vmConfiguration to configuration of targetVM
        set driveRecords to drives of vmConfiguration
        set updatedDriveRecords to {}
        set removableDriveCount to 0

        repeat with driveRecord in driveRecords
            set driveId to id of driveRecord
            set driveInterface to interface of driveRecord
            if removable of driveRecord then
                set removableDriveCount to removableDriveCount + 1
                if removableDriveCount is 1 then
                    set end of updatedDriveRecords to {id:driveId, interface:driveInterface, source:installerISO}
                else if removableDriveCount is 2 then
                    set end of updatedDriveRecords to {id:driveId, interface:driveInterface, source:guestToolsISO}
                else
                    set end of updatedDriveRecords to {id:driveId, interface:driveInterface}
                end if
            else
                set end of updatedDriveRecords to {id:driveId, interface:driveInterface}
            end if
        end repeat

        if removableDriveCount is 0 then error "The VM has no removable drive for the Windows installer ISO."
        if removableDriveCount is 1 then set end of updatedDriveRecords to {removable:true, source:guestToolsISO}
        update configuration targetVM with {drives:updatedDriveRecords}
    end tell
end run
APPLESCRIPT

if [[ "$start_after_attach" == true ]]; then
    /usr/bin/osascript - "$vm_name" <<'APPLESCRIPT'
on run argv
    set vmName to item 1 of argv
    tell application id "com.utmapp.UTM"
        set targetVM to first virtual machine whose name is vmName
        start targetVM
    end tell
end run
APPLESCRIPT
fi

print "Attached Windows installer ISO to $vm_name."
if [[ "$start_after_attach" == true ]]; then
    print "UTM has started $vm_name. Complete Windows Setup in the VM window."
fi
