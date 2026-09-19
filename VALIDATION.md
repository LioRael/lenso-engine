# Local migration validation

The migration is local-only across the new `lenso-engine` checkout and the existing
CLI convention worktree. The user-owned `docs/convention-authoring.md` edit in the
CLI worktree is intentionally preserved.

Validation covers:

- Independent Engine workspace unit/integration/doc tests and strict Clippy.
- CLI compatibility unit/integration/doc tests and its no-default-features library.
- Real App workflows: TypeScript without Cargo, mixed Rust/TypeScript with typed
  command macros and source-free execution, and a custom convention compiler.
- Generated processor capability projection freshness, Plan-bound request success,
  provider rejection with diagnostics, repeated invocation and clean shutdown.
- Read-only preset resolution, cycles, local discovery, executable/artifact locks,
  lock mismatch before invocation, incremental failure/recovery and disable/removal.
- Atomic file publication, conflict rollback and tamper detection.
- Manual end-to-end locked precompiled worker with empty PATH: inspect/run/dev,
  failure preserves current publication, recovery/deletion updates publication,
  SIGTERM closes the watch host, changed executable is rejected, and published
  resources verify after deleting original sources and implementation files.

No registry publication, remote repository setup, CI execution, merge or cleanup
is claimed by local tests. macOS is the executed platform for real process tests.

Latest local results:

- Independent workspace: 161 passing tests;
  10 opt-in legacy tests remain ignored by the default workspace command.
- CLI compatibility workspace: 17 passing tests, 5 opt-in tests ignored by default.
- All three opt-in convention/App acceptance flows passed separately. A focused
  no-Rust App build also validates the final planning fingerprint guard.
- Both workspaces pass strict all-target Clippy and formatting; the CLI's
  no-default-features authoring facade compiles.
- A standalone `build_app` library embedding built a TypeScript App using an
  explicitly supplied `lenso-engine-host`, with no CLI/Cargo/rustc on PATH. After
  deleting source and development tools, the distribution ran with empty PATH.
  Bun/Node were available during TypeScript authoring; they were not Rust tools.
