//! Minimal client for the parts of the itch.io API we need.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::io::AsyncWriteExt;

pub const DEFAULT_API_URL: &str = "https://api.itch.io";

pub struct ItchClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

/// A game (asset pack) the account owns, with the download key that grants access.
#[derive(Debug, Clone)]
pub struct OwnedGame {
    pub key_id: u64,
    pub game_id: u64,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Upload {
    pub id: u64,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub md5_hash: Option<String>,
    /// Set for uploads pushed with butler (wharf); changes on every push.
    #[serde(default)]
    pub build_id: Option<u64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Deserialize)]
struct OwnedKeysPage {
    #[serde(default, deserialize_with = "lenient_vec")]
    owned_keys: Vec<OwnedKey>,
}

#[derive(Deserialize)]
struct OwnedKey {
    id: u64,
    game_id: u64,
    #[serde(default)]
    game: Option<Game>,
}

#[derive(Deserialize)]
struct Game {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
}

#[derive(Deserialize)]
struct UploadsPage {
    #[serde(default, deserialize_with = "lenient_vec")]
    uploads: Vec<Upload>,
}

/// itch serialises an empty list as `{}` (a Lua empty table), so treat
/// anything that is not an array as empty.
fn lenient_vec<'de, D: serde::Deserializer<'de>, T: DeserializeOwned>(
    d: D,
) -> Result<Vec<T>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        v @ serde_json::Value::Array(_) => {
            serde_json::from_value(v).map_err(serde::de::Error::custom)
        }
        _ => Ok(Vec::new()),
    }
}

impl ItchClient {
    pub fn new(base: &str, token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("itch-nfs/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token,
        })
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, &self.token)
            .query(query)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .with_context(|| format!("GET {path}: HTTP {status}, body is not JSON"))?;
        // itch reports errors as `{"errors": [...]}`, sometimes with a 200 status.
        if let Some(errors) = body.get("errors") {
            bail!("GET {path}: itch.io API error: {errors}");
        }
        if !status.is_success() {
            bail!("GET {path}: HTTP {status}");
        }
        serde_json::from_value(body)
            .with_context(|| format!("GET {path}: unexpected response shape"))
    }

    /// Every game the account holds a download key for.
    pub async fn owned_games(&self) -> Result<Vec<OwnedGame>> {
        let mut out = Vec::new();
        for page in 1u32.. {
            let p: OwnedKeysPage = self
                .get_json("/profile/owned-keys", &[("page", page.to_string())])
                .await?;
            if p.owned_keys.is_empty() {
                break;
            }
            out.extend(p.owned_keys.into_iter().map(|k| {
                let g = k.game.unwrap_or(Game {
                    title: String::new(),
                    url: String::new(),
                });
                OwnedGame {
                    key_id: k.id,
                    game_id: k.game_id,
                    title: g.title,
                    url: g.url,
                }
            }));
        }
        Ok(out)
    }

    pub async fn uploads(&self, game_id: u64, key_id: u64) -> Result<Vec<Upload>> {
        let p: UploadsPage = self
            .get_json(
                &format!("/games/{game_id}/uploads"),
                &[("download_key_id", key_id.to_string())],
            )
            .await?;
        Ok(p.uploads)
    }

    /// Stream an upload to `dest`, returning the number of bytes written.
    pub async fn download(&self, upload_id: u64, key_id: u64, dest: &Path) -> Result<u64> {
        let url = format!("{}/uploads/{upload_id}/download", self.base);
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, &self.token)
            .query(&[("download_key_id", key_id.to_string())])
            .send()
            .await
            .with_context(|| format!("download of upload {upload_id}"))?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "download of upload {upload_id}: HTTP {}",
                resp.status()
            ));
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .with_context(|| format!("create {}", dest.display()))?;
        let mut stream = resp.bytes_stream();
        let mut written = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("download of upload {upload_id}"))?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.flush().await?;
        Ok(written)
    }
}
