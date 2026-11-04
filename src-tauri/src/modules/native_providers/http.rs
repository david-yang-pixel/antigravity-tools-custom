use super::Provider;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Method,
};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Failure {
    pub status: u16,
    pub message: String,
}
impl Failure {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Failure {}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::new(502, message)
    }
}
impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::new(502, message)
    }
}
pub fn client() -> Result<reqwest::Client, Failure> {
    crate::modules::custom_upstream::admin_http().map_err(Into::into)
}
pub fn insert(headers: &mut HeaderMap, key: &'static str, value: &str) -> Result<(), Failure> {
    headers.insert(
        key,
        HeaderValue::from_str(value)
            .map_err(|_| Failure::new(400, "Invalid provider identity header"))?,
    );
    Ok(())
}
/// Credential-bearing requests are restricted to the provider's HTTPS service family. A
/// callback, model response, or imported account cannot redirect credentials to a new host.
pub fn official_url(provider: Provider, raw: &str) -> Result<url::Url, Failure> {
    let url =
        url::Url::parse(raw).map_err(|_| Failure::new(400, "Invalid provider service URL"))?;
    let domains: &[&str] = match provider {
        Provider::Qoder => &["qoder.com", "qoder.com.cn", "qoder.sh"],
        Provider::Trae => &[
            "trae.ai",
            "trae.cn",
            "trae.com.cn",
            "traeapi.us",
            "marscode.com",
        ],
    };
    // The CN core endpoint is also documented in TRAE's official network-troubleshooting
    // forum. Permit this exact service, not arbitrary mchost.guru tenants.
    let allowed = url.host_str().is_some_and(|host| {
        domains
            .iter()
            .any(|d| host == *d || host.ends_with(&format!(".{d}")))
            || (provider == Provider::Trae && host == "trae-api-cn.mchost.guru")
    });
    if !allowed
        || url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port().is_some_and(|p| p != 443)
    {
        return Err(Failure::new(
            400,
            "Provider returned an untrusted service URL",
        ));
    }
    Ok(url)
}
pub async fn json(
    provider: Provider,
    method: Method,
    url: &str,
    headers: HeaderMap,
    body: Option<Value>,
) -> Result<Value, Failure> {
    let url = official_url(provider, url)?;
    let mut request = client()?
        .request(method, url)
        .headers(headers)
        .timeout(std::time::Duration::from_secs(20));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|_| Failure::new(502, "Provider connection failed or timed out"))?;
    read_json(response).await
}
pub async fn read_json(response: reqwest::Response) -> Result<Value, Failure> {
    let status = response.status().as_u16();
    if status == 202 {
        return Ok(Value::Null);
    }
    if !(200..300).contains(&status) {
        return Err(Failure::new(
            status,
            format!("Provider returned HTTP {status}"),
        ));
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Failure::new(502, "Provider response interrupted"))?
    {
        if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
            return Err(Failure::new(502, "Provider response exceeds size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Failure::new(502, "Provider returned invalid JSON"))?;
    if value
        .pointer("/ResponseMetadata/Error")
        .is_some_and(|v| !v.is_null())
    {
        return Err(Failure::new(
            502,
            "Provider rejected the authorization request",
        ));
    }
    Ok(value)
}
