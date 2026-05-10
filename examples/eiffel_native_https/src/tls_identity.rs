//! Self-signed TLS identity shared by both implementations. Both
//! sides install the same rcgen-generated leaf so the scripted
//! client can trust either by the same DER bytes.

/// One DER-encoded self-signed leaf cert and matching PKCS#8 key.
/// `cert_chain_der` is what the server presents on the wire;
/// `private_key_der` matches the leaf; `cert_der` is the same
/// leaf bytes the client adds to its trust store.
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
