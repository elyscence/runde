use serde::Deserialize;

const OWNER: &str = "elyscence";
const REPO: &str = "runde";

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateInfo {
    pub version: String,
    pub download_url: String,
    pub asset_name: String,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

fn asset_name_for_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "runde-windows-x86_64.exe"
    } else if cfg!(target_os = "linux") {
        "runde-linux-x86_64"
    } else {
        "runde-unknown"
    }
}

fn is_newer(current: &str, remote: &str) -> bool {
    fn parse(v: &str) -> Vec<u64> {
        v.trim_start_matches('v')
            .split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    }
    let c = parse(current);
    let r = parse(remote);
    for i in 0..r.len().max(c.len()) {
        let rv = r.get(i).copied().unwrap_or(0);
        let cv = c.get(i).copied().unwrap_or(0);
        if rv != cv {
            return rv > cv;
        }
    }
    false
}


pub async fn check_for_update() -> anyhow::Result<Option<UpdateInfo>> {
    let url = format!("https://api.github.com/repos/{OWNER}/{REPO}/releases/latest");

    let client = reqwest::Client::builder()
        .user_agent(format!("runde-updater/{}", env!("CARGO_PKG_VERSION")))
        .build()?;

    let resp = client.get(&url).send().await?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let resp = resp.error_for_status()?;
    let release: GhRelease = resp.json().await?;

    let remote_version = release.tag_name.trim_start_matches('v').to_string();
    let current_version = env!("CARGO_PKG_VERSION");

    if !is_newer(current_version, &remote_version) {
        return Ok(None);
    }

    let target_asset = asset_name_for_platform();
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == target_asset)
        .ok_or_else(|| anyhow::anyhow!("Ассет '{target_asset}' не найден в релизе {}", release.tag_name))?;

    Ok(Some(UpdateInfo {
        version: remote_version,
        download_url: asset.browser_download_url.clone(),
        asset_name: asset.name.clone(),
    }))
}

pub async fn download_and_install(info: &UpdateInfo) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(format!("runde-updater/{}", env!("CARGO_PKG_VERSION")))
        .build()?;

    let bytes = client
        .get(&info.download_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(&info.asset_name);
    tokio::fs::write(&tmp_path, &bytes).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = tokio::fs::metadata(&tmp_path).await?.permissions();
        perms.set_mode(0o755);
        tokio::fs::set_permissions(&tmp_path, perms).await?;
    }

    self_replace::self_replace(&tmp_path)?;

    let _ = tokio::fs::remove_file(&tmp_path).await;

    Ok(())
}
