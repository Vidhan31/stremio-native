#!/usr/bin/env bash
set -euo pipefail

# Ensure DNF parallel downloads is configured inside the container
if [[ ! -f /etc/dnf/libdnf5.conf.d/90-parallel-downloads.conf ]]; then
    sudo mkdir -p /etc/dnf/libdnf5.conf.d
    sudo sh -c 'printf "[main]\nfastestmirror=True\nmax_parallel_downloads=10\nmax_downloads_per_mirror=10\n" > /etc/dnf/libdnf5.conf.d/90-parallel-downloads.conf'
fi

# Configure vcpkg environment in /etc/profile.d/stremio-dev.sh
if [[ ! -f /etc/profile.d/stremio-dev.sh ]]; then
    sudo sh -c 'cat << "EOF" > /etc/profile.d/stremio-dev.sh
export VCPKG_ROOT=/opt/vcpkg
export VCPKG_INSTALLED_DIR=/opt/vcpkg/installed
export VCPKGRS_TRIPLET=x64-linux-release
export PATH=$PATH:/opt/vcpkg
EOF'
fi

# Clone and bootstrap vcpkg in /opt/vcpkg if missing
if [[ ! -d /opt/vcpkg ]]; then
    printf 'Bootstrapping vcpkg in /opt/vcpkg...\n'
    sudo git clone https://github.com/microsoft/vcpkg.git /opt/vcpkg
    sudo git -C /opt/vcpkg checkout 84bab45d415d22042bd0b9081aea57f362da3f35
    sudo /opt/vcpkg/bootstrap-vcpkg.sh -disableMetrics
    sudo chmod -R a+rwX /opt/vcpkg
fi

# Ensure parent Cargo config exists to link sibling checkouts
PROJECTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PARENT_CARGO_CONFIG="${PROJECTS_DIR}/.cargo/config.toml"

if [[ ! -f "$PARENT_CARGO_CONFIG" ]]; then
    mkdir -p "${PROJECTS_DIR}/.cargo"
    cat << 'EOF' > "$PARENT_CARGO_CONFIG"
[patch."https://github.com/Vidhan31/stremio-core.git"]
stremio-core = { path = "stremio-core" }
EOF
fi
