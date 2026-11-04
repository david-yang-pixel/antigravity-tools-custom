//! TRAE PKCE authorization and first-party model, inference and entitlement APIs.
use super::{
    http::{self, Failure},
    *,
};
use p256::pkcs8::{EncodePublicKey, LineEnding};
use reqwest::{header::HeaderMap, Method};
use serde_json::json;

pub fn version() -> String {
    std::env::var("ABV_TRAE_VERSION").unwrap_or_else(|_| "3.5.66".into())
}
pub fn auth_hosts(realm: Realm) -> &'static [&'static str] {
    match realm {
        Realm::Cn => &["https://api.trae.cn", "https://api.trae.com.cn"],
        Realm::Global => &[
            "https://api.trae.ai",
            "https://growsg-normal.trae.ai",
            "https://grow-normal.trae.ai",
            "https://grow-normal.traeapi.us",
        ],
    }
}
pub fn client_id(solo: bool) -> &'static str {
    if solo {
        "en1oxy7wnw8j9n"
    } else {
        "ono9krqynydwx5"
    }
}
pub fn core_host(realm: Realm, region: &str) -> &'static str {
    match realm {
        Realm::Cn => "https://trae-api-cn.mchost.guru",
        Realm::Global if matches!(region, "us" | "usttp") => "https://coreva-normal.trae.ai",
        Realm::Global => "https://coresg-normal.trae.ai",
    }
}
pub fn function(a: &Account, model: &str) -> &'static str {
    if a.credentials.platform.ends_with("solo")
        || a.credentials.inference_function == "solo_work_lite"
    {
        "solo_work_lite"
    } else if model.eq_ignore_ascii_case("auto") {
        "inline_chat"
    } else {
        "chat_v3"
    }
}
pub fn headers(a: &Account) -> Result<HeaderMap, Failure> {
    let os = match std::env::consts::OS {
        "macos" => "mac",
        "windows" => "windows",
        _ => "linux",
    };
    let mut h = HeaderMap::new();
    for (key, value) in [
        (
            "authorization",
            format!("Cloud-IDE-JWT {}", a.credentials.access_token),
        ),
        ("x-cloudide-token", a.credentials.access_token.clone()),
        ("x-ide-token", a.credentials.access_token.clone()),
        ("x-uid", a.uid.clone()),
        ("x-device-id", a.credentials.device_id.clone()),
        ("x-machine-id", a.credentials.machine_id.clone()),
        ("x-ide-version", version()),
        ("x-app-version", version()),
        ("x-ide-version-type", "stable".into()),
        ("x-app-id", "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8".into()),
        ("x-device-type", os.into()),
        ("x-device-brand", "PC".into()),
        ("request-traffic-type", "prod".into()),
        ("user-agent", format!("Trae/{}", version())),
        ("content-type", "application/json".into()),
        ("accept", "application/json".into()),
    ] {
        if !value.is_empty() {
            http::insert(&mut h, key, &value)?;
        }
    }
    Ok(h)
}
pub async fn post(a: &Account, host: &str, path: &str, body: Value) -> Result<Value, Failure> {
    let value = http::json(
        Provider::Trae,
        Method::POST,
        &format!("{}{path}", host.trim_end_matches('/')),
        headers(a)?,
        Some(body),
    )
    .await?;
    if let Some(code) = value.get("code").or_else(|| value.get("Code")) {
        if code != 0 && code != "0" && !code.is_null() {
            return Err(Failure::new(502, "TRAE rejected the service request"));
        }
    }
    Ok(value)
}
pub fn apply_tokens(a: &mut Account, value: &Value) -> Result<(), Failure> {
    let v = payload(value);
    let token = text(
        v,
        &[
            "Token",
            "AccessToken",
            "accessToken",
            "access_token",
            "token",
        ],
    );
    if token.is_empty() {
        return Err(Failure::new(
            401,
            "TRAE returned no access token; authorize the account again",
        ));
    }
    a.credentials.access_token = token;
    let refresh = text(v, &["RefreshToken", "refreshToken", "refresh_token"]);
    if !refresh.is_empty() {
        a.credentials.refresh_token = refresh;
    }
    a.expires_at = ["TokenExpireAt", "tokenExpireAt", "expiresAt", "expires_at"]
        .iter()
        .find_map(|key| v.get(*key).and_then(epoch))
        .or_else(|| {
            number(v, &["TokenExpireDuration", "tokenExpireDuration"]).map(|n| now() + n as i64)
        })
        .unwrap_or(now() + 86400);
    Ok(())
}
fn hosts(a: &Account) -> Vec<String> {
    let mut hosts = Vec::new();
    if !a.credentials.auth_host.is_empty() {
        hosts.push(a.credentials.auth_host.clone());
    }
    for host in auth_hosts(a.realm) {
        if !hosts.iter().any(|h| h == host) {
            hosts.push((*host).into());
        }
    }
    hosts
}
pub async fn refresh(a: &mut Account) -> Result<(), Failure> {
    if a.credentials.refresh_token.is_empty() {
        return Err(Failure::new(
            401,
            "TRAE requires browser authorization again",
        ));
    }
    let body = json!({"ClientID":a.credentials.client_id,"RefreshToken":a.credentials.refresh_token,"ClientSecret":"-","UserID":""});
    let mut last = Failure::new(502, "TRAE token refresh failed");
    for host in hosts(a) {
        match post(
            a,
            &host,
            "/cloudide/api/v3/trae/oauth/ExchangeToken",
            body.clone(),
        )
        .await
        {
            Ok(value) => {
                apply_tokens(a, &value)?;
                a.credentials.auth_host = host;
                return Ok(());
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}
pub async fn identity(a: &mut Account) -> Result<(), Failure> {
    let mut last = Failure::new(401, "TRAE did not return an account identity");
    for host in hosts(a) {
        match post(
            a,
            &host,
            "/cloudide/api/v3/trae/GetUserInfo",
            json!({"ReqSource":"IDE","IDEVersion":version()}),
        )
        .await
        {
            Ok(value) => {
                let data = payload(&value);
                let v = data
                    .get("UserInfo")
                    .or_else(|| data.get("userInfo"))
                    .unwrap_or(data);
                let uid = text(v, &["UserID", "userId", "uid", "id"]);
                if uid.is_empty() {
                    continue;
                }
                a.uid = uid;
                a.nickname = text(
                    v,
                    &["ScreenName", "screenName", "nickname", "Email", "email"],
                );
                a.credentials.auth_host = host;
                return Ok(());
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}
pub async fn exchange_code(
    a: &mut Account,
    code: &str,
    verifier: &str,
    user_tag: &str,
) -> Result<(), Failure> {
    let private = p256::SecretKey::random(&mut rand::rngs::OsRng);
    let public = private
        .public_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|_| "Cannot generate TRAE device key")?;
    let body = json!({"ClientID":a.credentials.client_id,"AuthCode":code,"CodeVerifier":verifier,"IDEVersion":version(),
        "DeviceInfo":{"DeviceID":a.credentials.device_id,"MachineID":a.credentials.machine_id,
        "PlatformCode":if a.credentials.platform.ends_with("solo") { "SOLO_PC" } else { "IDE_PC" },
        "DeviceType":"PC","DeviceName":"PC","DeviceModel":"PC","ClientVersion":version(),
        "DevicePublicKey":public,"DeviceBrand":"PC","DeviceCPU":std::env::consts::ARCH,"OSInfo":std::env::consts::OS,"OSVersion":std::env::consts::OS}});
    let mut hosts = hosts(a);
    if user_tag.to_ascii_lowercase().contains("us") && a.realm == Realm::Global {
        hosts.insert(0, "https://grow-normal.traeapi.us".into());
    }
    let mut last = Failure::new(401, "TRAE authorization code exchange failed");
    for host in hosts {
        match post(a, &host, "/trae/api/v3/oauth/ExchangeToken", body.clone()).await {
            Ok(value) => {
                apply_tokens(a, &value)?;
                a.credentials.auth_host = host;
                return Ok(());
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}
pub async fn models(a: &Account) -> Result<Vec<Model>, Failure> {
    let host = if a.credentials.api_host.is_empty() {
        core_host(a.realm, "")
    } else {
        &a.credentials.api_host
    };
    let first = function(a, "");
    let functions = if a.credentials.platform.ends_with("solo") {
        vec![first]
    } else if first == "solo_work_lite" {
        vec![first, "chat_v3"]
    } else {
        vec![first, "solo_work_lite"]
    };
    let mut last = Failure::new(502, "TRAE returned no model catalog");
    for function in functions {
        let result = post(a, host, "/api/ide/v1/get_detail_param", json!({"function":function,"config_names":null,"need_prompt":false,"current_config_info":null,"poly_prompt":true,"mode_type":null,"agent_type":null})).await;
        match result {
            Ok(value) => match parse_models(&value) {
                Ok(models) => {
                    let _ = store::update(a.provider, &a.id, Some(&a.revision), |a| {
                        a.credentials.inference_function = function.into()
                    });
                    return Ok(models);
                }
                Err(error) => last = error,
            },
            Err(error) if error.status == 401 || error.status == 403 => return Err(error),
            Err(error) => last = error,
        }
    }
    Err(last)
}
pub fn parse_models(value: &Value) -> Result<Vec<Model>, Failure> {
    let rows = payload(value)
        .get("config_info_list")
        .and_then(Value::as_array)
        .ok_or("TRAE returned an unrecognized model catalog")?;
    let mut models = BTreeMap::new();
    for row in rows {
        let id = text(row, &["config_name"]);
        if !crate::modules::custom_upstream::valid_model_id(&id) {
            continue;
        }
        models.insert(
            id.clone(),
            Model {
                id,
                name: text(&row["display_config"], &["display_name"]),
                context_window: number(row, &["max_input_tokens"]).unwrap_or(0.0).max(0.0) as u64,
                vision: row["is_vl"] == true,
                reasoning: row["is_reasoning"] == true,
                is_free: false,
            },
        );
    }
    if models.is_empty() {
        return Err("TRAE returned no models".into());
    }
    Ok(models.into_values().take(2000).collect())
}
pub async fn quota(a: &Account) -> Result<Quota, Failure> {
    let mut pay_hosts = Vec::new();
    let auth_host = if a.credentials.auth_host.is_empty() {
        auth_hosts(a.realm)[0]
    } else {
        &a.credentials.auth_host
    };
    if let Ok(value) = post(
        a,
        auth_host,
        "/cloudide/api/v3/trae/CheckLogin",
        json!({"GetAIPayHost":true,"ReqSource":"IDE"}),
    )
    .await
    {
        let data = payload(&value);
        let mut host = text(data, &["AIPayHost", "AiPayHost", "ai_pay_host"]);
        if host.is_empty() {
            host = data
                .get("AIPayHost")
                .map(|v| text(v, &["Host", "host"]))
                .unwrap_or_default();
        }
        if !host.is_empty() {
            let host = if host.starts_with("https://") {
                host
            } else {
                format!("https://{host}")
            };
            http::official_url(Provider::Trae, &host)?;
            pay_hosts.push(host);
        }
    }
    pay_hosts.push(auth_hosts(a.realm)[0].into());
    let mut last = Failure::new(
        502,
        "TRAE quota is unavailable; the previous balance has been retained",
    );
    for host in pay_hosts {
        for path in [
            "/trae/api/v2/pay/ide_user_ent_usage",
            "/trae/api/v1/pay/ide_user_ent_usage",
        ] {
            match post(a, &host, path, json!({"require_usage":true})).await {
                Ok(value) => match parse_quota(&value) {
                    Ok(quota) => return Ok(quota),
                    Err(error) => last = error,
                },
                Err(error) => last = error,
            }
        }
    }
    Err(last)
}
pub fn parse_quota(value: &Value) -> Result<Quota, Failure> {
    let v = payload(value);
    let rows = v
        .get("user_entitlement_pack_list")
        .or_else(|| v.get("userEntitlementPackList"))
        .and_then(Value::as_array)
        .ok_or("TRAE quota response is unrecognized; the previous balance has been retained")?;
    let mut packs = Vec::new();
    let mut recognized = rows.is_empty();
    for (index, row) in rows.iter().enumerate() {
        let base = row
            .get("entitlement_base_info")
            .or_else(|| row.get("entitlementBaseInfo"))
            .unwrap_or(row);
        let subscription = base
            .pointer("/product_extra/subscription_extra")
            .or_else(|| base.pointer("/productExtra/subscriptionExtra"));
        let quota = [
            base.get("quota"),
            subscription.and_then(|v| v.get("quota")),
            row.get("quota"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let Some(total) = quota.iter().find_map(|v| {
            number(
                v,
                &[
                    "credits_limit",
                    "creditsLimit",
                    "basic_usage_limit",
                    "basicUsageLimit",
                ],
            )
        }) else {
            continue;
        };
        recognized = true;
        let expires_at = base
            .get("end_time")
            .or_else(|| base.get("endTime"))
            .or_else(|| row.get("expire_time"))
            .and_then(epoch);
        if expires_at.is_some_and(|time| time <= now()) {
            continue;
        }
        let used = row
            .get("usage")
            .and_then(|v| {
                number(
                    v,
                    &[
                        "credits_amount",
                        "creditsAmount",
                        "basic_usage_amount",
                        "basicUsageAmount",
                    ],
                )
            })
            .or_else(|| {
                quota
                    .iter()
                    .find_map(|v| number(v, &["credits_amount", "basic_usage_amount"]))
            })
            .map(|v| v.max(0.0));
        let unlimited = total < 0.0;
        let name = text(row, &["display_desc", "displayDesc"]);
        packs.push(QuotaPack {
            name: if name.is_empty() {
                format!("套餐 {}", index + 1)
            } else {
                name
            },
            total: if unlimited { None } else { Some(total) },
            used,
            remaining: if unlimited {
                None
            } else {
                used.map(|used| (total - used).max(0.0))
            },
            unlimited,
            expires_at,
        });
    }
    if !recognized {
        return Err(
            "TRAE quota format is unrecognized; the previous balance has been retained".into(),
        );
    }
    Ok(Quota::from_packs(
        if v["is_dollar_usage_billing"] == true || v["isDollarUsageBilling"] == true {
            "USD"
        } else {
            "credits"
        },
        packs,
    ))
}
pub async fn chat(
    a: &Account,
    model: &str,
    payload: Value,
    timeout: std::time::Duration,
) -> Result<reqwest::Response, Failure> {
    let host = if a.credentials.api_host.is_empty() {
        core_host(a.realm, "")
    } else {
        &a.credentials.api_host
    };
    let url = http::official_url(
        Provider::Trae,
        &format!("{host}/api/agent/v3/llm_utils_chat"),
    )?;
    let mut h = headers(a)?;
    http::insert(&mut h, "accept", "text/event-stream")?;
    http::insert(&mut h, "x-request-id", &uuid::Uuid::new_v4().to_string())?;
    http::client()?
        .post(url)
        .headers(h)
        .json(&super::wire::trae_request(a, model, payload)?)
        .timeout(timeout)
        .send()
        .await
        .map_err(|_| Failure::new(502, "TRAE inference connection failed"))
}
