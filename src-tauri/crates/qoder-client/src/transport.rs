//! Node/undici HTTP/1.1 over OpenSSL 3.5. Each Client owns its connection pool.
//! Proxies tunnel bytes; TLS to the destination keeps the same verified profile.
use crate::Error;
use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::Bytes;
use futures::{future::BoxFuture, StreamExt};
use http::{header, HeaderMap, Method, Request, Uri};
use http_body_util::{BodyExt, Full};
use hyper_util::{
    client::legacy::{
        connect::{Connected, Connection},
        Client as HyperClient,
    },
    rt::{TokioExecutor, TokioIo},
};
use openssl::{
    ex_data::Index,
    ssl::{
        Ssl, SslConnector, SslConnectorBuilder, SslMethod, SslOptions, SslSession,
        SslSessionCacheMode, SslVersion,
    },
    x509::X509,
};
use std::{
    collections::BTreeMap,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::TcpStream,
    time::{timeout, timeout_at, Instant},
};
use tokio_openssl::SslStream;
use tower_service::Service;
use url::Url;

/// Node's tls.DEFAULT_CIPHERS, including OpenSSL's HIGH expansion (DHE/ARIA/CCM).
const NODE_CIPHERS: &str = "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:DHE-RSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA256:ECDHE-RSA-AES256-SHA384:DHE-RSA-AES256-SHA384:ECDHE-RSA-AES256-SHA256:DHE-RSA-AES256-SHA256:HIGH:!aNULL:!eNULL:!EXPORT:!DES:!RC4:!MD5:!PSK:!SRP:!CAMELLIA";

pub fn tls_connector() -> Result<SslConnector, Error> {
    Ok(tls_builder()?.build())
}
fn tls_builder() -> Result<SslConnectorBuilder, Error> {
    let mut b = SslConnector::builder(SslMethod::tls_client())?;
    b.set_min_proto_version(Some(SslVersion::TLS1_2))?;
    b.set_max_proto_version(Some(SslVersion::TLS1_3))?;
    b.set_cipher_list(NODE_CIPHERS)?;
    b.set_ciphersuites(
        "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256",
    )?;
    b.set_alpn_protos(b"\x08http/1.1")?;
    b.set_options(SslOptions::NO_COMPRESSION);
    b.set_groups_list("X25519MLKEM768:X25519:P-256:X448:P-384:P-521:ffdhe2048:ffdhe3072")?;
    // Vendored OpenSSL must also trust roots on Windows/macOS. Keep system/custom
    // CA support and add the Mozilla roots used by Node's default trust model.
    for cert in webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter() {
        b.cert_store_mut()
            .add_cert(X509::from_der(cert.as_ref())?)?;
    }
    Ok(b)
}
trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}
type BoxIo = Box<dyn Io>;
pub struct Socket(BoxIo);
impl Connection for Socket {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}
impl AsyncRead for Socket {
    fn poll_read(
        mut self: Pin<&mut Self>,
        c: &mut Context<'_>,
        b: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(c, b)
    }
}
impl AsyncWrite for Socket {
    fn poll_write(
        mut self: Pin<&mut Self>,
        c: &mut Context<'_>,
        b: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(c, b)
    }
    fn poll_flush(mut self: Pin<&mut Self>, c: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(c)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, c: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(c)
    }
}
#[derive(Clone)]
struct Connector {
    tls: Arc<SslConnector>,
    proxy: Option<Url>,
    sessions: Arc<Mutex<BTreeMap<String, SslSession>>>,
    session_key: Index<Ssl, String>,
}
impl Connector {
    async fn secure(&self, stream: BoxIo, host: &str, port: u16) -> Result<BoxIo, Error> {
        let mut ssl = self.tls.configure()?.into_ssl(host)?;
        let key = format!("{host}:{port}");
        ssl.set_ex_data(self.session_key, key.clone());
        if let Some(session) = self
            .sessions
            .lock()
            .map_err(|_| "Qoder TLS cache unavailable")?
            .get(&key)
        {
            // SAFETY: these sessions originate only from this client's own SSL_CTX,
            // and are selected by the same verified SNI host and destination port.
            unsafe {
                ssl.set_session(session)?;
            }
        }
        let mut stream = SslStream::new(ssl, stream)?;
        Pin::new(&mut stream).connect().await?;
        Ok(Box::new(stream))
    }
    async fn connect(&self, uri: Uri) -> Result<TokioIo<Socket>, Error> {
        let host = uri
            .host()
            .ok_or("Missing destination")?
            .trim_matches(['[', ']']);
        if uri.scheme_str() != Some("https") {
            return Err("Qoder transport requires HTTPS".into());
        }
        let port = uri.port_u16().unwrap_or(443);
        let mut stream: BoxIo = if let Some(proxy) = &self.proxy {
            let proxy_host = proxy
                .host_str()
                .ok_or("Missing proxy host")?
                .trim_matches(['[', ']']);
            let proxy_port = proxy.port_or_known_default().unwrap_or(1080);
            let tcp = TcpStream::connect((proxy_host, proxy_port)).await?;
            tcp.set_nodelay(true)?;
            let mut socket: BoxIo = Box::new(tcp);
            let user = percent_encoding::percent_decode_str(proxy.username()).decode_utf8()?;
            let pass = percent_encoding::percent_decode_str(proxy.password().unwrap_or(""))
                .decode_utf8()?;
            match proxy.scheme() {
                "socks5" | "socks5h" => {
                    // socks5 resolves locally, socks5h leaves DNS to the egress node.
                    let target = if proxy.scheme() == "socks5" {
                        tokio::net::lookup_host((host, port))
                            .await?
                            .next()
                            .ok_or("DNS returned no address")?
                            .to_string()
                    } else if host.contains(':') {
                        format!("[{host}]:{port}")
                    } else {
                        format!("{host}:{port}")
                    };
                    let socket = if user.is_empty() && pass.is_empty() {
                        tokio_socks::tcp::Socks5Stream::connect_with_socket(socket, target.as_str())
                            .await?
                    } else {
                        tokio_socks::tcp::Socks5Stream::connect_with_password_and_socket(
                            socket,
                            target.as_str(),
                            &user,
                            &pass,
                        )
                        .await?
                    };
                    Box::new(socket.into_inner()) as BoxIo
                }
                "socks4" | "socks4a" => {
                    let target = if proxy.scheme() == "socks4" {
                        tokio::net::lookup_host((host, port))
                            .await?
                            .find(|a| a.is_ipv4())
                            .ok_or("SOCKS4 requires IPv4")?
                            .to_string()
                    } else {
                        format!("{host}:{port}")
                    };
                    Box::new(
                        tokio_socks::tcp::Socks4Stream::connect_with_userid_and_socket(
                            socket,
                            target.as_str(),
                            &user,
                        )
                        .await?
                        .into_inner(),
                    ) as BoxIo
                }
                "http" | "https" => {
                    if proxy.scheme() == "https" {
                        socket = self.secure(socket, proxy_host, proxy_port).await?;
                    }
                    let target = if host.contains(':') {
                        format!("[{host}]:{port}")
                    } else {
                        format!("{host}:{port}")
                    };
                    let mut headers = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
                    if !user.is_empty() || !pass.is_empty() {
                        headers.push_str(&format!(
                            "Proxy-Authorization: Basic {}\r\n",
                            STANDARD.encode(format!("{user}:{pass}"))
                        ));
                    }
                    headers.push_str("\r\n");
                    socket.write_all(headers.as_bytes()).await?;
                    socket.flush().await?;
                    let mut response = Vec::new();
                    // Read exactly the CONNECT headers, leaving TLS bytes untouched.
                    loop {
                        response.push(socket.read_u8().await?);
                        if response.ends_with(b"\r\n\r\n") {
                            break;
                        }
                        if response.len() > 16384 {
                            return Err("Proxy response too large".into());
                        }
                    }
                    let status = std::str::from_utf8(&response)?.lines().next().unwrap_or("");
                    if status.split_whitespace().nth(1) != Some("200") {
                        return Err("Proxy refused tunnel".into());
                    }
                    socket
                }
                _ => return Err("Unsupported Qoder proxy scheme".into()),
            }
        } else {
            let tcp = TcpStream::connect((host, port)).await?;
            tcp.set_nodelay(true)?;
            Box::new(tcp)
        };
        stream = self.secure(stream, host, port).await?;
        Ok(TokioIo::new(Socket(stream)))
    }
}
impl Service<Uri> for Connector {
    type Response = TokioIo<Socket>;
    type Error = Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, uri: Uri) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            timeout(Duration::from_secs(15), this.connect(uri))
                .await
                .map_err(|_| "Qoder connection timed out")?
        })
    }
}
#[derive(Clone)]
pub struct Client {
    inner: Option<HyperClient<Connector, Full<Bytes>>>,
    legacy: Option<reqwest::Client>,
}
impl Client {
    pub fn new(proxy: &str) -> Result<Self, Error> {
        Self::with_tls(proxy, tls_builder()?)
    }
    pub fn legacy(proxy: &str) -> Result<Self, Error> {
        let builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(2);
        let legacy = if proxy.is_empty() {
            builder.no_proxy()
        } else {
            builder.proxy(reqwest::Proxy::all(proxy)?)
        }
        .build()?;
        Ok(Self {
            inner: None,
            legacy: Some(legacy),
        })
    }
    fn with_tls(proxy: &str, mut tls: SslConnectorBuilder) -> Result<Self, Error> {
        let proxy = if proxy.is_empty() {
            None
        } else {
            let p = Url::parse(proxy)?;
            if !matches!(
                p.scheme(),
                "http" | "https" | "socks5" | "socks5h" | "socks4" | "socks4a"
            ) || p.host_str().is_none()
            {
                return Err("Invalid outbound proxy".into());
            }
            Some(p)
        };
        let mut builder = HyperClient::builder(TokioExecutor::new());
        // Never replay a signed inference automatically on a different connection.
        builder
            .retry_canceled_requests(false)
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(Duration::from_secs(300));
        let sessions: Arc<Mutex<BTreeMap<String, SslSession>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let saved = sessions.clone();
        let session_key = Ssl::new_ex_index::<String>()?;
        tls.set_session_cache_mode(SslSessionCacheMode::CLIENT);
        tls.set_new_session_callback(move |ssl, session| {
            if let (Some(key), Ok(mut cache)) = (ssl.ex_data(session_key), saved.lock()) {
                if cache.len() >= 100 {
                    cache.pop_first();
                }
                cache.insert(key.clone(), session);
            }
        });
        Ok(Self {
            inner: Some(builder.build(Connector {
                tls: Arc::new(tls.build()),
                proxy,
                sessions,
                session_key,
            })),
            legacy: None,
        })
    }
    pub fn request(&self, method: Method, url: impl AsRef<str>) -> RequestBuilder {
        RequestBuilder {
            client: self.clone(),
            method,
            url: url.as_ref().to_owned(),
            headers: HeaderMap::new(),
            body: Vec::new(),
            timeout: Duration::from_secs(20),
            error: None,
        }
    }
    pub fn get(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::GET, url)
    }
    pub fn post(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::POST, url)
    }
    pub fn head(&self, url: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::HEAD, url)
    }
}
pub struct RequestBuilder {
    client: Client,
    method: Method,
    url: String,
    headers: HeaderMap,
    body: Vec<u8>,
    timeout: Duration,
    error: Option<serde_json::Error>,
}
impl RequestBuilder {
    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.headers.extend(headers);
        self
    }
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }
    pub fn json(mut self, value: &serde_json::Value) -> Self {
        match serde_json::to_vec(value) {
            Ok(b) => self.body = b,
            Err(e) => self.error = Some(e),
        }
        self.headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        self
    }
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    pub async fn send(mut self) -> Result<reqwest::Response, Error> {
        if let Some(e) = self.error {
            return Err(e.into());
        }
        if let Some(client) = &self.client.legacy {
            return Ok(client
                .request(self.method, &self.url)
                .headers(self.headers)
                .body(self.body)
                .timeout(self.timeout)
                .send()
                .await?);
        }
        let deadline = Instant::now() + self.timeout;
        for (key, value) in [
            ("connection", "keep-alive"),
            ("accept", "*/*"),
            ("accept-language", "*"),
            ("sec-fetch-mode", "cors"),
            ("user-agent", "undici"),
            ("accept-encoding", "br, gzip, deflate"),
        ] {
            self.headers
                .entry(key)
                .or_insert(header::HeaderValue::from_static(value));
        }
        let mut request = Request::builder()
            .method(self.method)
            .uri(&self.url)
            .body(Full::new(Bytes::from(self.body)))?;
        *request.headers_mut() = self.headers;
        let response = timeout_at(
            deadline,
            self.client
                .inner
                .as_ref()
                .ok_or("Qoder transport unavailable")?
                .request(request),
        )
        .await
        .map_err(|_| "Qoder request timed out")??;
        let (mut parts, body) = response.into_parts();
        let encoding = parts
            .headers
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let stream = async_stream::stream! {
            let mut data = body.into_data_stream();
            loop {
                match timeout_at(deadline, data.next()).await {
                    Ok(Some(Ok(bytes))) => yield Ok::<_, io::Error>(bytes),
                    Ok(None) => break,
                    Ok(Some(Err(_))) => { yield Err(io::Error::other("Qoder response interrupted")); break; }
                    Err(_) => { yield Err(io::Error::new(io::ErrorKind::TimedOut, "Qoder response timed out")); break; }
                }
            }
        };
        let stream = Box::pin(stream);
        let mut reader: Pin<Box<dyn AsyncRead + Send>> =
            Box::pin(tokio_util::io::StreamReader::new(stream));
        for encoding in encoding
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .rev()
        {
            let buffered = tokio::io::BufReader::new(reader);
            reader = match encoding {
                "gzip" | "x-gzip" => Box::pin(async_compression::tokio::bufread::GzipDecoder::new(
                    buffered,
                )),
                "deflate" => Box::pin(async_compression::tokio::bufread::ZlibDecoder::new(
                    buffered,
                )),
                "br" => Box::pin(async_compression::tokio::bufread::BrotliDecoder::new(
                    buffered,
                )),
                "identity" => Box::pin(buffered),
                _ => return Err("Unsupported Qoder response compression".into()),
            };
        }
        if !encoding.is_empty() {
            parts.headers.remove(header::CONTENT_ENCODING);
            parts.headers.remove(header::CONTENT_LENGTH);
        }
        Ok(http::Response::from_parts(
            parts,
            reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(reader)),
        )
        .into())
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
