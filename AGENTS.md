# Architectural boundaries

- Rewrite of Stremio UI and server in Rust for native performance.
- Do not add dependencies unless the user explicitly gives permission.
- Sibling checkouts: `~/Projects/stremio-core`. Consult before changing integrations.
  - `stremio-core` is the open-source Rust core engine behind Stremio.
    - This domain layer contains the shared logic used by Stremio apps, including addon communication, catalogs, library and profile state, notifications, playback state, deep links, and streaming models.
    - We forked upstream `stremio-core` to fix dependency conflicts.
- The server subsystem is a reverse-engineered Rust rewrite of Stremio's closed-source `server.js`. The server subsystem is split into modular crates:
  - `server/`: HTTP server, Axum routes, local addon indexing, and casting.
  - `enginefs/`: Torrent streaming engine, libtorrent alerts, piece caching, and transcoding.
  - `bindings/`: Native C++ FFI bridges (`libtorrent-sys`, `async-rar`, `async-sevenz`).

# Upstream reference

- Reference bundle of `server.js` at `research/server.js` (6.5 MB). Do not read whole. Search with `rg` and read targeted line ranges.
- We must maintain API compatibility with upstream `stremio-core`.

# Build environment

- Never run cargo directly on the host. Execute all builds, checks, and runs inside Distrobox; follow the `stremio-build` skill.
