#!/bin/sh
# nvram <label>: print the /dev/uioN device of an NVRAM label, as the
# guest-tools `nvram` helper does (its binary is glibc-only). The CLI's init
# lines run `dev=$(nvram <label>)` before chmod/chown of every NVRAM.
# Lookup (labelinfo.c): alias -> /uio@<hex> node -> its reg start -> the
# platform device <start>.uio owns exactly one uio device.
set -eu
fail() { echo "nvram: $*" >&2; exit 1; }
[ $# -eq 1 ] || fail "usage: nvram <label>"
alias=/proc/device-tree/aliases/$1
[ -r "$alias" ] || fail "no such label '$1'"
node=$(tr -d '\0' < "$alias")
case $node in /uio@*) ;; *) fail "label '$1' is not an NVRAM ($node)" ;; esac
start=$(od -An -v -tx1 -N8 "/proc/device-tree$node/reg" | tr -d ' \n')
[ ${#start} -eq 16 ] || fail "cannot read reg of $node"
set -- "/sys/devices/platform/$(printf '%x' "0x$start").uio/uio"/uio*
[ $# -eq 1 ] && [ -e "$1" ] || fail "cannot find the uio device of $node"
echo "/dev/${1##*/}"
