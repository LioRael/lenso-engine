# Bundled terminal support

The contract authority is `LioRael/lenso-terminal` at
`b08c6017226c59901b06e673de39aa145e638913`. The `contracts/command` and
`contracts/provider` snapshots preserve its `lenso.terminal.command@1` and
`lenso.terminal.command-provider@1` identities, versions, schemas, and digests.
There is no parallel request-only command protocol.

`command.ts`, `provider.ts`, and `src/app/terminal/{command,provider}.rs` are
unmodified projections generated with lenso-contract-codegen 0.9.0 (TypeScript
and RustRuntime). The workspace test `terminal_projections_match_owned_contract_snapshots`
checks all four against these snapshots. Regenerate with the same CLI's
`generate capability.json --typescript OUTPUT` or `--rust-runtime OUTPUT`.

The parser in `src/app/terminal/parser.rs` comes from that revision's
`lenso-terminal-cli-surface`. Local adaptations change the contract import,
handle text-only commands without looking up an absent JSON flag, and reject
invalid catalog options before Clap can panic. Regression tests cover these
adaptations. Retain these changes when synchronizing the upstream parser.

The CLI embeds these files at build time. `app add @lenso/cli` materializes a local
support source package, including its router contribution and Rust helper SDK.
Here `@lenso/cli` denotes bundled support, not a registry download. `compiler.mjs`
is an ordinary compiler extension using the same protocol available to users.
The router aggregates installed provider contracts; the Host supplies only the
Plan-bound typed ingress and generated codecs. Business execution stays in
provider Plugins. Rust `#[command]` macros and the TypeScript helper support asynchronous command
functions with bounded output. The Rust builder remains available. Macro and SDK
tests run from the materialized support packages in the mixed-language lifecycle
test, covering typed values, errors, progressive output, and cancellation.
Both implement the existing Stream contract and retain its terminal errors.
