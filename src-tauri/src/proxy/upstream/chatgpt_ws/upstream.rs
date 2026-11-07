//! One upstream Codex WebSocket bound to one ChatGPT account: handshake, liveness ping and
//! event reading.
use crate::modules::native_providers::{
    chatgpt::{client, endpoints, quota},
    http::{plain_client_for, Failure},
    Account,
};
use futures::{SinkExt, StreamExt};
use reqwest::header::{HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest_websocket::{Message, RequestBuilderExt, WebSocket};
use serde_json::Value;
use std::time::Duration;
use tokio::time::Instant;

/// Codex WebSocket protocol version announced in the handshake.
const BETA: &str = "responses_websockets=2026-02-06";

/// Why a handshake did not produce a socket.
pub(super) enum Refused {
    /// The connection failed before any answer; the same account may be tried again.
    Transport(Failure),
    /// The upstream answered with an HTTP status, classified, with its own error object.
    Answered(Failure, Option<Value>),
}

/// An upstream message during a turn.
pub(super) enum Incoming {
    Event(Value),
    Closed,
}

pub(super) struct Upstream {
    socket: WebSocket,
    /// The ChatGPT account (store id) the socket was opened for.
    pub account: String,
    last_active: Instant,
}

fn transport(message: &str) -> Failure {
    Failure {
        failover: true,
        ..Failure::new(502, message)
    }
}

/// The Codex WebSocket URL derived from the configured backend origin.
fn url() -> Result<url::Url, Failure> {
    let mut url = endpoints::url(
        &endpoints::current().backend,
        client::Endpoint::Responses.path(),
    )?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| Failure::new(400, "ChatGPT WebSocket URL is invalid"))?;
    Ok(url)
}

/// Opens one socket for `account`. Quota headers of the handshake answer are recorded
/// whether or not it upgrades.
pub(super) async fn connect(
    account: &Account,
    session: &str,
    deadline: Instant,
) -> Result<Upstream, Refused> {
    let url = url().map_err(|e| Refused::Answered(e, None))?;
    let mut headers = client::headers(
        account,
        session,
        &reqwest::header::HeaderMap::new(),
        client::Endpoint::Responses,
    )
    .map_err(|e| Refused::Answered(e, None))?;
    headers.remove(ACCEPT);
    headers.remove(CONTENT_TYPE);
    headers.insert("openai-beta", HeaderValue::from_static(BETA));
    let http = tokio::time::timeout_at(deadline, plain_client_for(account))
        .await
        .map_err(|_| {
            Refused::Answered(
                Failure::new(504, "ChatGPT egress resolution timed out"),
                None,
            )
        })?
        .map_err(|e| Refused::Answered(e, None))?;
    let handshake = http.get(url).headers(headers).upgrade().send();
    let response = match tokio::time::timeout_at(deadline, handshake).await {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => {
            return Err(Refused::Transport(transport(
                "ChatGPT WebSocket connection failed",
            )))
        }
        Err(_) => {
            return Err(Refused::Transport(transport(
                "ChatGPT WebSocket handshake timed out",
            )))
        }
    };
    quota::record(account, response.headers());
    if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        let (failure, upstream) =
            crate::proxy::upstream::native::chatgpt::rejection(account, response.into_inner())
                .await;
        return Err(Refused::Answered(failure, upstream));
    }
    let socket = response
        .into_websocket()
        .await
        .map_err(|_| Refused::Transport(transport("ChatGPT WebSocket upgrade failed")))?;
    Ok(Upstream {
        socket,
        account: account.id.clone(),
        last_active: Instant::now(),
    })
}

impl Upstream {
    /// Closes the socket normally, waiting at most one second.
    pub async fn close(self) {
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            self.socket
                .close(reqwest_websocket::CloseCode::Normal, None),
        )
        .await;
    }

    pub fn idle_for(&self) -> Duration {
        self.last_active.elapsed()
    }

    pub async fn send(&mut self, text: String) -> bool {
        self.last_active = Instant::now();
        self.socket.send(Message::Text(text)).await.is_ok()
    }

    /// Pings the upstream and waits up to `wait` for the pong.
    pub async fn ping(&mut self, wait: Duration) -> bool {
        if self
            .socket
            .send(Message::Ping(reqwest_websocket::Bytes::from_static(b"abv")))
            .await
            .is_err()
        {
            return false;
        }
        let deadline = Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, self.socket.next()).await {
                Ok(Some(Ok(Message::Pong(_)))) => {
                    self.last_active = Instant::now();
                    return true;
                }
                Ok(Some(Ok(Message::Ping(_)))) => {}
                // Anything else between turns is stale: the socket is not trusted.
                _ => return false,
            }
        }
    }

    /// The next upstream event. Control frames are absorbed; a close, a transport error or
    /// a non-JSON text frame ends the socket.
    pub async fn next(&mut self) -> Incoming {
        loop {
            let message = self.socket.next().await;
            self.last_active = Instant::now();
            let text = match message {
                Some(Ok(Message::Text(text))) => text,
                Some(Ok(Message::Binary(bytes))) => match String::from_utf8(bytes.to_vec()) {
                    Ok(text) => text,
                    Err(_) => return Incoming::Closed,
                },
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                _ => return Incoming::Closed,
            };
            return match serde_json::from_str::<Value>(&text) {
                Ok(event) if event.is_object() => Incoming::Event(event),
                _ => Incoming::Closed,
            };
        }
    }
}
