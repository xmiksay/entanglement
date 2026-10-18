# Embedding TLS & dependency pinning

> Part of the [embedding guide](../embedding.md).

## TLS and the rustls crypto provider

`entanglement-provider` talks TLS through rustls with the **ring** provider and
the **bundled webpki roots**, configured explicitly on every client it builds
(`client::client_builder()`; [provider](../architecture/provider.md)). To keep
aws-lc out of the tree it builds reqwest 0.13 with `rustls-no-provider`, and
Cargo's feature unification carries that into *your* build. Two cases bite:

- **You also enable `rustls/aws_lc_rs`** — e.g. stock reqwest 0.13 with its
  default features. Both providers are now compiled, so rustls can no longer
  pick one implicitly (`CryptoProvider::from_crate_features()` returns
  `None`): any code of yours relying on that implicit provider —
  `rustls::ClientConfig::builder()`, tokio-tungstenite's or sqlx's rustls
  paths — panics. Your own `reqwest::Client::new()` still works (reqwest's
  aws-lc feature names its provider explicitly).
- **You use reqwest 0.13 with `default-features = false` and no TLS
  feature.** Our `rustls-no-provider` unifies `__rustls` into your reqwest, so
  your `reqwest::Client::new()` now *panics* at construction for want of a
  provider — previously such a client only failed HTTPS at request time.

The general cure, covering both cases and every rustls user in the process:

```rust
// Early in main, before any client is built. Process-global, first caller
// wins (returns whether this call installed it); the engine never calls it.
entanglement_provider::client::install_default_crypto_provider();
```

Building your reqwest clients from
`entanglement_provider::client::client_builder()` (ring + webpki roots, no
global state) is an alternative **for reqwest clients only** — it fully cures
case (b), but in case (a) your other rustls users (tokio-tungstenite, sqlx,
`rustls::ClientConfig::builder()`) still need the install above or an explicit
provider of their own. An embedder on stock reqwest 0.13
compiles a second TLS provider (aws-lc-rs) next to ours — that's expected.
`rustls-platform-verifier` is compiled too (pulled by `rustls-no-provider`) but
never used: our clients never consult the OS trust store.

## Pinning a dependency

Until now there were no tags, so a `Cargo.toml` git dependency had nothing
stable to pin beyond a raw commit (`rev = "…"`). Starting from this guide,
notable merges get tagged `v0.1.x`:

```toml
entanglement-runtime = { git = "https://github.com/xmiksay/entanglement", tag = "v0.1.0", default-features = false }
```

Prefer `tag =` over `rev =` in a site-style consumer — it survives a
`cargo update` re-resolve intentionally (a `rev` pin is silently frozen either
way, but a tag documents *which* release you're on).

Every tagged release also publishes to crates.io (see
[`releasing.md`](../releasing.md)), so once the crate you need has a release out,
a plain registry dependency works too and needs no `git`/`tag` at all:

```toml
entanglement-runtime = { version = "0.1", default-features = false }
```

---

Previous: [Migration notes](migrations.md)
