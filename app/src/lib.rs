//! Library half of `mnemis-app`. Currently only exposes the UI test
//! harness so it can be shared between the `ui-probe` binary and the
//! `app/tests/ui_smoke.rs` integration test.
//!
//! All contents are gated behind the `ui-probe` feature so default builds
//! (the user-facing `mnemis-app` binary) don't drag in fantoccini etc.

#[cfg(feature = "ui-probe")]
pub mod test_support;
