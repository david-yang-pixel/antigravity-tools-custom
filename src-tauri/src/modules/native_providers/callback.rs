//! Exact loopback callbacks for desktop sign-in and server-owned login browsers.
use super::Provider;
use axum::{
    extract::RawQuery,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;

pub fn loopback_port() -> u16 {
    std::env::var("ABV_TRAE_CALLBACK_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|p| *p != 0)
        .unwrap_or(18080)
}

pub fn target(server_browser: bool) -> Result<String, String> {
    let port = if server_browser {
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .map_err(|_| "TRAE_BROWSER_START_FAILED")?
            .port()
    } else {
        loopback_port()
    };
    Ok(format!("http://127.0.0.1:{port}/authorize"))
}

fn trusted_origin(origin: &str) -> bool {
    super::http::official_url(Provider::Trae, origin).is_ok_and(|url| {
        url.path() == "/"
            && url.query().is_none()
            && url.origin().ascii_serialization() == origin
            && url.host_str().is_some_and(|host| {
                ["trae.cn", "trae.com.cn", "trae.ai"]
                    .iter()
                    .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
            })
    })
}

fn reply(headers: &HeaderMap, status: StatusCode, success: bool) -> Response {
    let mut response = if status == StatusCode::NO_CONTENT {
        status.into_response()
    } else {
        (status, Json(if success { json!({"success":true}) } else {
            json!({"success":false,"error":"TRAE authorization could not be saved. Check the account page or start a new login."})
        })).into_response()
    };
    let out = response.headers_mut();
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    out.insert(
        header::VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Headers, Access-Control-Request-Method",
        ),
    );
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .filter(|origin| origin.to_str().is_ok_and(trusted_origin))
    {
        out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        out.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
        out.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        out.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            headers
                .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
                .cloned()
                .unwrap_or_else(|| HeaderValue::from_static("content-type")),
        );
        if headers
            .get("access-control-request-private-network")
            .is_some_and(|v| v == "true")
        {
            out.insert(
                "access-control-allow-private-network",
                HeaderValue::from_static("true"),
            );
        }
    }
    response
}

fn origin_allowed(headers: &HeaderMap) -> bool {
    headers
        .get(header::ORIGIN)
        .is_none_or(|origin| origin.to_str().is_ok_and(trusted_origin))
}

async fn options(headers: HeaderMap) -> Response {
    reply(
        &headers,
        if origin_allowed(&headers) {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::FORBIDDEN
        },
        false,
    )
}

async fn loopback(headers: HeaderMap, RawQuery(query): RawQuery) -> Response {
    if !origin_allowed(&headers) {
        return reply(&headers, StatusCode::FORBIDDEN, false);
    }
    let query = query.unwrap_or_default();
    if query.len() > 64 * 1024 {
        return reply(&headers, StatusCode::PAYLOAD_TOO_LARGE, false);
    }
    let accepted = super::login::receive_loopback(&query).await.is_ok();
    reply(
        &headers,
        if accepted {
            StatusCode::OK
        } else {
            StatusCode::BAD_REQUEST
        },
        accepted,
    )
}

pub fn loopback_routes() -> Router {
    Router::new().route("/authorize", get(loopback).options(options))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn callback_targets_are_exact_loopback_addresses() {
        for server_browser in [false, true] {
            let url = url::Url::parse(&target(server_browser).unwrap()).unwrap();
            assert_eq!(url.scheme(), "http");
            assert_eq!(url.host_str(), Some("127.0.0.1"));
            assert_eq!(url.path(), "/authorize");
            assert!(url.port().is_some_and(|port| port > 0));
            assert!(url.query().is_none());
            assert!(url.fragment().is_none());
            if !server_browser {
                assert_eq!(url.port(), Some(loopback_port()));
            }
        }
    }
    #[test]
    fn callback_cors_accepts_only_exact_provider_origins() {
        for origin in [
            "https://www.trae.cn",
            "https://www.trae.ai",
            "https://www.trae.com.cn",
        ] {
            assert!(trusted_origin(origin));
        }
        for origin in [
            "null",
            "http://www.trae.cn",
            "https://www.trae.cn.evil.test",
            "https://www.trae.cn/path",
            "https://user@www.trae.cn",
            "https://trae-api-cn.mchost.guru",
        ] {
            assert!(!trusted_origin(origin));
        }
    }
}
