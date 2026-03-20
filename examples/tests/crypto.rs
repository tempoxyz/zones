use anyhow::Result;
use zone_examples::crypto::{
    build_recipient_cert, generate_signing_key, now_unix, verify_recipient_cert,
};

#[test]
fn recipient_cert_roundtrip_verifies() -> Result<()> {
    let key = generate_signing_key();
    let cert = build_recipient_cert("user@example.com", "route-root", &key, now_unix() + 60, 1);
    verify_recipient_cert(&cert, "user@example.com")
}
