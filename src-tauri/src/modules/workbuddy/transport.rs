//! WorkBuddy 5.6.2 Linux / Electron 37.10.3 Node fetch transport.
//! Captured with the official runtime against loopback, without account traffic.
use rquest::{AlpnProtos, EmulationProvider, SslCurve, TlsConfig, TlsVersion};
use std::time::Duration;

const CIPHERS: &str = "TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES128-SHA256:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA";
const SIGNATURES: &str = "ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:rsa_pkcs1_sha256:ecdsa_secp384r1_sha384:rsa_pss_rsae_sha384:rsa_pkcs1_sha384:rsa_pss_rsae_sha512:rsa_pkcs1_sha512:rsa_pkcs1_sha1";

pub fn builder() -> rquest::ClientBuilder {
    let tls = TlsConfig::builder()
        .cipher_list(CIPHERS)
        .sigalgs_list(SIGNATURES)
        .curves(&[SslCurve::X25519, SslCurve::SECP256R1, SslCurve::SECP384R1][..])
        .alpn_protos(AlpnProtos::HTTP1)
        .min_tls_version(TlsVersion::TLS_1_2)
        .max_tls_version(TlsVersion::TLS_1_3)
        .grease_enabled(false)
        .permute_extensions(false)
        .build();
    rquest::Client::builder()
        .emulation(EmulationProvider::builder().tls_config(tls).build())
        .http1_only()
        .no_proxy()
        .redirect(rquest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::collections::BTreeMap;
    use tokio::io::AsyncReadExt;

    fn profile(bytes: &[u8]) -> Value {
        fn u16s(bytes: &[u8]) -> Vec<u16> {
            bytes
                .chunks_exact(2)
                .map(|p| u16::from_be_bytes([p[0], p[1]]))
                .collect()
        }
        let mut pos = 4 + 2 + 32;
        pos += 1 + bytes[pos] as usize;
        let n = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        let ciphers = u16s(&bytes[pos..pos + n]);
        pos += n;
        pos += 1 + bytes[pos] as usize;
        let end = pos + 2 + u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
        pos += 2;
        let mut order = Vec::new();
        let mut ext = BTreeMap::new();
        while pos < end {
            let key = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]);
            let n = u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]) as usize;
            pos += 4;
            ext.insert(key, &bytes[pos..pos + n]);
            order.push(key);
            pos += n;
        }
        json!({"ciphers":ciphers,"extensions":order,"groups":u16s(&ext[&10][2..]),"signature_algorithms":u16s(&ext[&13][2..]),"alpn":[std::str::from_utf8(&ext[&16][3..]).unwrap()],"versions":u16s(&ext[&43][1..])})
    }

    #[tokio::test]
    async fn workbuddy_client_hello_matches_the_official_electron_node_fetch_capture() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request = tokio::spawn(async move {
            builder()
                .resolve("localhost", address)
                .build()
                .unwrap()
                .get(format!("https://localhost:{}/", address.port()))
                .timeout(Duration::from_secs(3))
                .send()
                .await
        });
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut header = [0; 5];
        socket.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0], 22);
        let mut body = vec![0; u16::from_be_bytes([header[3], header[4]]) as usize];
        socket.read_exact(&mut body).await.unwrap();
        drop(socket);
        assert!(request.await.unwrap().is_err());
        let fixture: Value =
            serde_json::from_str(include_str!("fixtures/linux-5.6.2.json")).unwrap();
        assert_eq!(profile(&body), fixture["tls"]["fetch"]);
    }
}
