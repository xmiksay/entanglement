//! The one TLS setup every outbound client uses (LLM endpoints, MCP HTTP,
//! OAuth): rustls with the `ring` provider and the bundled webpki roots.
//!
//! WHY explicit: reqwest 0.13's stock `rustls` feature means aws-lc-rs + the OS
//! trust store. We keep ring + bundled roots instead — no runtime reliance on
//! the OS trust store (minimal/musl containers work without `ca-certificates`)
//! and no aws-lc C build in ARM/musl cross-builds. (`rustls-platform-verifier`
//! is still compiled — `rustls-no-provider` pulls it — but never used.) reqwest
//! is built with `rustls-no-provider`, under which a client built *without*
//! this config panics looking for a process-wide crypto provider, so every
//! client in this crate must start from [`client_builder`].

use std::sync::{Arc, LazyLock};

static TLS_CONFIG: LazyLock<rustls::ClientConfig> = LazyLock::new(|| {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("invariant: ring's default provider supports rustls' safe default TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    // reqwest is built without `http2`, so advertise exactly what it speaks
    // (a preconfigured config is used verbatim — reqwest sets no ALPN on it).
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
});

/// A `reqwest` builder preloaded with the crate's TLS configuration. Every
/// `reqwest::Client` the provider builds starts here.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().tls_backend_preconfigured(TLS_CONFIG.clone())
}

/// Opt-in for embedders: install ring as the **process-wide** default rustls
/// crypto provider. Returns whether this call won — first caller wins, and a
/// provider someone else already installed is left in place.
///
/// The engine never calls this itself; the provider's own clients don't need
/// it. An embedder does when other code in its build relies on rustls' implicit
/// provider — a bare `reqwest::Client::new()` (which panics under the
/// `rustls-no-provider` feature this crate unifies in), or
/// `rustls::ClientConfig::builder()` / tokio-tungstenite / sqlx rustls paths
/// when both ring and aws-lc-rs are compiled and rustls can't pick one. Call
/// it early in `main`, before any such client is built.
pub fn install_default_crypto_provider() -> bool {
    rustls::crypto::ring::default_provider()
        .install_default()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_without_a_process_wide_crypto_provider() {
        // With `rustls-no-provider`, a bare `Client::new()` would panic here
        // (no provider installed); the preconfigured builder must not.
        client_builder().build().expect("client builds");
    }

    #[test]
    fn config_uses_ring_and_bundled_roots() {
        assert!(!TLS_CONFIG.crypto_provider().cipher_suites.is_empty());
        assert_eq!(TLS_CONFIG.alpn_protocols, vec![b"http/1.1".to_vec()]);
        assert!(!webpki_roots::TLS_SERVER_ROOTS.is_empty());
    }
}
