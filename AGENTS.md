# Architectural boundaries

- Rewrite of Stremio UI and server in Rust for native performance.
- Do not add dependencies unless the user explicitly gives permission.
- Sibling checkouts: `~/Projects/stream-server` and `~/Projects/stremio-core`. Consult before changing integrations.

# Build environment

- Never run cargo directly on the host. Execute all builds, checks, and runs inside Distrobox; follow the `stremio-build` skill.
