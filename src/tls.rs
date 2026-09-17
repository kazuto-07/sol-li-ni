//! Process-wide TLS setup.
//!
//! webrtc-rs and reqwest both use rustls, which needs one crypto provider chosen for the whole
//! process. reqwest is built with `rustls-no-provider` and panics if none is installed, so this
//! runs before any client is created — from `main`, and from anywhere that builds a client.

use std::sync::Once;

static INSTALL: Once = Once::new();

/// Installs ring as the rustls provider. Idempotent, and safe to call from tests.
pub fn install() {
    INSTALL.call_once(|| {
        // Fails only if something else installed a provider first, which is equally fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
