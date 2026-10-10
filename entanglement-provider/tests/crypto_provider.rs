//! `install_default_crypto_provider` mutates process-global rustls state, so it
//! is tested in its own integration-test binary (its own process) rather than
//! beside the lib unit tests that assert clients build *without* it.

use entanglement_provider::client::install_default_crypto_provider;

#[test]
fn install_is_first_caller_wins_and_enables_bare_clients() {
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
    assert!(
        install_default_crypto_provider(),
        "first call installs ring"
    );
    assert!(
        !install_default_crypto_provider(),
        "a second call leaves the installed provider in place"
    );
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    // The embedder hazard this cures: a bare client under
    // `rustls-no-provider` panics without a process-wide provider.
    reqwest::Client::new();
}
