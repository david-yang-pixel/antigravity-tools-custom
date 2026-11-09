//! Ruozhen's account-first portal uses a private cookie session per submission.
//! Never put this session in the shared HTTP client's cookie jar.
use super::{exported_account, now, Batch, LoginCredentials, Progress, Service};
use reqwest::header::{HeaderMap, COOKIE, SET_COOKIE};
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};

fn cookies(previous: &str, headers: &HeaderMap) -> Result<String, String> {
    let mut jar = BTreeMap::new();
    for pair in previous.split("; ").chain(
        headers
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next()),
    ) {
        if let Some((name, value)) = pair.split_once('=') {
            if name.is_empty() || name.len() > 256 || pair.len() > 16384 {
                return Err("Invalid reauthorization session".into());
            }
            jar.insert(name.to_owned(), value.to_owned());
        }
    }
    let value = jar
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    if value.is_empty() || value.len() > 16384 {
        return Err("Reauthorization service did not return a valid session".into());
    }
    Ok(value)
}

async fn response_json(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let bytes = super::super::chatgpt::client::read_bytes(response)
        .await
        .map_err(|_| "Cannot read reauthorization response")?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        if status.is_success() {
            "Invalid reauthorization response".to_owned()
        } else {
            format!("Reauthorization service returned HTTP {}", status.as_u16())
        }
    })?;
    if value["code"] == "cdk_required" || value["cdk_required"] == true {
        return Err(
            "This merchant requires a CDK for this account; edit the login credentials and add it"
                .into(),
        );
    }
    if !status.is_success() {
        return Err(format!(
            "Reauthorization service returned HTTP {}",
            status.as_u16()
        ));
    }
    if value["ok"] == false || value.get("error").is_some_and(|e| !e.is_null() && e != "") {
        // Error messages and task logs can contain login material. Do not echo them.
        return Err(
            "Reauthorization service rejected the request; check the credentials and CDK".into(),
        );
    }
    Ok(value)
}

pub(super) async fn create(
    client: &reqwest::Client,
    login: &LoginCredentials,
    base: String,
) -> Result<Batch, String> {
    let mut payload = json!({
        "text": format!("{}----{}----{}", login.email, login.password, login.two_factor),
        "account_first": true,
    });
    if !login.authorization.cdk.trim().is_empty() {
        payload["code"] = json!(login.authorization.cdk.trim());
    }
    let response = client
        .post(format!("{base}/api/submit"))
        .timeout(Duration::from_secs(120))
        .json(&payload)
        .send()
        .await
        .map_err(|_| "Cannot submit reauthorization; retry is delayed to avoid duplicate jobs")?;
    let headers = response.headers().clone();
    let value = response_json(response).await?;
    let created = value["created"].as_array()
        .filter(|items| items.len() == 1)
        .ok_or("The merchant did not create exactly one task; check the account on its authorization website")?;
    let id = created[0]["task_id"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or("Reauthorization service returned an invalid task")?;
    Ok(Batch {
        service: Service::Ruozhen,
        id: id.to_string(),
        token: cookies("", &headers)?,
        base,
        created_at: now(),
        node: None,
    })
}

pub(super) async fn poll(client: &reqwest::Client, batch: &mut Batch) -> Result<Progress, String> {
    if batch.id.parse::<u64>().ok().filter(|id| *id > 0).is_none() {
        return Err("Invalid reauthorization task".into());
    }
    let response = client
        .get(format!("{}/api/tasks/{}", batch.base, batch.id))
        .header(COOKIE, &batch.token)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|_| "Cannot query reauthorization task")?;
    if matches!(response.status().as_u16(), 401 | 403 | 404) {
        return Ok(Progress::Failed(
            "Merchant session or task expired; retry reauthorization".into(),
        ));
    }
    batch.token = cookies(&batch.token, response.headers())?;
    let value = response_json(response).await?;
    let task = &value["task"];
    if task["id"].as_u64().map(|id| id.to_string()).as_deref() != Some(batch.id.as_str()) {
        return Err("Reauthorization service returned a different task".into());
    }
    match task["status"].as_str() {
        Some("pending" | "running") => return Ok(Progress::Pending),
        Some("failed" | "cancelled") => return Ok(Progress::Failed(
            "Reauthorization failed; check the login credentials or account status on the merchant website".into(),
        )),
        Some("success") => {}
        _ => return Err("Invalid reauthorization task status".into()),
    }
    let response = client
        .get(format!(
            "{}/api/download/{}?format=sub2api",
            batch.base, batch.id
        ))
        .header(COOKIE, &batch.token)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|_| "Cannot download reauthorized Sub2 JSON")?;
    batch.token = cookies(&batch.token, response.headers())?;
    let export = response_json(response).await?;
    Ok(Progress::Ready(exported_account(&export)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Bytes,
        http::{HeaderMap, Uri},
        Json, Router,
    };
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn cookie_sessions_are_private_persisted_and_rotated_without_reposting_credentials() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let log = calls.clone();
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let log = log.clone();
            async move {
                let path = uri.path_and_query().unwrap().to_string();
                log.lock().unwrap().push(path.clone());
                let mut out = HeaderMap::new();
                let value = match path.as_str() {
                    "/api/submit" => {
                        assert!(!headers.contains_key(COOKIE));
                        let value: Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(value["text"], "a@example.com----private-password----JBSWY3DPEHPK3PXP");
                        assert_eq!(value["code"], "private-cdk");
                        assert_eq!(value["account_first"], true);
                        out.insert(SET_COOKIE, "session=private-one; HttpOnly; Path=/".parse().unwrap());
                        json!({"ok":true,"created":[{"task_id":42,"account_id":8}]})
                    }
                    "/api/tasks/42" => {
                        let cookie = headers[COOKIE].to_str().unwrap();
                        let status = if cookie == "session=private-one" { "running" } else {
                            assert_eq!(cookie, "session=private-two"); "success"
                        };
                        out.insert(SET_COOKIE, "session=private-two; HttpOnly; Path=/".parse().unwrap());
                        json!({"ok":true,"task":{"id":42,"status":status}})
                    }
                    "/api/download/42?format=sub2api" => {
                        assert_eq!(headers[COOKIE], "session=private-two");
                        json!({"accounts":[{"platform":"openai","credentials":{"access_token":"private-token"}}]})
                    }
                    _ => panic!("unexpected request"),
                };
                (out, Json(value))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        let mut login =
            super::super::parse("a@example.com----private-password----JBSWY3DPEHPK3PXP")
                .unwrap()
                .remove(0);
        login.authorization = super::super::ServiceConfig {
            service: Service::Ruozhen,
            cdk: "private-cdk".into(),
        };
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut batch = create(&client, &login, base).await.unwrap();
        assert!(matches!(
            poll(&client, &mut batch).await.unwrap(),
            Progress::Pending
        ));
        let mut resumed: Batch =
            serde_json::from_str(&serde_json::to_string(&batch).unwrap()).unwrap();
        assert!(matches!(
            poll(&client, &mut resumed).await.unwrap(),
            Progress::Ready(_)
        ));
        // Another submission on the same client must not inherit this job's session.
        create(&client, &login, resumed.base).await.unwrap();
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|p| *p == "/api/submit")
                .count(),
            2
        );
        server.abort();
    }

    #[tokio::test]
    async fn cdk_errors_do_not_disclose_the_merchants_response() {
        let app = Router::new().fallback(|| async {
            (axum::http::StatusCode::FORBIDDEN, Json(json!({"ok":false,"code":"cdk_required","error":"private-password private-cdk"})))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        let login = super::super::parse("a@example.com----private-password----JBSWY3DPEHPK3PXP")
            .unwrap()
            .remove(0);
        let error = create(
            &reqwest::Client::builder().no_proxy().build().unwrap(),
            &login,
            base,
        )
        .await
        .err()
        .unwrap();
        assert!(error.contains("requires a CDK"));
        assert!(!error.contains("private-password") && !error.contains("private-cdk"));
        server.abort();
    }
}
