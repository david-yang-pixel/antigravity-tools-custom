//! The two ChatGPT origins and the only hosts a request may ever be sent to.
use crate::modules::native_providers::http::Failure;
#[cfg(test)]
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub struct Endpoints {
    pub auth: String,
    pub backend: String,
}

/// Replaced only by tests, which hold the shared env lock and restore `None` afterwards.
#[cfg(test)]
static TEST: Mutex<Option<Endpoints>> = Mutex::new(None);

#[cfg(test)]
fn overridden() -> Option<Endpoints> {
    TEST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}
#[cfg(not(test))]
fn overridden() -> Option<Endpoints> {
    None
}

#[cfg(test)]
pub fn set_test(e: Option<Endpoints>) {
    *TEST.lock().unwrap_or_else(|e| e.into_inner()) = e;
}

pub fn current() -> Endpoints {
    overridden().unwrap_or_else(|| Endpoints {
        auth: "https://auth.openai.com".into(),
        backend: "https://chatgpt.com/backend-api".into(),
    })
}

fn allowed_host(host: &str) -> bool {
    host == "chatgpt.com" || host == "openai.com" || host.ends_with(".openai.com")
}

/// `base` + `path`, restricted to https on ChatGPT/OpenAI hosts. While a test override is
/// set, plain http to `127.0.0.1` is accepted too.
pub fn url(base: &str, path: &str) -> Result<url::Url, Failure> {
    let reject = || Failure::new(400, "ChatGPT endpoint is not an allowed OpenAI host");
    let joined = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let url = url::Url::parse(&joined).map_err(|_| reject())?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(reject());
    }
    let host = url.host_str().ok_or_else(reject)?;
    let production = url.scheme() == "https" && url.port().is_none() && allowed_host(host);
    let local = overridden().is_some() && url.scheme() == "http" && host == "127.0.0.1";
    (production || local).then_some(url).ok_or_else(reject)
}
