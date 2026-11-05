//! Official platform UMID bridge. Resources are pinned, verified, and never downloaded
//! at runtime. The bridge is invoked lazily; no idle account heartbeat is introduced.
use crate::Error;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::Mutex,
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineInfo {
    pub machine_token: String,
    pub machine_type: String,
    pub machine_code: String,
}
impl MachineInfo {
    pub fn validate(&self) -> Result<(), Error> {
        if self.machine_token.trim().is_empty()
            || [&self.machine_token, &self.machine_type, &self.machine_code]
                .iter()
                .any(|v| v.len() > 16384 || !v.bytes().all(|b| (32..=126).contains(&b)))
        {
            return Err("Qoder machine identity is unavailable or invalid".into());
        }
        Ok(())
    }
}
fn bundle() -> Result<(&'static str, &'static [u8]), Error> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return Ok((
        "linux-x64",
        include_bytes!("../vendor/runtime-info-linux-x64.gz"),
    ));
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return Ok((
        "linux-arm64",
        include_bytes!("../vendor/runtime-info-linux-arm64.gz"),
    ));
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return Ok((
        "darwin-x64",
        include_bytes!("../vendor/runtime-info-darwin-x64.gz"),
    ));
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Ok((
        "darwin-arm64",
        include_bytes!("../vendor/runtime-info-darwin-arm64.gz"),
    ));
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return Ok((
        "win32-x64",
        include_bytes!("../vendor/runtime-info-win32-x64.gz"),
    ));
    #[allow(unreachable_code)]
    Err("Official Qoder UMID does not support this platform; use the explicit legacy protocol setting".into())
}
fn publish(dir: &Path, resource: &str, file: &str, compressed: &[u8]) -> Result<PathBuf, Error> {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../vendor/manifest.json"))?;
    let metadata = &manifest["files"][resource];
    if format!("{:x}", Sha256::digest(compressed))
        != metadata["sha256"]
            .as_str()
            .ok_or("Missing Qoder resource checksum")?
    {
        return Err("Qoder resource checksum mismatch".into());
    }
    let expected = metadata["uncompressed_sha256"]
        .as_str()
        .ok_or("Missing Qoder binary checksum")?;
    let file = dir.join(file);
    if let Ok(existing) = std::fs::read(&file) {
        if format!("{:x}", Sha256::digest(&existing)) == expected {
            return Ok(file);
        }
    }
    let mut bytes = Vec::new();
    GzDecoder::new(compressed)
        .take(16 * 1024 * 1024)
        .read_to_end(&mut bytes)?;
    if format!("{:x}", Sha256::digest(&bytes)) != expected {
        return Err("Qoder binary checksum mismatch".into());
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        file.file_name()
            .ok_or("Invalid Qoder binary path")?
            .to_string_lossy(),
        rand::random::<u64>()
    ));
    let result = (|| -> Result<(), Error> {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o700);
        }
        let mut f = options.open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        // The versioned directory prevents upgrades replacing an executing binary.
        if file.exists() {
            std::fs::remove_file(&file)?;
        }
        std::fs::rename(&tmp, &file)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(file)
}
fn prepare(root: &Path) -> Result<PathBuf, Error> {
    let (platform, bytes) = bundle()?;
    let directory = root
        .join("qoder-runtime")
        .join(format!("{}-{platform}", crate::CLI_VERSION));
    #[cfg(windows)]
    publish(
        &directory,
        "runtime-info-sgsdk-win32-x64.gz",
        "sgsdk.dll",
        include_bytes!("../vendor/runtime-info-sgsdk-win32-x64.gz"),
    )?;
    publish(
        &directory,
        &format!("runtime-info-{platform}.gz"),
        if cfg!(windows) {
            "runtime-info.exe"
        } else {
            "runtime-info"
        },
        bytes,
    )
}
async fn invoke(binary: &Path, cn: bool, uid: &str) -> Result<MachineInfo, Error> {
    let mut command = Command::new(binary);
    command.current_dir(binary.parent().ok_or("Missing Qoder runtime directory")?);
    command
        .arg(if cn { "0" } else { "4" })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let account_stdin = cfg!(any(target_os = "windows", target_os = "macos")) && !uid.is_empty();
    if account_stdin {
        command.arg("--account-stdin").stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command
        .spawn()
        .map_err(|_| "Cannot start the official Qoder identity helper")?;
    let result = tokio::time::timeout(Duration::from_secs(25), async {
        if account_stdin {
            let mut stdin = child.stdin.take().ok_or("Missing Qoder helper stdin")?;
            stdin
                .write_all(format!("{}\n", serde_json::json!({"account":uid})).as_bytes())
                .await?;
            stdin.shutdown().await?;
        }
        let stdout = child.stdout.take().ok_or("Missing Qoder helper stdout")?;
        let mut reader = BufReader::new(stdout).take(1024 * 1024);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await?;
        let info: MachineInfo = serde_json::from_slice(&line)
            .map_err(|_| "Qoder identity helper returned invalid output")?;
        info.validate()?;
        Ok::<_, Error>(info)
    })
    .await
    .map_err(|_| "Qoder identity helper timed out")
    .and_then(|r| r.map_err(|_| "Qoder machine identity could not be obtained"));
    // A bridge can remain alive after returning JSON. Bound that tail and reap it.
    if tokio::time::timeout(
        Duration::from_secs(if account_stdin && result.is_ok() {
            10
        } else {
            1
        }),
        child.wait(),
    )
    .await
    .is_err()
    {
        let _ = child.kill().await;
    }
    result.map_err(Into::into)
}
/// Cache by account and realm, keeping the official SDK's account association separate.
pub async fn machine(root: &Path, cn: bool, uid: &str) -> Result<MachineInfo, Error> {
    if uid.len() > 512 {
        return Err("Qoder account identity exceeds size limit".into());
    }
    type Cache = BTreeMap<(PathBuf, bool, String), (Instant, MachineInfo)>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let mut cache = tokio::time::timeout(
        Duration::from_secs(40),
        CACHE.get_or_init(Default::default).lock(),
    )
    .await
    .map_err(|_| "Qoder identity helper is busy")?;
    let key = (root.to_owned(), cn, uid.to_owned());
    if let Some((at, info)) = cache
        .get(&key)
        .filter(|(at, _)| at.elapsed() < Duration::from_secs(3600))
    {
        let _ = at;
        return Ok(info.clone());
    }
    let directory = root.to_owned();
    let binary = tokio::task::spawn_blocking(move || prepare(&directory)).await??;
    let info = match invoke(&binary, cn, uid).await {
        Ok(info) => info,
        Err(error) => {
            // Keep the last verified identity on transient SDK failure, as the CLI does.
            if let Some((_, info)) = cache.get(&key) {
                return Ok(info.clone());
            }
            return Err(error);
        }
    };
    if cache.len() >= 500 {
        cache.clear();
    }
    cache.insert(key, (Instant::now(), info.clone()));
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packaged_helper_is_verified_and_repaired_before_execution() {
        let root = tempfile::tempdir().unwrap();
        let path = prepare(root.path()).unwrap();
        let expected = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"corrupt").unwrap();
        assert_eq!(prepare(root.path()).unwrap(), path);
        assert_eq!(std::fs::read(path).unwrap(), expected);
    }
    #[test]
    fn machine_identity_rejects_empty_tokens_and_header_injection() {
        let mut info = MachineInfo {
            machine_token: "token".into(),
            machine_code: "code".into(),
            machine_type: "type".into(),
        };
        assert!(info.validate().is_ok());
        info.machine_code = "code\r\nx: y".into();
        assert!(info.validate().is_err());
        info.machine_code.clear();
        info.machine_token.clear();
        assert!(info.validate().is_err());
    }
}
