#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTAINER_NAME="stremio-dev"
DISTROBOX_INI="$PROJECT_DIR/distrobox.ini"

usage() {
    cat <<EOF
Usage: $0 [command]

Commands:
  create       Create or update the distrobox container from distrobox.ini
  enter        Enter the interactive container terminal
  build-server Build libtorrent via vcpkg and compile stream-server
  build-app    Compile stremio-native
  run          Run the stremio-native GUI application from inside the container
  rm           Remove the container
  help         Show this message
EOF
}

cmd="${1:-enter}"

case "$cmd" in
    create)
        PARENT_DIR="$(cd "$PROJECT_DIR/.." && pwd)"
        PARENT_CARGO_CONFIG="$PARENT_DIR/.cargo/config.toml"
        if [[ ! -f "$PARENT_CARGO_CONFIG" ]]; then
            mkdir -p "$PARENT_DIR/.cargo"
            cat << 'EOF' > "$PARENT_CARGO_CONFIG"
[patch."https://github.com/Vidhan31/stremio-core.git"]
stremio-core = { path = "stremio-core" }
EOF
        fi
        distrobox assemble create --file "$DISTROBOX_INI"
        ;;
    enter)
        distrobox enter "$CONTAINER_NAME"
        ;;
    build-server)
        distrobox enter "$CONTAINER_NAME" -- bash -c '
            set -euo pipefail
            export VCPKG_ROOT=/opt/vcpkg
            export VCPKG_INSTALLED_DIR=/opt/vcpkg/installed
            export VCPKGRS_TRIPLET=x64-linux-release

            cd "'"$PROJECT_DIR"'"
            mkdir -p "$VCPKG_INSTALLED_DIR/vcpkg"
            "$VCPKG_ROOT/vcpkg" install \
                --x-install-root="$VCPKG_INSTALLED_DIR" \
                --x-buildtrees-root="$VCPKG_ROOT/downloads/buildtrees" \
                --triplet=x64-linux-release \
                --overlay-triplets="$PROJECT_DIR/triplets" \
                --overlay-ports="$PROJECT_DIR/vcpkg-overlays"
            cargo build --package server
        '
        ;;
    build-app)
        distrobox enter "$CONTAINER_NAME" -- bash -c '
            set -euo pipefail
            export VCPKG_ROOT=/opt/vcpkg
            export VCPKG_INSTALLED_DIR=/opt/vcpkg/installed
            export VCPKGRS_TRIPLET=x64-linux-release
            cd "'"$PROJECT_DIR"'"
            cargo build --package stremio-native
        '
        ;;
    run)
        distrobox enter "$CONTAINER_NAME" -- bash -c '
            set -euo pipefail
            export VCPKG_ROOT=/opt/vcpkg
            export VCPKG_INSTALLED_DIR=/opt/vcpkg/installed
            export VCPKGRS_TRIPLET=x64-linux-release
            cd "'"$PROJECT_DIR"'"
            cargo run --package stremio-native
        '
        ;;
    rm)
        distrobox assemble rm --file "$DISTROBOX_INI"
        ;;
    help|--help|-h)
        usage
        ;;
    *)
        printf "Unknown command: %s\n\n" "$cmd" >&2
        usage >&2
        exit 1
        ;;
esac
