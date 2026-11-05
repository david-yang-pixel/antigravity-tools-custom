use super::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::net::TcpListener;

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
    json!({"ciphers":ciphers,"extensions":order,"groups":u16s(&ext[&10][2..]),"signature_algorithms":u16s(&ext[&13][2..]),"point_formats":&ext[&11][1..],"alpn":ext[&16],"supported_versions":ext[&43]})
}

#[tokio::test]
async fn client_hello_matches_node_openssl35_not_the_build_hosts_tls() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let request = tokio::spawn(async move {
        Client::new("")
            .unwrap()
            .get(format!("https://localhost:{port}/"))
            .timeout(Duration::from_secs(3))
            .send()
            .await
    });
    let (mut socket, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut header = [0; 5];
    socket.read_exact(&mut header).await.unwrap();
    assert_eq!(header[0], 22);
    let mut body = vec![0; u16::from_be_bytes([header[3], header[4]]) as usize];
    socket.read_exact(&mut body).await.unwrap();
    drop(socket);
    let _ = request.await;
    let fixture: Value =
        serde_json::from_str(include_str!("../tests/fixtures/node-tls-3.5.json")).unwrap();
    assert_eq!(profile(&body), fixture["profile"]);
}

fn server_tls() -> (openssl::ssl::SslAcceptor, SslConnectorBuilder) {
    use openssl::{
        asn1::Asn1Time,
        hash::MessageDigest,
        pkey::PKey,
        rsa::Rsa,
        ssl::SslAcceptor,
        x509::{extension::SubjectAlternativeName, X509NameBuilder},
    };
    let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "localhost").unwrap();
    let name = name.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_subject_name(&name).unwrap();
    cert.set_issuer_name(&name).unwrap();
    cert.set_pubkey(&key).unwrap();
    cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    let san = SubjectAlternativeName::new()
        .dns("localhost")
        .build(&cert.x509v3_context(None, None))
        .unwrap();
    cert.append_extension(san).unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    let cert = cert.build();
    let mut server = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).unwrap();
    server.set_certificate(&cert).unwrap();
    server.set_private_key(&key).unwrap();
    server.set_alpn_select_callback(|_, offers| {
        openssl::ssl::select_next_proto(b"\x08http/1.1", offers)
            .ok_or(openssl::ssl::AlpnError::NOACK)
    });
    let mut client = tls_builder().unwrap();
    client.cert_store_mut().add_cert(cert).unwrap();
    (server.build(), client)
}
async fn read_headers<S: AsyncRead + Unpin>(socket: &mut S) -> String {
    let mut out = Vec::new();
    while !out.ends_with(b"\r\n\r\n") {
        out.push(socket.read_u8().await.unwrap());
        assert!(out.len() < 16384);
    }
    String::from_utf8(out).unwrap()
}
#[tokio::test]
async fn verified_http_tunnel_sends_proxy_auth_only_to_proxy_and_never_redirects() {
    let (server, tls) = server_tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let connect = read_headers(&mut socket).await;
        assert!(connect.starts_with("CONNECT localhost:443 HTTP/1.1"));
        assert!(connect.contains("Proxy-Authorization: Basic dXNlcjpwYXNz"));
        socket
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .unwrap();
        let mut secure = SslStream::new(Ssl::new(server.context()).unwrap(), socket).unwrap();
        Pin::new(&mut secure).accept().await.unwrap();
        let request = read_headers(&mut secure).await.to_ascii_lowercase();
        assert!(request.starts_with("get /probe http/1.1"));
        assert!(!request.contains("proxy-authorization"));
        secure.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://untrusted.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
    });
    let client = Client::with_tls(&format!("http://user:pass@{address}"), tls).unwrap();
    let response = client.get("https://localhost/probe").send().await.unwrap();
    assert_eq!(response.status(), 302);
    task.await.unwrap();
}
#[tokio::test]
async fn socks5h_keeps_remote_dns_auth_and_verifies_upstream_tls() {
    let (server, tls) = server_tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        assert_eq!(socket.read_u8().await.unwrap(), 5);
        let methods = socket.read_u8().await.unwrap();
        let mut bytes = vec![0; methods as usize];
        socket.read_exact(&mut bytes).await.unwrap();
        assert!(bytes.contains(&2));
        socket.write_all(&[5, 2]).await.unwrap();
        assert_eq!(socket.read_u8().await.unwrap(), 1);
        let n = socket.read_u8().await.unwrap();
        let mut user = vec![0; n as usize];
        socket.read_exact(&mut user).await.unwrap();
        let n = socket.read_u8().await.unwrap();
        let mut pass = vec![0; n as usize];
        socket.read_exact(&mut pass).await.unwrap();
        assert_eq!(&user, b"user");
        assert_eq!(&pass, b"p@ss");
        socket.write_all(&[1, 0]).await.unwrap();
        let mut request = [0; 4];
        socket.read_exact(&mut request).await.unwrap();
        assert_eq!(request, [5, 1, 0, 3]);
        let n = socket.read_u8().await.unwrap();
        let mut host = vec![0; n as usize];
        socket.read_exact(&mut host).await.unwrap();
        assert_eq!(&host, b"localhost");
        assert_eq!(socket.read_u16().await.unwrap(), 443);
        socket
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
            .await
            .unwrap();
        let mut secure = SslStream::new(Ssl::new(server.context()).unwrap(), socket).unwrap();
        Pin::new(&mut secure).accept().await.unwrap();
        read_headers(&mut secure).await;
        secure
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await
            .unwrap();
    });
    let client = Client::with_tls(&format!("socks5h://user:p%40ss@{address}"), tls).unwrap();
    assert_eq!(
        client
            .get("https://localhost/")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "{}"
    );
    task.await.unwrap();
}
#[tokio::test]
async fn socks4a_preserves_user_id_and_remote_dns() {
    let (server, tls) = server_tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 8];
        socket.read_exact(&mut request).await.unwrap();
        assert_eq!(&request[..4], &[4, 1, 1, 187]);
        assert_eq!(&request[4..7], &[0, 0, 0]);
        assert_ne!(request[7], 0);
        let mut identity = Vec::new();
        loop {
            let byte = socket.read_u8().await.unwrap();
            if byte == 0 {
                break;
            }
            identity.push(byte);
        }
        assert_eq!(identity, b"proxy-user");
        let mut host = Vec::new();
        loop {
            let byte = socket.read_u8().await.unwrap();
            if byte == 0 {
                break;
            }
            host.push(byte);
        }
        assert_eq!(host, b"localhost");
        socket.write_all(&[0, 90, 0, 0, 0, 0, 0, 0]).await.unwrap();
        let mut secure = SslStream::new(Ssl::new(server.context()).unwrap(), socket).unwrap();
        Pin::new(&mut secure).accept().await.unwrap();
        read_headers(&mut secure).await;
        secure
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await
            .unwrap();
    });
    let client = Client::with_tls(&format!("socks4a://proxy-user@{address}"), tls).unwrap();
    assert_eq!(
        client
            .get("https://localhost/")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "{}"
    );
    task.await.unwrap();
}

#[tokio::test]
async fn deadline_includes_stream_body_and_untrusted_certificates_are_rejected() {
    let (server, tls) = server_tls();
    let server = Arc::new(server);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        for _ in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let server = server.clone();
            tokio::spawn(async move {
                let mut secure =
                    SslStream::new(Ssl::new(server.context()).unwrap(), socket).unwrap();
                if Pin::new(&mut secure).accept().await.is_err() {
                    return;
                }
                read_headers(&mut secure).await;
                secure
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nx")
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_secs(2)).await;
            });
        }
    });
    let url = format!("https://localhost:{port}/");
    assert!(Client::new("").unwrap().get(&url).send().await.is_err());
    let response = Client::with_tls("", tls)
        .unwrap()
        .get(&url)
        .timeout(Duration::from_millis(500))
        .send()
        .await
        .unwrap();
    assert!(response.text().await.is_err());
    task.await.unwrap();
}
#[tokio::test]
async fn sessions_resume_within_one_account_and_are_isolated_between_clients() {
    let (server, tls) = server_tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut second_tls = tls_builder().unwrap();
    second_tls
        .cert_store_mut()
        .add_cert(server.context().certificate().unwrap().to_owned())
        .unwrap();
    let task = tokio::spawn(async move {
        let mut reused = Vec::new();
        for _ in 0..3 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut secure = SslStream::new(Ssl::new(server.context()).unwrap(), socket).unwrap();
            Pin::new(&mut secure).accept().await.unwrap();
            reused.push(secure.ssl().session_reused());
            read_headers(&mut secure).await;
            secure
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
            secure.flush().await.unwrap();
        }
        reused
    });
    let url = format!("https://localhost:{port}/");
    let client = Client::with_tls("", tls).unwrap();
    for _ in 0..2 {
        assert_eq!(
            client.get(&url).send().await.unwrap().text().await.unwrap(),
            "{}"
        );
    }
    assert_eq!(
        Client::with_tls("", second_tls)
            .unwrap()
            .get(&url)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "{}"
    );
    assert_eq!(task.await.unwrap(), vec![false, true, false]);
}

#[tokio::test]
async fn compressed_responses_are_decoded_like_undici_fetch() {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(b"{\"decoded\":true}").unwrap();
    let compressed = encoder.finish().unwrap();
    let (server, tls) = server_tls();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut secure = SslStream::new(Ssl::new(server.context()).unwrap(), socket).unwrap();
        Pin::new(&mut secure).accept().await.unwrap();
        assert!(read_headers(&mut secure)
            .await
            .to_ascii_lowercase()
            .contains("accept-encoding: br, gzip, deflate"));
        secure.write_all(format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",compressed.len()).as_bytes()).await.unwrap();
        secure.write_all(&compressed).await.unwrap();
    });
    let response = Client::with_tls("", tls)
        .unwrap()
        .get(format!("https://localhost:{port}/"))
        .send()
        .await
        .unwrap();
    assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
    assert_eq!(response.text().await.unwrap(), "{\"decoded\":true}");
    task.await.unwrap();
}
