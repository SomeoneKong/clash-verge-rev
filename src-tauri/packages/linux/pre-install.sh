#!/bin/bash
# Before anything is written, an install the update component does not run records itself, so the
# component's recovery of an interrupted update does not undo it.
root=/var/lib/clash-verge-updater
if [ -z "$CLASH_VERGE_UPDATE_TXN" ] && [ -d "$root" ]; then
    if ! printf '%s\tdeb\t%s\n' "$(date -Iseconds)" "${2:-}" >>"$root/external-installs"; then
        echo "Clash Verge: cannot record this installation for the update component" >&2
        exit 1
    fi
fi
