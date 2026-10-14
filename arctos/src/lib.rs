//! Client for the parts of the Arctos API the recorder uses.
//!
//! The server is <https://events.californiajugger.org> unless `ARCTOS_URL` is
//! set. For servers behind Cloudflare Access, set
//! `ARCTOS_CF_ACCESS_CLIENT_ID` and `ARCTOS_CF_ACCESS_CLIENT_SECRET` to a
//! service token.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use protocol::{Span, Timestamp};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::multipart::{Form, Part};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncReadExt;

const DEFAULT_URL: &str = "https://events.californiajugger.org";
/// Arctos rejects upload chunks larger than 16 MiB.
const CHUNK_BYTES: usize = 8 << 20;

/// A tournament organizer account.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

#[derive(Clone)]
pub struct Arctos {
    url: String,
    http: Client,
}

impl Arctos {
    pub fn from_env() -> Result<Self> {
        let var = |name| std::env::var(name).ok().filter(|value| !value.is_empty());
        let url = var("ARCTOS_URL").unwrap_or_else(|| DEFAULT_URL.into());
        let mut headers = HeaderMap::new();
        for (header, name) in [
            ("CF-Access-Client-Id", "ARCTOS_CF_ACCESS_CLIENT_ID"),
            ("CF-Access-Client-Secret", "ARCTOS_CF_ACCESS_CLIENT_SECRET"),
        ] {
            if let Some(value) = var(name) {
                headers.insert(header, HeaderValue::from_str(&value).context(name)?);
            }
        }
        let http = Client::builder()
            .default_headers(headers)
            .cookie_store(true)
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Arctos {
            url: url.trim_end_matches('/').to_owned(),
            http,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Lists the tournament's fields. Logs in first, since unpublished
    /// tournaments are only visible to their organizers.
    pub async fn fields(&self, tournament: &str, credentials: &Credentials) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Fields {
            fields: Vec<Field>,
        }
        #[derive(Deserialize)]
        struct Field {
            name: String,
        }
        self.login(credentials).await?;
        let url = format!("{}/_api/tournaments/{tournament}/fields", self.url);
        let fields: Fields = json_response(self.http.get(url).send().await?).await?;
        Ok(fields.fields.into_iter().map(|f| f.name).collect())
    }

    pub async fn scoreboard(&self, tournament: &str, field: &str) -> Result<Scoreboard> {
        let request = self
            .http
            .get(format!("{}/_api/scoreboard-state", self.url))
            .query(&[("tournament", tournament), ("field", field)])
            .timeout(Duration::from_secs(3));
        json_response(request.send().await?).await
    }

    /// Fetches any page of the Arctos web app, e.g. to show the scoreboard
    /// overlay without needing the Cloudflare credentials in the browser.
    pub async fn page(&self, path_and_query: &str) -> Result<Page> {
        let response = self
            .http
            .get(format!("{}{path_and_query}", self.url))
            .send()
            .await?;
        Ok(Page {
            status: response.status(),
            content_type: response.headers().get(CONTENT_TYPE).cloned(),
            body: response.bytes().await?,
        })
    }

    async fn login(&self, credentials: &Credentials) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/_api/login", self.url))
            .json(credentials)
            .send()
            .await?;
        json_response::<serde_json::Value>(response)
            .await
            .context("logging in to Arctos")?;
        Ok(())
    }

    /// Uploads footage of a match, reporting progress in `[0, 1]`.
    pub async fn upload_footage(
        &self,
        credentials: &Credentials,
        footage: &Footage<'_>,
        mut progress: impl FnMut(f32),
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct Init {
            upload_id: String,
        }
        self.login(credentials).await?;
        let Footage {
            tournament,
            match_id,
            camera,
            video,
            anchors,
        } = footage;
        let base = format!(
            "{}/_api/tournaments/{tournament}/matches/{match_id}/footage/upload",
            self.url
        );
        let mut file = tokio::fs::File::open(video).await?;
        let size = file.metadata().await?.len() as usize;
        let total_chunks = size.div_ceil(CHUNK_BYTES);

        let init: Init = json_response(
            self.http
                .post(format!("{base}/init"))
                .json(&json!({
                    "camera_name": camera,
                    "filename": "panorama.mp4",
                    "content_type": "video/mp4",
                    "total_chunks": total_chunks,
                    "anchors": anchors,
                }))
                .send()
                .await?,
        )
        .await?;

        for index in 0..total_chunks {
            let mut chunk = Vec::with_capacity(CHUNK_BYTES);
            (&mut file)
                .take(CHUNK_BYTES as u64)
                .read_to_end(&mut chunk)
                .await?;
            let form = Form::new()
                .text("upload_id", init.upload_id.clone())
                .text("chunk_index", index.to_string())
                .part("chunk", Part::bytes(chunk).file_name("chunk"));
            let response = self
                .http
                .post(format!("{base}/chunk"))
                .multipart(form)
                .send()
                .await?;
            json_response::<serde_json::Value>(response).await?;
            progress((index + 1) as f32 / total_chunks as f32);
        }

        let response = self
            .http
            .post(format!("{base}/complete"))
            .json(&json!({ "upload_id": init.upload_id }))
            .send()
            .await?;
        json_response::<serde_json::Value>(response).await?;
        Ok(())
    }
}

/// Path (and query) of a field's scoreboard overlay page.
pub fn scoreboard_path(tournament: &str, field: &str) -> String {
    // The page decodes `%20` but not `+` in the query.
    let encode = |text| utf8_percent_encode(text, NON_ALPHANUMERIC);
    format!("/{}/scoreboard?field={}", encode(tournament), encode(field))
}

pub struct Page {
    pub status: StatusCode,
    pub content_type: Option<HeaderValue>,
    pub body: Bytes,
}

/// The public scoreboard state of one field (`/_api/scoreboard-state`).
#[derive(Clone, Debug, Deserialize)]
pub struct Scoreboard {
    pub has_active_match: bool,
    pub match_id: Option<String>,
    pub team1_name: Option<String>,
    pub team2_name: Option<String>,
    /// Only reported for stones matches.
    pub points_for_stones: Option<Vec<ScoreboardPoint>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ScoreboardPoint {
    pub stamp: DateTime<Utc>,
    pub end_stamp: Option<DateTime<Utc>>,
}

impl ScoreboardPoint {
    pub fn span(&self) -> Span {
        Span {
            start: timestamp(self.stamp),
            end: self.end_stamp.map(timestamp),
        }
    }
}

impl Scoreboard {
    pub fn title(&self) -> String {
        let name = |n: &Option<String>| n.clone().unwrap_or_else(|| "?".into());
        format!("{} vs {}", name(&self.team1_name), name(&self.team2_name))
    }
}

pub fn timestamp(time: DateTime<Utc>) -> Timestamp {
    Timestamp(time.timestamp_micros())
}

pub fn datetime(timestamp: Timestamp) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(timestamp.0).expect("timestamp in range")
}

/// Maps a moment of the match to a position in the uploaded video.
#[derive(Clone, Debug, Serialize)]
pub struct Anchor {
    pub world_time: DateTime<Utc>,
    /// Seconds from the start of the video.
    pub video_offset: f64,
}

/// A stitched video of one camera's view of a match.
pub struct Footage<'a> {
    pub tournament: &'a str,
    pub match_id: &'a str,
    pub camera: &'a str,
    pub video: &'a Path,
    /// Where segments start, so Arctos can line the video up with the points.
    pub anchors: &'a [Anchor],
}

async fn json_response<T: serde::de::DeserializeOwned>(response: Response) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("Arctos returned {status}: {body}");
    }
    Ok(response.json().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoreboard_path_encodes_spaces_as_percent_20() {
        assert_eq!(
            scoreboard_path("fow26", "Field 1"),
            "/fow26/scoreboard?field=Field%201"
        );
    }
}
