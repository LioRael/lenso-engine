# Lenso Plugin catalog protocol

The shared signed catalog protocol for native CLI and Marketplace Workers consumers.
The default feature set supports `wasm32-unknown-unknown` and contains no CLI,
network client, filesystem installer, or runtime adapter dependencies.

`Snapshot`, `Release`, `Envelope`, `Trust`, `Checkpoint`, and `VerifiedSnapshot`
retain the `lenso.marketplace.snapshot.v1` wire format. `sign` and `verify` use
the existing domain-separated Ed25519 protocol and exact decoded payload bytes.
Callers supply trusted keys, current time, and the last durable checkpoint.
Verification preserves freshness, rollback, equivocation, and immutable release
identity checks; it never grants installation or execution authority.

For display-only callers, `verify_for_browse` returns a `BrowseSnapshot` that
permits expired metadata while retaining signature, schema, identity, future-issue,
rollback, and equivocation checks. It exposes expiry for a visible stale notice
and has no installation selection API. `verify` and `VerifiedSnapshot::select`
continue to reject expired catalogs; browsing must never substitute for them.

The optional `bundle-verification` feature adds the native
`Release::verify_bundle_directory` compatibility method using the framework's
Bundle verifier. It is enabled by `lenso-cli`; Workers consumers must leave it
disabled. Download, extraction, origin policy, installation, and persistence
remain caller responsibilities.

Canonical Plugin identity and exact release-version validators are exposed in
`identity` and reexported by the CLI's existing `identity` module. Existing CLI
catalog imports continue through `lenso_app_authoring::signed_plugin_catalog`.

This new package is prepared for local review. It has not been published; registry
release requires the repository's Trusted Publisher workflow and explicit approval.
