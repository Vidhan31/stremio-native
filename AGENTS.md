# Architectural boundaries

- Rewrite of Stremio UI and server in Rust for native performance.
- Do not add dependencies unless the user explicitly gives permission.
- Sibling checkouts: `~/Projects/stremio-core`. Consult before changing integrations.

# Upstream reference

- Reference bundle is at `research/server.js` (6.5 MB). Do not read whole. Search with `rg` and read targeted line ranges.
- Legacy protocols from `server.js` (FTP and NZB/Usenet) are discarded and out of scope.

# Build environment

- Never run cargo directly on the host. Execute all builds, checks, and runs inside Distrobox; follow the `stremio-build` skill.
