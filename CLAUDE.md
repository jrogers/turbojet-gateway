# Working on Turbojet Hub

Turbojet Hub is a FIX connectivity gateway built on the Turbojet engine, from crates.io.

- House style is Turbojet's [STYLE.md](https://github.com/jrogers/turbojet/blob/main/docs/STYLE.md)
  (`../turbojet/docs/STYLE.md` in a checkout); read it before writing code.
- Plans and working notes go in `docs/plans/` (gitignored).
- Feature branches are git worktrees under `.worktrees/` (gitignored).
- Commits are small and each says why.
- `--all-features` includes `console`, which embeds `web/dist`: run `(cd web && npm ci && npm run
  build)` first.
- Before committing, `scripts/check.sh`: CI's per-push checks (`.github/workflows/ci.yml`), the
  web console's lint, tests and build, then clippy and tests for each feature. While iterating,
  `cargo test --all-features` and, in `web/`, `npm test -- --run` are enough.
- With a PostgreSQL to hand, also `TURBOJET_HUB_POSTGRES_URL=… scripts/check.sh --postgres`: see
  `src/store/tests.rs`'s `postgres()`. `--msrv` adds the 1.89 tests, `--deny` cargo-deny.
- Engine work builds against a Turbojet checkout at `../turbojet` through a local, untracked
  `.cargo/config.toml` (gitignored):
  ```toml
  [patch.crates-io]
  turbojet = { path = "../turbojet/crates/turbojet" }
  turbojet-config = { path = "../turbojet/crates/turbojet-config" }
  turbojet-fix42 = { path = "../turbojet/crates/turbojet-fix42" }
  turbojet-fix44 = { path = "../turbojet/crates/turbojet-fix44" }
  ```
  It rewrites `Cargo.lock`'s Turbojet entries to the path; don't commit those. In the main
  checkout it applies to the worktrees under `.worktrees/` too. The checkout's version must match
  `0.3`, or cargo ignores the patch with only a warning.
- Engine bugs found here are fixed in the Turbojet repo, in their own commits; Turbojet Hub takes the
  fix from the next release, or meanwhile through the local patch.
