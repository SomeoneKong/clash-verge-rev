#!/usr/bin/env bash
# Installs or upgrades Clash Verge from the releases of this fork, on Debian,
# Ubuntu and their derivatives:
#
#   curl -fsSL https://github.com/SomeoneKong/clash-verge-rev/releases/latest/download/install-linux.sh | bash
#
# Installs the latest release, or the one whose tag is the first argument
# (`| bash -s -- v2.5.10-custom.2`).
set -euo pipefail

REPO="SomeoneKong/clash-verge-rev"
PACKAGE="clash-verge"
SERVICE_UNIT="clash-verge-service.service"
CORE="/usr/bin/verge-mihomo"

fail() {
    echo "Installation failed: $*" >&2
    exit 1
}

has_service() {
    local state
    state="$(systemctl show --property=LoadState --value "$SERVICE_UNIT" 2>/dev/null || true)"
    [ -n "$state" ] && [ "$state" != "not-found" ]
}

package_state() {
    dpkg-query -W -f='${Status}|${Version}' "$PACKAGE" 2>/dev/null || true
}

core_version() {
    local version
    version="$("$CORE" -v 2>/dev/null | awk 'NR == 1 { print $3 }')" || true
    echo "${version:-(not found)}"
}

main() {
    command -v apt-get >/dev/null || fail "this script needs apt-get (Debian, Ubuntu or a derivative)"
    command -v curl >/dev/null || fail "curl is not installed"

    local arch
    arch="$(dpkg --print-architecture)"
    case "$arch" in
    amd64 | arm64) ;;
    *) fail "no package is published for the $arch architecture" ;;
    esac

    local tag="${1:-}"
    if [ -z "$tag" ]; then
        local latest
        latest="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")" ||
            fail "could not look up the latest release"
        tag="${latest##*/}"
    fi
    case "$tag" in
    v*-custom.*) ;;
    *) fail "\"$tag\" is not a release tag of $REPO" ;;
    esac

    local version="${tag#v}"
    local deb="Clash.Verge_${version}_${arch}.deb"
    local url="https://github.com/$REPO/releases/download/$tag"

    local previous=""
    case "$(package_state)" in
    "install ok installed|"*) previous="$(package_state)" && previous="${previous#*|}" ;;
    esac
    if [ "$previous" = "$version" ]; then
        echo "Clash Verge $version is already installed, with mihomo $(core_version)."
        return
    fi

    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT
    # apt-get reads a local package as its unprivileged _apt user
    chmod 755 "$work"

    echo "Downloading $deb"
    curl -fL --progress-bar -o "$work/$deb" "$url/$deb" || fail "could not download $url/$deb"
    curl -fsSL -o "$work/checksums.txt" "$url/checksums.txt" || fail "could not download $url/checksums.txt"
    (cd "$work" && awk -v name="$deb" '$2 == name' checksums.txt | sha256sum --check --status) ||
        fail "$deb does not match its checksum"
    chmod 644 "$work/$deb"

    local sudo=""
    [ "$(id -u)" -eq 0 ] || sudo="sudo"
    echo "Installing Clash Verge $version"
    $sudo apt-get install -y --allow-downgrades "$work/$deb" || fail "apt-get could not install $deb"

    [ "$(package_state)" = "install ok installed|$version" ] ||
        fail "$PACKAGE $version is not installed after apt-get finished"

    echo
    echo "Clash Verge $version installed successfully${previous:+ (upgraded from $previous)}, with mihomo $(core_version)."
    # the package installs the service for the user who ran sudo; run directly as root, it has none
    if ! has_service; then
        echo "The Clash Verge service is not installed. To use service mode or TUN, install it in Settings."
    fi
    if pgrep -x clash-verge >/dev/null 2>&1; then
        echo "The previous version is still running: quit Clash Verge from the tray and start it again."
    fi
}

main "$@"
