//! Shared normalized-Chat response delivery. Provider transports only supply events;
//! protocol output, streaming/non-streaming parity and error boundaries live here.
use super::chat_codec as codec;
use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures::{stream::BoxStream, StreamExt};
use serde_json::Value;
pub(crate) type Events = BoxStream<'static, Result<String, String>>;

pub(crate) struct Relay {
    pub protocol: codec::Protocol,
    pub alias: String,
    pub namespaced: codec::Namespaced,
    pub headers: HeaderMap,
    pub error_code: &'static str,
}
impl Relay {
    fn failed(&self, message: &str) -> Response {
        tracing::warn!(
            "Chat upstream response failed: {}",
            crate::proxy::upstream::client::sanitize_error_for_log(message)
        );
        super::error_response(
            StatusCode::BAD_GATEWAY,
            self.error_code,
            "The upstream response failed or was incomplete.",
        )
    }
    fn decorate(&self, mut response: Response) -> Response {
        response.headers_mut().extend(self.headers.clone());
        response
    }
    pub async fn run(self, mut events: Events, streaming: bool) -> Response {
        let mut output =
            codec::Output::new(self.protocol, &self.alias).with_namespaced(self.namespaced.clone());
        if streaming {
            let mut initial = output.start();
            while !output.has_output() {
                let data = match events.next().await {
                    Some(Ok(data)) if data != "[DONE]" => data,
                    Some(Err(message)) => return self.failed(&message),
                    _ => return self.failed("Upstream ended before producing a completion"),
                };
                if data.trim().is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&data) else {
                    return self.failed("Invalid upstream event");
                };
                match output.push(value) {
                    Ok(frames) => initial.extend(frames),
                    Err(message) => return self.failed(&message),
                }
            }
            let body = async_stream::try_stream! {
                for frame in initial { yield bytes::Bytes::from(frame); }
                while let Some(data) = events.next().await {
                    let data = data.map_err(std::io::Error::other)?;
                    if data == "[DONE]" {
                        if let Some(chunk) = output.implicit_finish() { for frame in output.push(chunk).map_err(std::io::Error::other)? { yield bytes::Bytes::from(frame); } }
                        break;
                    }
                    if data.trim().is_empty() { continue; }
                    let value: Value = serde_json::from_str(&data).map_err(|_| std::io::Error::other("Invalid upstream stream event"))?;
                    for frame in output.push(value).map_err(std::io::Error::other)? { yield bytes::Bytes::from(frame); }
                }
                let (_, frames) = output.end().map_err(std::io::Error::other)?;
                for frame in frames { yield bytes::Bytes::from(frame); }
            };
            return self.decorate(
                Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header("x-accel-buffering", "no")
                    .body(Body::from_stream(
                        body.map(|item: Result<bytes::Bytes, std::io::Error>| item),
                    ))
                    .unwrap(),
            );
        }
        while let Some(data) = events.next().await {
            let data = match data {
                Ok(data) => data,
                Err(message) => return self.failed(&message),
            };
            if data == "[DONE]" {
                if let Some(chunk) = output.implicit_finish() {
                    if let Err(message) = output.push(chunk) {
                        return self.failed(&message);
                    }
                }
                break;
            }
            if data.trim().is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                return self.failed("Invalid upstream event");
            };
            if let Err(message) = output.push(value) {
                return self.failed(&message);
            }
        }
        match output.end() {
            Ok((value, _)) => self.decorate(Json(value).into_response()),
            Err(message) => self.failed(&message),
        }
    }
}
