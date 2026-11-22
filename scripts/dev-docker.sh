#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE_NAME="stremio-fedora44-dev:local"

STREAM_SERVER_GIT_URL="$(sed -n 's/^[[:space:]]*stream-server = { git = "\([^"]*\)".*/\1/p' "$PROJECT_DIR/Cargo.toml")"
STREAM_SERVER_REV="$(sed -n 's/^[[:space:]]*stream-server = { git = "[^"]*", rev = "\([^"]*\)".*/\1/p' "$PROJECT_DIR/Cargo.toml")"

if [[ -z "$STREAM_SERVER_GIT_URL" || -z "$STREAM_SERVER_REV" ]]; then
    printf 'Could not read the pinned Stream Server Git dependency from Cargo.toml.\n' >&2
    exit 1
fi

GENERATED_SLINT_IMPORTS="$PROJECT_DIR/app/ui/imported_fonts.slint"
if ! printf '%s\n' \
    '// Generated automatically by build.rs. DO NOT EDIT.' \
    'export component Fonts {}' \
    | cmp -s - "$GENERATED_SLINT_IMPORTS"; then
    printf '%s\n' \
        '// Generated automatically by build.rs. DO NOT EDIT.' \
        'export component Fonts {}' > "$GENERATED_SLINT_IMPORTS"
fi

docker build \
    --build-arg "DEV_UID=$(id -u)" \
    --build-arg "DEV_GID=$(id -g)" \
    --file "$PROJECT_DIR/Dockerfile.dev" \
    --tag "$IMAGE_NAME" \
    "$PROJECT_DIR"

docker run --rm -it \
    --name stremio-fedora44-dev \
    --volume "$PROJECT_DIR:/work/stremio-native:Z" \
    --volume stremio-dev-cargo-registry:/home/dev/.cargo/registry \
    --volume stremio-dev-cargo-git:/home/dev/.cargo/git \
    --volume stremio-dev-cargo-target:/home/dev/.cargo/target \
    --volume stremio-dev-vcpkg-downloads:/home/dev/vcpkg/downloads \
    --env "STREAM_SERVER_GIT_URL=$STREAM_SERVER_GIT_URL" \
    --env "STREAM_SERVER_REV=$STREAM_SERVER_REV" \
    --workdir /work/stremio-native \
    "$IMAGE_NAME" \
    bash -lc '
        set -euo pipefail

        export VCPKG_ROOT=/home/dev/vcpkg
        export VCPKG_INSTALLED_DIR=/home/dev/vcpkg/downloads/installed
        export VCPKGRS_TRIPLET=x64-linux-release
        export CARGO_TARGET_DIR=/home/dev/.cargo/target

        SERVER_DIR=/tmp/stream-server
        git init --quiet "$SERVER_DIR"
        git -C "$SERVER_DIR" remote add origin "$STREAM_SERVER_GIT_URL"
        git -C "$SERVER_DIR" fetch --quiet --depth=1 origin "$STREAM_SERVER_REV"
        git -C "$SERVER_DIR" checkout --quiet --detach FETCH_HEAD
        if [[ "$(git -C "$SERVER_DIR" rev-parse HEAD)" != "$STREAM_SERVER_REV" ]]; then
            printf "Fetched Stream Server revision does not match Cargo.toml.\n" >&2
            exit 1
        fi

        cd "$SERVER_DIR"
        mkdir -p "$VCPKG_INSTALLED_DIR/vcpkg"
        "$VCPKG_ROOT/vcpkg" install \
            --x-install-root="$VCPKG_INSTALLED_DIR" \
            --x-buildtrees-root="$VCPKG_ROOT/downloads/buildtrees" \
            --triplet=x64-linux-release \
            --overlay-triplets="$SERVER_DIR/triplets" \
            --overlay-ports="$SERVER_DIR/vcpkg-overlays"
        cargo build --locked --package server

        cd /work/stremio-native
        cargo build --locked --package stremio-native
    '
