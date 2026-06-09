use anyhow::{Context, Result};
use serde::Deserialize;

const VERSION_MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

#[derive(Debug, Clone)]
pub struct MinecraftVersion {
    pub id: String,
    pub kind: String,
    pub server_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VersionManifest {
    versions: Vec<ManifestVersion>,
}

#[derive(Debug, Deserialize)]
struct ManifestVersion {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct VersionPackage {
    downloads: Downloads,
}

#[derive(Debug, Deserialize)]
struct Downloads {
    server: Option<Download>,
}

#[derive(Debug, Deserialize)]
struct Download {
    url: String,
}

pub async fn fetch_versions(limit: usize) -> Result<Vec<MinecraftVersion>> {
    let manifest = reqwest::get(VERSION_MANIFEST)
        .await
        .context("fetch Mojang version manifest")?
        .error_for_status()?
        .json::<VersionManifest>()
        .await
        .context("decode Mojang version manifest")?;

    let mut versions = Vec::new();
    for item in manifest.versions.into_iter().take(limit) {
        let server_url = fetch_server_url(&item.url).await.unwrap_or(None);
        versions.push(MinecraftVersion {
            id: item.id,
            kind: item.kind,
            server_url,
        });
    }

    Ok(versions)
}

async fn fetch_server_url(url: &str) -> Result<Option<String>> {
    let package = reqwest::get(url)
        .await?
        .error_for_status()?
        .json::<VersionPackage>()
        .await?;
    Ok(package.downloads.server.map(|download| download.url))
}
