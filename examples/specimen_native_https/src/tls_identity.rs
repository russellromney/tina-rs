//! Shared rcgen self-signed leaf. Both sides install the same DER
//! bytes so the scripted client trusts either server.

/// `cert_der` and `cert_chain_der[0]` are identical — the leaf bytes
/// the server presents and the client trusts.
pub struct GeneratedIdentity {
    pub cert_der: Vec<u8>,
    pub cert_chain_der: Vec<Vec<u8>>,
    pub private_key_der: Vec<u8>,
}

pub fn generate() -> GeneratedIdentity {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen self-sign");
    let cert_der = certified.cert.der().to_vec();
    let private_key_der = certified.key_pair.serialize_der();
    GeneratedIdentity {
        cert_der: cert_der.clone(),
        cert_chain_der: vec![cert_der],
        private_key_der,
    }
}
