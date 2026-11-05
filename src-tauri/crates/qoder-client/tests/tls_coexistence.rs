//! Both TLS engines must link and run in the same gateway process.
use std::time::Duration;
use tokio::{io::AsyncReadExt, net::TcpListener, time::timeout};

async fn hello(listener: TcpListener) -> Vec<u8> {
    let (mut stream, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut header = [0; 5];
    stream.read_exact(&mut header).await.unwrap();
    assert_eq!(header[0], 22);
    let mut data = vec![0; u16::from_be_bytes([header[3], header[4]]) as usize];
    stream.read_exact(&mut data).await.unwrap();
    data
}

#[tokio::test]
async fn openssl_and_boringssl_emit_independent_handshakes_in_one_process() {
    let qoder_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let browser_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let qoder_url = format!(
        "https://localhost:{}/",
        qoder_listener.local_addr().unwrap().port()
    );
    let browser_url = format!(
        "https://localhost:{}/",
        browser_listener.local_addr().unwrap().port()
    );
    let qoder = qoder_client::transport::Client::new("").unwrap();
    let browser = rquest::Client::builder()
        .no_proxy()
        .emulation(
            rquest_util::EmulationOption::builder()
                .emulation(rquest_util::Emulation::Chrome123)
                .skip_headers(true)
                .build(),
        )
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let (qoder_hello, browser_hello, _, _) = tokio::join!(
        hello(qoder_listener),
        hello(browser_listener),
        qoder.get(qoder_url).timeout(Duration::from_secs(3)).send(),
        browser.get(browser_url).send(),
    );
    // Our fixture fixes Node's full cipher order. Chrome offers a different list,
    // including GREASE, and HTTP/2 in addition to HTTP/1.1.
    fn ciphers(hello: &[u8]) -> Vec<u16> {
        let start = 4 + 2 + 32;
        let start = start + 1 + hello[start] as usize;
        let length = u16::from_be_bytes([hello[start], hello[start + 1]]) as usize;
        hello[start + 2..start + 2 + length]
            .chunks_exact(2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .collect()
    }
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/node-tls-3.5.json")).unwrap();
    assert_eq!(
        serde_json::to_value(ciphers(&qoder_hello)).unwrap(),
        fixture["profile"]["ciphers"]
    );
    assert_ne!(ciphers(&qoder_hello), ciphers(&browser_hello));
    assert!(browser_hello
        .windows(12)
        .any(|b| b == b"\x02h2\x08http/1.1"));
    assert!(qoder_hello.windows(9).any(|b| b == b"\x08http/1.1"));
}
