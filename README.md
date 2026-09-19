# Lenso Engine

An independently embeddable authoring layer over Lenso. The Engine plans work
from immutable input snapshots and executes explicitly selected processors.
File conventions, languages, and App composition belong to optional packages.
The Lenso CLI is a consumer of these libraries.

## Packages

| Package | Owner |
| --- | --- |
| `lenso-engine` | Inputs, processing plans, dependencies, session cache, local bootstrap locks, incremental sessions, resource publication |
| `lenso-engine-runtime` | `lenso.engine.processor@1` contract, generated Rust/TypeScript projections, native SDK lowering through the existing Kernel/Adapter |
| `lenso-engine-markdown` | Optional Markdown reading convention |
| `lenso-engine-worker` | Precompiled, independently adoptable Markdown processor artifact |
| `lenso-engine-host` | Standalone precompiled App runtime/resolver for library embeddings |
| `lenso-engine-app` | Optional `AppProject` processor, App/Plugin scaffolding, compilation, contracts, Host assembly, distribution and development integration |
| `lenso-engine-authoring` | App source discovery, composition, Host and Plugin Root authority |
| `lenso-plugin-catalog` | Portable signed catalog protocol; retained package identity |

The core and Markdown packages do not depend on App, Cargo, Bun, the CLI or its
argument parser. App dependencies are absent when those packages are used alone.
The CLI's `lenso_app_authoring` library remains a compatibility re-export.

## Start without configuration

With the updated precompiled CLI:

```sh
lenso engine inspect --source ./content --markdown
lenso engine run --source ./content --markdown
lenso engine dev --source ./content --markdown --output ./dist
```

`inspect` returns a read-only plan. `run` returns one complete JSON generation.
`dev` keeps a session alive and emits structured update/failure events. No App,
Host project, Rust toolchain, Cargo or Bun is needed for this Markdown workflow.
Unrelated files such as `plugin.rs` have no meaning unless support is selected.

Existing App commands continue to work. `app build` and `app dev` select the
optional `AppProject` processor. Its App-specific discovery and assembly live in
this repository, not in the CLI. Lower-level App APIs remain public:

```rust,ignore
lenso_engine_app::app::create_empty(project.clone())?;
lenso_engine_app::app::adopt(project.clone(), "@lenso/cli".into(), true)?;
lenso_engine_app::app::build_local(project, distribution, precompiled_engine_host)?;
```

An embedding tool can register `AppProject { root, output, runtime_executable }` in its own Engine
through `RuntimeProcessor::new`, without invoking CLI argument parsing. The
runtime executable is explicit: an IDE can supply `lenso-engine-host` instead of
copying itself as a runtime. The selected host must pass a protocol/target probe.
`crates/lenso-engine-app/examples/build_app.rs` demonstrates this boundary.

## Local plugin sources and presets

A workflow is optional and contains only composition decisions:

```json
{
  "schema": "lenso.engine-workflow.v1",
  "sources": ["content"],
  "plugin_sources": ["tools"],
  "plugins": ["example.reader.v1"],
  "presets": []
}
```

`plugin_sources` are local directories searched for `engine-plugin.json` files.
They are not marketplaces. Discovery does not activate every available plugin.
`plugins` explicitly selects identities; duplicate identities are rejected.
Presets are other workflow JSON files, resolved relative to their own directory.
Cycles and excessive nesting fail before processor execution.

A local processor manifest:

```json
{
  "schema": "lenso.engine-plugin.v1",
  "identity": "example.reader.v1",
  "entries": ["*.md", "**/*.md"],
  "program": "bun",
  "args": ["processor.ts"],
  "artifacts": ["processor.ts"],
  "after": []
}
```

`after` contains explicit predecessor step IDs; `{path}` expands to the matched
input path. Multiple processors may consume one input. Artifacts list the owned
implementation files/directories; directory proofs include membership, so adding
or deleting a file invalidates the lock. Declare the complete private dependency
closure, or distribute a self-contained precompiled executable.

```sh
lenso engine lock --workflow engine.json
lenso engine inspect --workflow engine.json
lenso engine run --workflow engine.json --output dist
lenso engine dev --workflow engine.json --output dist
```

When `engine.json` exists, `run` and `dev` use it if no source/workflow argument is
supplied. `lock` hashes the existing selected executables, declared artifacts and
preset/configuration files without running them. It never installs packages or
compiles source. `run` verifies `engine.lock.json`; changed tools/configuration
require an explicit re-lock. Source documents remain editable without re-locking.
The lock records canonical local paths and is host-local; relocate the tools or
workspace by regenerating it explicitly. It is not a registry lock or signature.

Direct `--plugin ./tools/engine-plugin.json` adoption remains available for local
experiments without a lock. Workflow-based execution uses the strict lock path.

## Language-neutral processing

Programs receive one JSON request on stdin and write one response on stdout.
Diagnostics belong on stderr. The process is a trusted implementation tool owned
by the processor Plugin; it is not a new Lenso Execution Class.

```json
{
  "schema": "lenso.engine-process.v1",
  "step": {"id":"example.reader.v1/hello.md","inputs":["hello.md"],"after":[],"options":null},
  "files": {"hello.md":[72,105]},
  "dependencies": {}
}
```

```json
{
  "schema": "lenso.engine-processed.v1",
  "outputs": {
    "document": {"schema":"example.document.v1","value":{"text":"Hi"}},
    "page": {"schema":"lenso.engine.file.v1","value":{"path":"hello.txt","bytes":[72,105]}}
  }
}
```

Rust, Bun, Python and other executables can implement this protocol. Read, parse,
compile, transform and index are processor behavior, not a closed Engine action
enumeration. Data schemas are consumer-owned; the core validates carriers and
bounds, not every domain payload. Rust processors implement the public `Plugin`
interface (`Send + Sync`) and can construct file carriers with `Resource::file`.

## Runtime and bootstrap boundary

Official and third-party processor implementations use `RuntimeProcessor`, which
lowers execution to the generated `lenso.engine.processor@1` request Capability.
Each invocation resolves an exact consumer/provider binding, starts an existing
Lenso Kernel/native Adapter generation, invokes its typed handle, and shuts down
on success or error. The Engine work graph orders processing tasks; it does not
replace Kernel bindings, plugin supervision or application business routing.

The native SDK bridge owns two closed packages, `lenso.engine.consumer` and
`lenso.engine.processor`, pinned to the SDK package version. The latter owns the
selected implementation tool and exposes only the processing role. Business
processors receive declared input bytes and predecessor resources; the host
retains selection authority. Generated contract sources and projections live in
`lenso-engine-runtime`, with an automated freshness test. The contract is portable;
this host bridge currently uses the native Adapter. Existing App distributions
retain their Bun/Process/Wasm admission paths.

The App workflow itself is an optional processor and invokes convention
processors through the same public seam. This is the DX self-hosting boundary:
the compiled bootstrap host loads explicit manifests/precompiled artifacts before
source conventions run. It never needs a `plugin.rs` convention to load the plugin
that defines `plugin.rs`. The standalone worker example demonstrates adoption of
an already-built official processor through the same local path as third parties.

```sh
# Toolchain-author workflow, not an end-user prerequisite:
cargo build -p lenso-engine-worker
# Put that precompiled binary on PATH, then from examples/documents:
lenso engine lock
lenso engine run --output dist
```

## Incremental results and resources

`Session` preserves the last successful generation after a failed refresh.
`replace_engine` replaces the admitted processor set and invalidates selection;
deleted files or disabled plugins disappear from the next successful generation.
`watch` is an embeddable, cancellable polling subscription emitting typed events.
The CLI also reloads changed workflow locks. Caching is session-local, bounded,
and opt-in for pure processors with fully declared inputs. Cache keys include
implementation identity, options, input bytes and predecessor results. Compilers
with filesystem side effects are not cached as if JSON results restored artifacts.

`publication::publish` validates resource paths/conflicts, creates an immutable
generation with content hashes, then atomically replaces `current.json`. It holds
a publication lock and never exposes half-written output. Consumers can call
`publication::verify` and read referenced files after source/plugin deletion.
The CLI refuses to publish inside its input source directories. Old immutable
generations are retained; deletion/retention is an explicit host policy.

## Limits and trust

- Native extensions and subprocess tools are trusted code, not OS sandboxes.
- Input snapshots reject symlinks/special files and are bounded to 4096 entries,
  32 directory levels and 16 MiB. Source roots are explicit; the core has no
  language-specific `app/` or `node_modules` policy.
- External tools have a default 60-second execution budget, 128-MiB request
  ceiling and 1-MiB stdout/stderr ceilings. A selected App convention can declare
  a compiler-specific `timeout_seconds` (1–300) and `output_limit_bytes`
  (1–16 MiB) in its `compiler` metadata; omitted values retain those defaults.
  Unix cancellation/timeout terminates the process group. Windows currently
  terminates the direct child; descendant isolation requires a Windows Job Object
  host. No Windows execution proof is claimed here.
- Native processor cancellation is cooperative. App builds check cancellation
  between artifacts and before atomic publication; an in-progress package-manager
  command may finish before that checkpoint.
- The core installs no signal handlers and never exits the embedding process.
  CLI/App hosts own their signal policy. Each native SDK invocation owns its
  runtime thread, permitting nested App-to-compiler invocations without nested
  executor failures.
- The low-level core can execute trusted processors directly. Applications
  wanting Lenso lifecycle/admission use the runtime SDK, as the CLI does.

## Development and local migration

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

This repository was created locally from the existing CLI implementation. The CLI
checkout currently consumes these unpublished packages through local path
references. No remote repository, commit, push, merge or registry release has
been performed. A later authorized release must publish the dependency chain and
switch CLI references to released versions before remote standalone closeout.

## Repository layout

Rust workspace members live under `crates/`. Each crate keeps its own tests,
assets, contracts and focused examples alongside its implementation. Root
`examples/` contains repository-level usage examples. Run Cargo commands from
the repository root; all members share the root lockfile and target directory.
