use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use reqwest::blocking::Client;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::tags::TrackTags;

#[derive(Debug)]
pub enum LyricsResult {
    Found(String),
    NotFound,
    TemporaryFailure,
}

#[derive(Clone)]
pub struct LrclibClient {
    client: Client,
    limiter: Arc<RateLimiter>,
}

impl LrclibClient {
    pub fn new(request_interval: Duration, request_timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .user_agent("lrcget-cli/0.1 (contact: codeberg.org/lutyjj/lrcget-cli)")
                .timeout(request_timeout)
                .build()?,
            limiter: Arc::new(RateLimiter::new(request_interval)),
        })
    }

    pub fn fetch(&self, tags: &TrackTags) -> LyricsResult {
        self.fetch_once(tags, true)
    }

    fn fetch_once(&self, tags: &TrackTags, allow_retry_after: bool) -> LyricsResult {
        self.limiter.wait();
        let mut params = vec![
            ("artist_name", tags.artist.as_str()),
            ("track_name", tags.title.as_str()),
        ];
        if !tags.album.is_empty() {
            params.push(("album_name", tags.album.as_str()));
        }
        let duration;
        if tags.duration_secs > 0 {
            duration = tags.duration_secs.to_string();
            params.push(("duration", duration.as_str()));
        }

        let response = match self
            .client
            .get("https://lrclib.net/api/get")
            .query(&params)
            .send()
        {
            Ok(response) => response,
            Err(err) => {
                warn!(artist = %tags.artist, title = %tags.title, error = %err, "LRCLib request failed");
                return LyricsResult::TemporaryFailure;
            }
        };

        match response.status().as_u16() {
            200 => match response.json::<LrclibResponse>() {
                Ok(body) => body
                    .synced_lyrics
                    .or(body.plain_lyrics)
                    .filter(|lyrics| !lyrics.trim().is_empty())
                    .map(LyricsResult::Found)
                    .unwrap_or(LyricsResult::NotFound),
                Err(err) => {
                    warn!(artist = %tags.artist, title = %tags.title, error = %err, "failed parsing LRCLib response");
                    LyricsResult::TemporaryFailure
                }
            },
            404 => LyricsResult::NotFound,
            429 if allow_retry_after => {
                let delay = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| Duration::from_secs(30))
                    .min(Duration::from_secs(120));
                warn!(?delay, artist = %tags.artist, title = %tags.title, "LRCLib rate limited request; retrying once");
                thread::sleep(delay);
                self.fetch_once(tags, false)
            }
            status => {
                debug!(status, artist = %tags.artist, title = %tags.title, "LRCLib returned non-success status");
                LyricsResult::TemporaryFailure
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct LrclibResponse {
    #[serde(rename = "syncedLyrics")]
    synced_lyrics: Option<String>,
    #[serde(rename = "plainLyrics")]
    plain_lyrics: Option<String>,
}

#[derive(Debug)]
struct RateLimiter {
    min_interval: Duration,
    next_allowed: Mutex<Instant>,
}

impl RateLimiter {
    fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            next_allowed: Mutex::new(Instant::now()),
        }
    }

    fn wait(&self) {
        if self.min_interval.is_zero() {
            return;
        }

        let mut next_allowed = self.next_allowed.lock().expect("rate limiter poisoned");
        let now = Instant::now();
        if *next_allowed > now {
            thread::sleep(*next_allowed - now);
        }
        *next_allowed = Instant::now() + self.min_interval;
    }
}
