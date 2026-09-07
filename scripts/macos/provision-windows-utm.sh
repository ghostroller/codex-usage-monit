#!/bin/zsh
# Provision an Apple-Silicon Windows test VM through UTM's official AppleScript bridge.

set -euo pipefail

readonly SCRIPT_NAME="${0:t}"

vm_name="codex-usage-monit-windows"
storage_root="/Volumes/Drive/codex-usage-monit-windows"
iso_path=""
guest_tools_iso=""
guest_tools_explicit=false
memory_mib=6144
cpu_cores=4
disk_mib=98304
start_after_provision=false

usage() {
    print "Usage: $SCRIPT_NAME --iso PATH [options]"
    print
    print "Create a Windows 11 ARM64 UTM VM on an external macOS volume."
    print
    print "Options:"
    print "  --iso PATH             Windows 11 ARM64 ISO (required)"
    print "  --guest-tools-iso PATH UTM Windows Guest Tools ISO (downloads official default if absent)"
    print "  --storage-root PATH    External VM root (default: $storage_root)"
    print "  --vm-name NAME         UTM VM name (default: $vm_name)"
    print "  --memory-mib NUMBER    Guest memory in MiB (default: $memory_mib)"
    print "  --cpu-cores NUMBER     Guest CPU cores (default: $cpu_cores)"
    print "  --disk-mib NUMBER      Dynamic system disk size in MiB (default: $disk_mib)"
    print "  --start                Start the VM after provisioning"
    print "  -h, --help             Show this help"
}

fail() {
    print -u2 -- "error: $*"
    exit 1
}

require_positive_integer() {
    local value="$1"
    local option_name="$2"
    [[ "$value" == <-> && "$value" -gt 0 ]] || fail "$option_name must be a positive integer."
}

while (( $# > 0 )); do
    case "$1" in
        --iso)
            (( $# >= 2 )) || fail "--iso requires a path."
            iso_path="$2"
            shift 2
            ;;
        --storage-root)
            (( $# >= 2 )) || fail "--storage-root requires a path."
            storage_root="$2"
            shift 2
            ;;
        --guest-tools-iso)
            (( $# >= 2 )) || fail "--guest-tools-iso requires a path."
            guest_tools_iso="$2"
            guest_tools_explicit=true
            shift 2
            ;;
        --vm-name)
            (( $# >= 2 )) || fail "--vm-name requires a name."
            vm_name="$2"
            shift 2
            ;;
        --memory-mib)
            (( $# >= 2 )) || fail "--memory-mib requires a value."
            memory_mib="$2"
            shift 2
            ;;
        --cpu-cores)
            (( $# >= 2 )) || fail "--cpu-cores requires a value."
            cpu_cores="$2"
            shift 2
            ;;
        --disk-mib)
            (( $# >= 2 )) || fail "--disk-mib requires a value."
            disk_mib="$2"
            shift 2
            ;;
        --start)
            start_after_provision=true
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

[[ -n "$iso_path" ]] || fail "--iso is required. Download the ARM64 ISO to the external volume first."
[[ -f "$iso_path" ]] || fail "Windows installer ISO does not exist: $iso_path"
[[ -x "/Applications/UTM.app/Contents/MacOS/UTM" ]] || fail "UTM.app was not found in /Applications."
[[ -n "$vm_name" && "$vm_name" != "." && "$vm_name" != ".." && "$vm_name" != *[/$'\n'$'\r']* ]] || fail "--vm-name must be a single nonempty directory name."
[[ "$vm_name" != *\\* ]] || fail "--vm-name must not contain path separators."

require_positive_integer "$memory_mib" "--memory-mib"
require_positive_integer "$cpu_cores" "--cpu-cores"
require_positive_integer "$disk_mib" "--disk-mib"

storage_root="${storage_root:A}"
iso_path="${iso_path:A}"
[[ "$storage_root" == /Volumes/* ]] || fail "--storage-root must be on a mounted external volume under /Volumes."
volume_name="${${storage_root#/Volumes/}%%/*}"
volume_root="/Volumes/$volume_name"
[[ -d "$volume_root" ]] || fail "external volume is not mounted: $volume_root"
[[ "$(/usr/bin/stat -f %d "$volume_root")" != "$(/usr/bin/stat -f %d /)" ]] || fail "--storage-root is on the system volume, not a mounted external volume."

bundle_path="$storage_root/utm/$vm_name.utm"
[[ ! -e "$bundle_path" ]] || fail "refusing to overwrite existing VM $bundle_path; use: utmctl start '$vm_name'"

mkdir -p "$storage_root/utm" "$storage_root/logs"

if [[ -z "$guest_tools_iso" ]]; then
    guest_tools_iso="$storage_root/iso/utm-guest-tools-latest.iso"
fi
if [[ ! -f "$guest_tools_iso" && "$guest_tools_explicit" == false ]]; then
    mkdir -p "$storage_root/iso"
    print "Downloading the official UTM Windows Guest Tools ISO to $guest_tools_iso"
    download_temporary="$(mktemp "$storage_root/iso/.guest-tools.XXXXXXXX")"
    trap '[[ -z "${download_temporary:-}" ]] || rm -f -- "$download_temporary"' EXIT
    curl --fail --location --retry 3 --connect-timeout 20 --max-time 900 --output "$download_temporary" https://getutm.app/downloads/utm-guest-tools-latest.iso
    [[ -s "$download_temporary" ]] || fail "UTM Guest Tools download was empty."
    mv -- "$download_temporary" "$guest_tools_iso"
    download_temporary=""
fi
[[ -f "$guest_tools_iso" ]] || fail "UTM Windows Guest Tools ISO does not exist: $guest_tools_iso"
guest_tools_iso="${guest_tools_iso:A}"

staging_id="$(
    /usr/bin/osascript - "$vm_name" "$iso_path" "$guest_tools_iso" "$bundle_path" "$memory_mib" "$cpu_cores" "$disk_mib" <<'APPLESCRIPT'
on run argv
    set vmName to item 1 of argv
    set isoPath to item 2 of argv
    set guestToolsPath to item 3 of argv
    set bundlePath to item 4 of argv
    set memoryMib to (item 5 of argv) as integer
    set cpuCores to (item 6 of argv) as integer
    set diskMib to (item 7 of argv) as integer
    set createdVM to missing value
    set installISO to POSIX file isoPath
    set guestToolsISO to POSIX file guestToolsPath
    set outputBundle to POSIX file bundlePath

    tell application id "com.utmapp.UTM"
        launch
        delay 1
        if (count of (virtual machines whose name is vmName)) > 0 then error "A UTM VM named '" & vmName & "' is already registered."

        try
            set vmConfiguration to {name:vmName, notes:"Native ARM64 Windows test VM for codex-usage-monit", architecture:"aarch64", machine:"virt", memory:memoryMib, cpu cores:cpuCores, hypervisor:true, uefi:true, directory share mode:WebDAV, drives:{{removable:true, source:installISO}, {removable:true, source:guestToolsISO}, {guest size:diskMib, interface:NVMe}}, network interfaces:{{mode:shared}}, displays:{{hardware:"virtio-ramfb", dynamic resolution:true, native resolution:false}}}
            set createdVM to make new virtual machine with properties {backend:qemu, configuration:vmConfiguration}
            export createdVM to outputBundle
            return id of createdVM
        on error errorMessage number errorNumber
            if createdVM is not missing value then
                try
                    delete createdVM
                end try
            end if
            error errorMessage number errorNumber
        end try
    end tell
end run
APPLESCRIPT
)"

[[ -f "$bundle_path/config.plist" ]] || fail "UTM did not produce a valid external VM bundle at $bundle_path."

# UTM's scripting API creates in its sandbox first. After a verified export, remove
# only that newly-created staging VM so its disk does not consume internal storage.
/usr/bin/osascript - "$staging_id" <<'APPLESCRIPT'
on run argv
    set stagingID to item 1 of argv
    tell application id "com.utmapp.UTM"
        set stagingVM to first virtual machine whose id is stagingID
        delete stagingVM
    end tell
end run
APPLESCRIPT

# Opening a .utm package through the application registers the external package in
# place. The AppleScript `import` command is intentionally not used because it
# copies VM bundles back into UTM's internal Documents directory.
/usr/bin/osascript - "$bundle_path" "$vm_name" <<'APPLESCRIPT'
on run argv
    set bundlePath to item 1 of argv
    set vmName to item 2 of argv
    set bundleFile to POSIX file bundlePath
    tell application id "com.utmapp.UTM"
        open bundleFile
        delay 2
        set registeredVM to first virtual machine whose name is vmName
        if status of registeredVM is not stopped then error "The external VM was not registered in the expected stopped state."
    end tell
end run
APPLESCRIPT

if [[ "$start_after_provision" == true ]]; then
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

print "External UTM VM created: $bundle_path"
if [[ "$start_after_provision" == true ]]; then
    print "UTM has started $vm_name. Complete Windows Setup in the VM window."
else
    print "UTM has registered $vm_name. Start the existing VM with: utmctl start '$vm_name'"
fi
