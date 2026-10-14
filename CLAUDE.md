# Working on Jetbridge

Jetbridge is a FIX connectivity gateway built on the Turbojet engine, from crates.io.

- House style is Turbojet's [STYLE.md](../turbojet/docs/STYLE.md); read it before writing code.
- Plans and working notes go in `docs/plans/` (gitignored).
- Feature branches are git worktrees under `.worktrees/` (gitignored).
- Commits are small and each says why.
- `--all-features` includes `console`, which embeds `web/dist`: run `(cd web && npm ci && npm run
  build)` first.
- Before committing: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`, and
  in `web/`, `npm run lint && npm test -- --run`.
- With a PostgreSQL to hand, also `JETBRIDGE_POSTGRES_URL=… cargo test --features postgres
  store::`: see `src/store/tests.rs`'s `postgres()`.
- Engine work builds against a Turbojet checkout at `../turbojet` through a local, untracked
  `.cargo/config.toml` (gitignored):
  ```toml
  [patch.crates-io]
  turbojet = { path = "../turbojet/crates/turbojet" }
  turbojet-config = { path = "../turbojet/crates/turbojet-config" }
  turbojet-fix42 = { path = "../turbojet/crates/turbojet-fix42" }
  turbojet-fix44 = { path = "../turbojet/crates/turbojet-fix44" }
  ```
  It rewrites `Cargo.lock`'s Turbojet entries to the path; don't commit those.
- Engine bugs found here are fixed in the Turbojet repo, in their own commits; Jetbridge takes the
  fix from the next release, or meanwhile through the local patch.
