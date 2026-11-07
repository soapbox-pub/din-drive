//! Relays bring ring and Blossom brings aws-lc-rs, so rustls can't choose a
//! crypto provider by itself; din-drive chooses one, or every wss:// and
//! https:// connection panics.

#[test]
fn crypto_provider_is_chosen() {
    din_drive::install_crypto();
    let _ = rustls::ClientConfig::builder();
}
