#!/bin/bash
chmod +x /usr/bin/clash-verge-service-install
chmod +x /usr/bin/clash-verge-service-uninstall
chmod +x /usr/bin/clash-verge-service

. /etc/os-release

if [ "$ID" = "deepin" ]; then
    PACKAGE_NAME="$DPKG_MAINTSCRIPT_PACKAGE"
    DESKTOP_FILES=$(dpkg -L "$PACKAGE_NAME" 2>/dev/null | grep "\.desktop$")
    echo "$DESKTOP_FILES" | while IFS= read -r f; do
        if [ "$(basename "$f")" == "Clash Verge.desktop" ]; then
            echo "Fixing deepin desktop file"
            mv -vf "$f" "/usr/share/applications/clash-verge.desktop"
        fi
    done
fi

# Install the service with the app, so TUN needs no authorization later, and replace it when the
# package carries a different service binary or the service is not running. Otherwise only the
# cores are published, and a running core keeps its file until the app restarts it.
service_args=--install-service
if cmp -s /usr/bin/clash-verge-service /var/lib/clash-verge-service/bin/clash-verge-service &&
    systemctl is-enabled --quiet clash-verge-service && systemctl is-active --quiet clash-verge-service; then
    service_args=
fi
/usr/bin/clash-verge-service-install $service_args \
    --install-core /usr/bin/verge-mihomo --install-core /usr/bin/verge-mihomo-alpha ||
    echo "Clash Verge: the service was not installed or updated; it can be installed from the app" >&2
