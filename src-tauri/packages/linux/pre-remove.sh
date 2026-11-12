#!/bin/bash
# An upgrade keeps the service; the new package's post-install script updates it.
case "$1" in
upgrade | failed-upgrade) ;;
*) /usr/bin/clash-verge-service-uninstall ;;
esac

. /etc/os-release

if [ "$ID" = "deepin" ]; then
    if [ -f "/usr/share/applications/clash-verge.desktop" ]; then
        echo "Removing deepin desktop file"
        rm -vf "/usr/share/applications/clash-verge.desktop"
    fi
fi

