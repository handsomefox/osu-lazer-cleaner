#!/usr/bin/env bash
# Checks that a Linux build runs on glibc 2.35 and links nothing a desktop might lack.
#
# The display stack (Wayland, X11, xkbcommon, GL) is loaded with dlopen, so the executable
# starts on a machine that has only some of it. A direct link to any of those libraries, or a
# glibc symbol newer than the floor, would make the binary refuse to start where it used to.
set -euo pipefail

binary=${1:?usage: check-linux-floor.sh <binary>}
floor=GLIBC_2.35

highest=$(objdump -T "$binary" | grep -o 'GLIBC_[0-9][0-9.]*' | sort -uV | tail -n 1)
echo "highest glibc symbol version: ${highest:-none}"
if [[ -n "$highest" && "$(printf '%s\n' "$floor" "$highest" | sort -V | tail -n 1)" != "$floor" ]]; then
    echo "error: $binary needs $highest, above the $floor floor" >&2
    objdump -T "$binary" | grep -F "$highest" >&2
    exit 1
fi

unexpected=0
while read -r library _; do
    case "$library" in
        linux-vdso.so.1 | /lib64/ld-linux-x86-64.so.2 | libc.so.6 | libm.so.6 | libstdc++.so.6 | libgcc_s.so.1) ;;
        *)
            echo "error: $binary links $library" >&2
            unexpected=1
            ;;
    esac
done < <(ldd "$binary")
exit "$unexpected"
