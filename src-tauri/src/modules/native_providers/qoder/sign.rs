//! COSY compatibility constants/wire layout documented by qoder2api-hub (MIT).
//! Cryptographic operations use RustCrypto/rsa; this module does not implement primitives.
use super::super::{
    http::{self, Failure},
    Account,
};
use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use reqwest::header::HeaderMap;
use rsa::{pkcs8::DecodePublicKey, Pkcs1v15Encrypt, RsaPublicKey};
use serde_json::json;
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::BTreeMap,
    sync::{LazyLock, Mutex},
};

const ALPHABET: &[u8] = b"_doRTgHZBKcGVjlvpC,@aFSx#DPuNJme&i*MzLOEn)sUrthbf%Y^w.(kIQyXqWA!";
const STANDARD_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const SERVER_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDA8iMH5c02LilrsERw9t6Pv5Nc\n4k6Pz1EaDicBMpdpxKduSZu5OANqUq8er4GM95omAGIOPOh+Nx0spthYA2BqGz+l\n6HRkPJ7S236FZz73In/KVuLnwI8JJ2CbuJap8kvheCCZpmAWpb/cPx/3Vr/J6I17\nXcW+ML9FoCI6AOvOzwIDAQAB\n-----END PUBLIC KEY-----";

pub fn version() -> String {
    std::env::var("ABV_QODER_VERSION").unwrap_or_else(|_| {
        if legacy() {
            "1.1.75"
        } else {
            qoder_client::CLI_VERSION
        }
        .into()
    })
}
/// Explicit rollback only. A failed modern signature/identity must never silently
/// downgrade an already rejected request or replay it through another protocol.
pub fn legacy() -> bool {
    std::env::var("ABV_QODER_LEGACY_PROTOCOL")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true"))
}

type WasmCache = BTreeMap<String, (String, qoder_client::signing::Signer)>;
static WASM_SIGNERS: LazyLock<Mutex<WasmCache>> = LazyLock::new(Default::default);
// One running signer plus at most three asynchronously waiting preparations.
static SIGNING_QUEUE: LazyLock<qoder_client::signing_queue::Queue> =
    LazyLock::new(|| qoder_client::signing_queue::Queue::new(4));

pub fn forget(id: &str) {
    if let Ok(mut cache) = WASM_SIGNERS.lock() {
        cache.remove(id);
    }
    if let Ok(mut cache) = SIGNERS.lock() {
        cache.remove(id);
    }
}

pub fn normalize_hostname(value: &str) -> String {
    let value = value.trim();
    fn shorten(value: &str) -> String {
        if value.len() <= 96 {
            return value.into();
        }
        let hash = format!("{:x}", Sha256::digest(value.as_bytes()));
        let prefix = value[..87].trim_end_matches(|c: char| c == '-' || c.is_ascii_whitespace());
        format!(
            "{}-{}",
            if prefix.is_empty() { "unknown" } else { prefix },
            &hash[..8]
        )
    }
    if value.bytes().all(|b| (32..=126).contains(&b)) {
        return shorten(value);
    }
    if let Ok(url::Host::Domain(ascii)) = url::Host::parse(value) {
        if !ascii.is_empty() && ascii.bytes().all(|b| (32..=126).contains(&b)) {
            return shorten(&ascii);
        }
    }
    let hash = format!("{:x}", Sha256::digest(value.as_bytes()));
    let mut safe = String::new();
    for c in value.chars() {
        let c = if c.is_ascii() && ('!'..='~').contains(&c) {
            c
        } else {
            '-'
        };
        if c != '-' || !safe.ends_with('-') {
            safe.push(c);
        }
    }
    let prefix = safe.trim_matches('-');
    shorten(&format!(
        "{}-{}",
        if prefix.is_empty() { "unknown" } else { prefix },
        &hash[..8]
    ))
}

pub async fn prepare(
    a: &Account,
    body: String,
    inference: Option<&str>,
    proxy_in_use: bool,
) -> Result<qoder_client::signing::PreparedRequest, Failure> {
    let endpoint = super::gateway(a.realm);
    if legacy() {
        let encoded = encode(body.as_bytes());
        let path = if inference.is_some() {
            "/algo/api/v2/service/pro/sse/agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1"
        } else {
            "/algo/api/v2/model/list?Encode=1"
        };
        let url = format!("{endpoint}{path}");
        let headers = headers(
            a,
            &encoded,
            &url,
            inference.unwrap_or(""),
            inference.is_some(),
        )?;
        return Ok(qoder_client::signing::PreparedRequest {
            url,
            body: Some(encoded),
            headers: headers
                .iter()
                .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_owned())))
                .collect(),
        });
    }
    let allow_direct = std::env::var("ABV_QODER_ALLOW_DIRECT_UMID")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true"));
    qoder_client::identity::check_network_policy(proxy_in_use, allow_direct)
        .map_err(|e| Failure::new(503, e.to_string()))?;
    if body.len() > qoder_client::signing::MAX_BYTES {
        return Err(Failure::new(
            413,
            "Qoder request exceeds the signing size limit",
        ));
    }
    let reservation = SIGNING_QUEUE
        .try_reserve()
        .map_err(|e| Failure::new(503, e.to_string()))?;
    let root = crate::modules::account::get_data_dir()?;
    let machine = qoder_client::identity::machine(&root, a.realm == super::super::Realm::Cn, &a.uid).await
        .map_err(|_| Failure::new(503, "Official Qoder machine identity unavailable; check the runtime helper or explicitly select the legacy protocol"))?;
    let a = a.clone();
    let model = inference.map(str::to_owned);
    reservation.run(std::time::Duration::from_secs(20), move || {
        let version = version();
        let mut identity = json!({"uid":a.uid,"security_oauth_token":a.credentials.access_token,"organization_id":a.credentials.organization_id});
        if let Some(tags) = &a.credentials.organization_tags { identity["organization_tags"] = tags.clone(); }
        if let Some(agreed) = a.credentials.data_policy_agreed { identity["data_policy_agreed"] = json!(agreed); }
        let fingerprint = format!("{:x}", Sha256::digest(format!("{}:{}:{}", a.credentials.machine_id, version, identity)));
        let mut cache = WASM_SIGNERS.lock().map_err(|_| Failure::new(503, "Qoder signer unavailable"))?;
        if cache.get(&a.id).is_none_or(|(old, _)| old != &fingerprint) {
            let signer = qoder_client::signing::Signer::new(&a.credentials.machine_id, &version, identity)
                .map_err(|_| Failure::new(502, "Cannot initialize Qoder WASM identity"))?;
            if cache.len() >= 64 { cache.clear(); }
            cache.insert(a.id.clone(), (fingerprint, signer));
        }
        let signer = &mut cache.get_mut(&a.id).ok_or("Qoder signer unavailable")?.1;
        let mut request = match &model {
            Some(model) => signer.inference(endpoint, &body, model),
            None => signer.request(endpoint, "/api/v2/model/list", "GET", Some(&body)),
        }.map_err(|_| Failure::new(502, "Qoder WASM signing failed"))?;
        // UMID middleware follows WASM signing in the official CLI.
        for (name, value) in [
            ("Cosy-Version", version), ("Cosy-ClientType", "5".into()),
            ("Cosy-MachineOS", format!("{}_{}", if std::env::consts::ARCH == "aarch64" { "aarch64" } else { "x86_64" }, if cfg!(windows) { "win32" } else if cfg!(target_os="macos") { "darwin" } else { "linux" })),
            ("Cosy-MachineCode", machine.machine_code), ("Cosy-MachineToken", machine.machine_token), ("Cosy-MachineType", machine.machine_type),
        ] { request.headers.retain(|k, _| !k.eq_ignore_ascii_case(name)); request.headers.insert(name.into(), value); }
        if model.is_some() {
            let host = normalize_hostname(&sysinfo::System::host_name().unwrap_or_default());
            if !host.is_empty() { request.headers.insert("Cosy-MachineHostname".into(), host); }
        }
        Ok(request)
    }).await.map_err(|error| Failure::new(
        if error == qoder_client::signing_queue::Error::Timeout { 504 } else { 503 },
        error.to_string(),
    ))?
}
pub fn encode(bytes: &[u8]) -> String {
    let s = STANDARD.encode(bytes);
    let n = s.len();
    let third = n / 3;
    [&s[n - third..], &s[third..n - third], &s[..third]]
        .concat()
        .bytes()
        .map(|b| {
            if b == b'=' {
                '$'
            } else {
                ALPHABET[STANDARD_ALPHABET.iter().position(|c| *c == b).unwrap()] as char
            }
        })
        .collect()
}
fn sorted(value: serde_json::Value) -> Result<String, Failure> {
    let map: BTreeMap<_, _> = value
        .as_object()
        .ok_or("Invalid COSY identity")?
        .iter()
        .collect();
    serde_json::to_string(&map).map_err(|_| "Cannot encode COSY identity".into())
}
#[derive(Clone)]
struct Signer {
    key: String,
    info: String,
}
static SIGNERS: LazyLock<Mutex<BTreeMap<String, (String, Signer)>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
fn signer(a: &Account) -> Result<Signer, Failure> {
    let fingerprint = STANDARD.encode(Sha256::digest(format!(
        "{}:{}:{}",
        a.credentials.access_token, a.credentials.refresh_token, a.revision
    )));
    let mut cache = SIGNERS.lock().map_err(|_| "COSY signer unavailable")?;
    if let Some((_, signer)) = cache.get(&a.id).filter(|(f, _)| f == &fingerprint) {
        return Ok(signer.clone());
    }
    let temp = uuid::Uuid::new_v4().simple().to_string();
    let temp = &temp.as_bytes()[..16];
    let public =
        RsaPublicKey::from_public_key_pem(SERVER_KEY).map_err(|_| "Invalid COSY service key")?;
    let key = STANDARD.encode(
        public
            .encrypt(&mut rand::rngs::OsRng, Pkcs1v15Encrypt, temp)
            .map_err(|_| "Cannot prepare COSY session")?,
    );
    let plain = sorted(
        json!({"name":a.nickname,"aid":a.uid,"uid":a.uid,"yx_uid":"",
        "organization_id":a.credentials.organization_id,"organization_name":a.credentials.organization_name,
        "user_type":a.credentials.user_type,"security_oauth_token":a.credentials.access_token,"refresh_token":a.credentials.refresh_token}),
    )?;
    let encrypted = cbc::Encryptor::<aes::Aes128>::new_from_slices(temp, temp)
        .map_err(|_| "Invalid COSY session key")?
        .encrypt_padded_vec_mut::<Pkcs7>(plain.as_bytes());
    let signer = Signer {
        key,
        info: STANDARD.encode(encrypted),
    };
    if cache.len() >= 500 {
        cache.clear();
    }
    cache.insert(a.id.clone(), (fingerprint, signer.clone()));
    Ok(signer)
}
/// Per-account bytes derived from the account's random, persisted device id, so the
/// values below stay stable across restarts and reauthorization (which keeps the device
/// id) and differ between accounts. A constant shared by every account would link them.
fn device_seed(a: &Account, label: &str) -> [u8; 32] {
    Sha256::digest(format!("cosy-{label}:{}", a.credentials.device_id)).into()
}
/// Local address reported by the client, in the link-local form the reference capture
/// used (169.254.0.0/16), one per account.
pub fn client_ip(a: &Account) -> String {
    let d = device_seed(a, "client-ip");
    format!("169.254.{}.{}", 1 + d[0] % 254, 1 + d[1] % 254)
}
/// Host name reported by the client: the default form of this server's OS, one per account.
pub fn hostname(a: &Account) -> String {
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let d = device_seed(a, "hostname");
    let pick = |n: usize| -> String {
        d[..n]
            .iter()
            .map(|b| ALNUM[*b as usize % ALNUM.len()] as char)
            .collect()
    };
    match std::env::consts::OS {
        "windows" => format!("DESKTOP-{}", pick(7)),
        "macos" => format!("MacBook-Pro-{}.local", pick(4)),
        _ => format!("dev-{}", pick(6).to_ascii_lowercase()),
    }
}
pub fn headers(
    a: &Account,
    body: &str,
    url: &str,
    model: &str,
    sse: bool,
) -> Result<HeaderMap, Failure> {
    let signer = signer(a)?;
    let payload = STANDARD.encode(sorted(
        json!({"cosyVersion":version(),"ideVersion":"","info":signer.info,
        "requestId":uuid::Uuid::new_v4().to_string(),"version":"v1"}),
    )?);
    let date = super::super::now().to_string();
    let url = url::Url::parse(url).map_err(|_| "Invalid COSY URL")?;
    let path = url.path().strip_prefix("/algo").unwrap_or(url.path());
    let signature = format!(
        "{:x}",
        md5::Md5::digest(format!("{payload}\n{}\n{date}\n{body}\n{path}", signer.key))
    );
    let token = URL_SAFE_NO_PAD.encode(Sha512::digest(format!("machinetoken:{}", a.uid)));
    let machine_type = a.credentials.device_id.replace('-', "");
    let arch = if std::env::consts::ARCH == "aarch64" {
        "aarch64"
    } else {
        "x86_64"
    };
    let os = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        _ => "linux",
    };
    let values = [
        (
            "authorization",
            format!("Bearer COSY.{payload}.{signature}"),
        ),
        ("cosy-key", signer.key),
        ("cosy-date", date),
        ("cosy-user", a.uid.clone()),
        ("cosy-data-policy", "agree".into()),
        ("cosy-business-product", "cli".into()),
        ("cosy-business-type", "agent".into()),
        ("cosy-scene", "assistant".into()),
        ("cosy-clienttype", "5".into()),
        ("cosy-version", version()),
        ("cosy-machineid", a.credentials.machine_id.clone()),
        ("cosy-machinetoken", token[..43].into()),
        ("cosy-machinetype", machine_type.chars().take(18).collect()),
        ("cosy-machineos", format!("{arch}_{os}")),
        ("cosy-clientip", client_ip(a)),
        ("login-version", "v2".into()),
        ("user-agent", "undici".into()),
        ("content-type", "application/json".into()),
        (
            "accept",
            if sse {
                "text/event-stream"
            } else {
                "application/json"
            }
            .into(),
        ),
        ("accept-encoding", "identity".into()),
    ];
    let mut headers = HeaderMap::new();
    for (key, value) in values {
        http::insert(&mut headers, key, &value)?;
    }
    if sse {
        http::insert(&mut headers, "cache-control", "no-cache")?;
        http::insert(&mut headers, "cosy-machinehostname", &hostname(a))?;
    }
    if !model.is_empty() {
        http::insert(&mut headers, "x-model-key", model)?;
        http::insert(&mut headers, "x-model-source", "system")?;
    }
    Ok(headers)
}
