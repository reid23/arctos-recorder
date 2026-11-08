//! Client for the parts of the Arctos API the recorder uses.
//!
//! The server is <https://events.californiajugger.org> unless `ARCTOS_URL` is
//! set. For servers behind Cloudflare Access, set
//! `ARCTOS_CF_ACCESS_CLIENT_ID` and `ARCTOS_CF_ACCESS_CLIENT_SECRET` to a
//! service token.

mod auth;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use protocol::{Span, Timestamp};
use reqwest::header::{CONTENT_TYPE, DATE, HeaderMap, HeaderValue};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncReadExt;

pub use auth::{REDIRECT_PATH, TokenSource, Tokens, User};

const DEFAULT_URL: &str = "https://events.californiajugger.org";
/// Arctos rejects upload chunks larger than 16 MiB.
const CHUNK_BYTES: usize = 8 << 20;

#[derive(Clone)]
pub struct Arctos {
    url: String,
    http: Client,
    headers: HeaderMap,
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
            .default_headers(headers.clone())
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Arctos {
            url: url.trim_end_matches('/').to_owned(),
            http,
            headers,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Headers every request to Arctos needs, for clients other than this
    /// one (e.g. a browser rendering the scoreboard).
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Lists the tournament's fields. Unpublished tournaments are only
    /// visible to their organizers.
    pub async fn fields(&self, tournament: &str, token: &dyn TokenSource) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Fields {
            fields: Vec<Field>,
        }
        #[derive(Deserialize)]
        struct Field {
            name: String,
        }
        let fields: Fields = self
            .get(
                &format!("/tournaments/{}/fields", encode(tournament)),
                token,
            )
            .await?;
        Ok(fields.fields.into_iter().map(|f| f.name).collect())
    }

    /// The match in progress on a field, if any, and how far Arctos's clock
    /// is ahead of this computer.
    pub async fn scoreboard(
        &self,
        tournament: &str,
        field: &str,
        token: &dyn TokenSource,
    ) -> Result<Timed<Option<Scoreboard>>> {
        #[derive(Deserialize)]
        struct Response {
            r#match: Option<Scoreboard>,
        }
        let path = format!(
            "/tournaments/{}/fields/{}/scoreboard",
            encode(tournament),
            encode(field)
        );
        let timed: Timed<Response> = self.get_timed(&path, token).await?;
        Ok(Timed {
            value: timed.value.r#match,
            offset: timed.offset,
        })
    }

    /// One match's stored status and points. `status` is the lifecycle the
    /// API reports, which for a static break follows the clock rather than a
    /// referee ending the match.
    pub async fn match_progress(
        &self,
        tournament: &str,
        match_id: &str,
        token: &dyn TokenSource,
    ) -> Result<Timed<MatchProgress>> {
        #[derive(Deserialize)]
        struct Response {
            r#match: MatchProgress,
        }
        let path = format!("/tournaments/{}/matches/{}", encode(tournament), encode(match_id));
        let timed: Timed<Response> = self.get_timed(&path, token).await?;
        Ok(Timed {
            value: timed.value.r#match,
            offset: timed.offset,
        })
    }

    /// Rendered scoreboard overlay images for every state of a match, each
    /// `size` pixels with transparency, in the order they took effect.
    pub async fn scoreboard_overlay(
        &self,
        tournament: &str,
        match_id: &str,
        (width, height): (u32, u32),
    ) -> Result<Vec<OverlayFrame>> {
        #[derive(Deserialize)]
        struct Response {
            frames: Vec<Frame>,
        }
        #[derive(Deserialize)]
        struct Frame {
            effective_at: DateTime<Utc>,
            png: String,
        }
        let url = format!(
            "{}/_api/tournaments/{}/matches/{}/scoreboard-overlay",
            self.url,
            encode(tournament),
            encode(match_id)
        );
        let response = self
            .http
            .get(url)
            .query(&[("width", width), ("height", height)])
            .send()
            .await?;
        let response: Response = json_response(response).await?;
        response
            .frames
            .into_iter()
            .map(|frame| {
                Ok(OverlayFrame {
                    effective_at: frame.effective_at,
                    png: STANDARD.decode(frame.png)?.into(),
                })
            })
            .collect()
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

    /// Uploads footage of a match, reporting progress in `[0, 1]`. The user
    /// must organize the tournament.
    pub async fn upload_footage(
        &self,
        token: &dyn TokenSource,
        footage: &Footage<'_>,
        mut progress: impl FnMut(f32),
    ) -> Result<()> {
        #[derive(Deserialize)]
        struct Upload {
            upload_id: String,
        }
        let Footage {
            tournament,
            match_id,
            camera,
            video,
            anchors,
        } = footage;
        let uploads = format!(
            "{}/api/v1/tournaments/{}/matches/{}/footage/uploads",
            self.url,
            encode(tournament),
            encode(match_id)
        );
        let mut file = tokio::fs::File::open(video).await?;
        let size = file.metadata().await?.len() as usize;
        let total_chunks = size.div_ceil(CHUNK_BYTES);

        let request = self.http.post(&uploads).json(&json!({
            "camera_name": camera,
            "filename": "panorama.mp4",
            "content_type": "video/mp4",
            "total_chunks": total_chunks,
            "anchors": anchors,
        }));
        let upload: Upload = self.send(request, token).await?;
        let upload = format!("{uploads}/{}", encode(&upload.upload_id));

        for index in 0..total_chunks {
            let mut chunk = Vec::with_capacity(CHUNK_BYTES);
            (&mut file)
                .take(CHUNK_BYTES as u64)
                .read_to_end(&mut chunk)
                .await?;
            let request = self
                .http
                .put(format!("{upload}/chunks/{index}"))
                .header(CONTENT_TYPE, "application/octet-stream")
                .body(chunk);
            self.send::<serde_json::Value>(request, token).await?;
            progress((index + 1) as f32 / total_chunks as f32);
        }

        let request = self.http.post(format!("{upload}/complete"));
        self.send::<serde_json::Value>(request, token).await?;
        Ok(())
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        token: &dyn TokenSource,
    ) -> Result<T> {
        Ok(self.get_timed(path, token).await?.value)
    }

    async fn get_timed<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        token: &dyn TokenSource,
    ) -> Result<Timed<T>> {
        let token = token.access_token().await?;
        let started = Utc::now();
        let response = self
            .http
            .get(format!("{}/api/v1{path}", self.url))
            .bearer_auth(token)
            .timeout(Duration::from_secs(5))
            .send()
            .await?;
        let received = Utc::now();
        let offset = clock_offset(response.headers(), started, received);
        Ok(Timed {
            value: json_response(response).await?,
            offset,
        })
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        token: &dyn TokenSource,
    ) -> Result<T> {
        let token = token.access_token().await?;
        json_response(request.bearer_auth(token).send().await?).await
    }
}

fn encode(segment: &str) -> percent_encoding::PercentEncode<'_> {
    utf8_percent_encode(segment, NON_ALPHANUMERIC)
}

/// Path (and query) of a field's scoreboard overlay page.
pub fn scoreboard_path(tournament: &str, field: &str) -> String {
    // The page decodes `%20` but not `+` in the query.
    format!("/{}/scoreboard?field={}", encode(tournament), encode(field))
}

pub struct Page {
    pub status: StatusCode,
    pub content_type: Option<HeaderValue>,
    pub body: Bytes,
}

/// A value read from Arctos, with the clock offset measured on that read.
pub struct Timed<T> {
    pub value: T,
    /// Arctos's clock minus this computer's, in microseconds: the response's
    /// `Date` minus the midpoint of the request. `None` when `Date` is absent,
    /// in which case the last offset that was measured still applies.
    pub offset: Option<i64>,
}

/// Status and points of one match, as [`Arctos::match_progress`] reads them.
#[derive(Clone, Debug, Deserialize)]
pub struct MatchProgress {
    pub status: String,
    pub schedule_type: String,
    pub points: Vec<ScoreboardPoint>,
}

/// The match in progress on a field.
#[derive(Clone, Debug, Deserialize)]
pub struct Scoreboard {
    pub id: String,
    pub team1: Team,
    pub team2: Team,
    pub points: Vec<ScoreboardPoint>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Team {
    pub name: Option<String>,
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
        let name = |team: &Team| team.name.clone().unwrap_or_else(|| "?".into());
        format!("{} vs {}", name(&self.team1), name(&self.team2))
    }
}

/// One state of a match's scoreboard overlay.
#[derive(Clone, Debug)]
pub struct OverlayFrame {
    pub effective_at: DateTime<Utc>,
    pub png: Bytes,
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

/// Arctos's clock minus the local clock, from the `Date` header and the
/// midpoint of the request.
fn clock_offset(headers: &HeaderMap, started: DateTime<Utc>, received: DateTime<Utc>) -> Option<i64> {
    let value = headers.get(DATE)?.to_str().ok()?;
    let arctos = DateTime::parse_from_rfc2822(value).ok()?.to_utc();
    let midpoint = started.timestamp_micros() + (received.timestamp_micros() - started.timestamp_micros()) / 2;
    Some(arctos.timestamp_micros() - midpoint)
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
    fn clock_offset_is_arctos_minus_the_local_midpoint() {
        let started = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let received = started + chrono::Duration::seconds(2);
        let arctos = started + chrono::Duration::seconds(6);
        let mut headers = HeaderMap::new();
        headers.insert(DATE, HeaderValue::from_str(&arctos.to_rfc2822()).unwrap());
        assert_eq!(clock_offset(&headers, started, received), Some(5_000_000));
        assert_eq!(clock_offset(&HeaderMap::new(), started, received), None);
    }

    #[test]
    fn scoreboard_path_encodes_spaces_as_percent_20() {
        assert_eq!(
            scoreboard_path("fow26", "Field 1"),
            "/fow26/scoreboard?field=Field%201"
        );
    }
}
