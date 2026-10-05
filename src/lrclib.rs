use std::{
    io::Read,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, de::DeserializeOwned};
use tracing::{debug, warn};

use crate::{
    shutdown::Shutdown,
    tags::{TrackTags, normalize},
};

const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum LyricsResult {
    Found(String),
    Instrumental,
    NotFound,
    Ambiguous,
    TemporaryFailure(String),
    Cancelled,
}

#[derive(Clone)]
pub struct LrclibClient {
    client: Client,
    base_url: String,
    gate: Arc<Mutex<RequestGate>>,
    shutdown: Shutdown,
}

struct RequestGate {
    interval: Duration,
    next_allowed: Option<Instant>,
}

impl LrclibClient {
    #[cfg(test)]
    pub fn test_client(base_url: String, shutdown: Shutdown) -> Self {
        let mut client = Self::new(Duration::ZERO, Duration::from_secs(2), shutdown).unwrap();
        client.base_url = base_url;
        client
    }
    pub fn new(interval: Duration, timeout: Duration, shutdown: Shutdown) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .user_agent(concat!(
                    "lrc-sync/",
                    env!("CARGO_PKG_VERSION"),
                    " (",
                    env!("CARGO_PKG_REPOSITORY"),
                    ")"
                ))
                .timeout(timeout)
                .build()?,
            base_url: "https://lrclib.net/api".into(),
            gate: Arc::new(Mutex::new(RequestGate {
                interval,
                next_allowed: Some(Instant::now()),
            })),
            shutdown,
        })
    }

    pub fn fetch(&self, tags: &TrackTags, clean_fallback: bool) -> LyricsResult {
        match self.fetch_inner(tags, clean_fallback) {
            Ok(result) => result,
            Err(_) if self.shutdown.is_cancelled() => LyricsResult::Cancelled,
            Err(err) => LyricsResult::TemporaryFailure(format!("{err:#}")),
        }
    }

    fn fetch_inner(&self, tags: &TrackTags, clean_fallback: bool) -> Result<LyricsResult> {
        if self.shutdown.is_cancelled() {
            return Ok(LyricsResult::Cancelled);
        }
        if tags.duration_secs <= 0 {
            return Ok(LyricsResult::Ambiguous);
        }
        // A lookup without an album can hide conflicting recordings behind one result.
        if !tags.album.is_empty() {
            // Long durations cannot be sent to /get, whose single result can hide matches.
            let result = if tags.duration_secs > 3600 {
                self.search(tags, tags, false, false)?
            } else {
                self.get(tags)?
                    .map(|record| select_records(tags, vec![record], false, false))
                    .unwrap_or(LyricsResult::NotFound)
            };
            if result != LyricsResult::NotFound {
                return Ok(result);
            }
            if !clean_fallback {
                return Ok(LyricsResult::NotFound);
            }
        }
        let cleaned = if clean_fallback {
            tags.cleaned()
        } else {
            tags.clone()
        };
        if !tags.album.is_empty() && tags.duration_secs <= 3600 && cleaned != *tags {
            debug!(artist = %tags.artist, title = %tags.title, "retrying without release packaging labels");
            if let Some(record) = self.get(&cleaned)? {
                let result = select_records(tags, vec![record], true, false);
                if result != LyricsResult::NotFound {
                    return Ok(result);
                }
            }
        }
        // Relaxing album requires a duration and a unique validated recording.
        self.search(tags, &cleaned, clean_fallback, clean_fallback)
    }

    fn search(
        &self,
        tags: &TrackTags,
        query: &TrackTags,
        clean: bool,
        relax_album: bool,
    ) -> Result<LyricsResult> {
        let mut params = vec![
            ("artist_name", query.artist.as_str()),
            ("track_name", query.title.as_str()),
        ];
        if !relax_album && !query.album.is_empty() {
            params.push(("album_name", query.album.as_str()));
        }
        let records: Vec<LrclibResponse> = self.request("search", &params)?.unwrap_or_default();
        if records.len() >= 20 {
            warn!(artist = %tags.artist, title = %tags.title, "LRCLib search reached its result limit; refusing an incomplete match set");
            return Ok(LyricsResult::Ambiguous);
        }
        Ok(select_records(tags, records, clean, relax_album))
    }

    fn get(&self, tags: &TrackTags) -> Result<Option<LrclibResponse>> {
        let mut params = vec![
            ("artist_name", tags.artist.as_str()),
            ("track_name", tags.title.as_str()),
        ];
        if !tags.album.is_empty() {
            params.push(("album_name", tags.album.as_str()));
        }
        let duration = tags.duration_secs.to_string();
        // LRCLib rejects durations over one hour; validate the response locally.
        if (1..=3600).contains(&tags.duration_secs) {
            params.push(("duration", duration.as_str()));
        }
        self.request("get", &params)
    }

    fn request<T: DeserializeOwned>(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
    ) -> Result<Option<T>> {
        // This owner spans body reading as well as shared Retry-After cooldowns.
        let mut gate = self.gate.lock().expect("request gate poisoned");
        for attempt in 0..=1 {
            let deadline = gate
                .next_allowed
                .context("LRCLib requested a cooldown outside the supported clock range")?;
            if !self
                .shutdown
                .wait(deadline.saturating_duration_since(Instant::now()))
            {
                bail!("lookup cancelled");
            }
            let result = self
                .client
                .get(format!("{}/{endpoint}", self.base_url))
                .query(params)
                .send();
            gate.next_allowed = Instant::now().checked_add(gate.interval);
            let mut response = result.context("sending LRCLib request")?;
            let status = response.status();
            if status.as_u16() == 429 {
                let delay = retry_after(
                    response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok()),
                    SystemTime::now(),
                );
                gate.next_allowed = Instant::now().checked_add(delay.max(gate.interval));
                warn!(
                    ?delay,
                    "LRCLib rate limited requests; suspending all workers"
                );
                if attempt == 0 {
                    continue;
                }
                bail!("LRCLib returned HTTP 429 after one retry; shared cooldown remains active");
            }
            if status.as_u16() == 404 {
                return Ok(None);
            }
            if !status.is_success() {
                bail!("LRCLib returned HTTP {status}");
            }
            let mut bytes = Vec::new();
            let read = response
                .by_ref()
                .take(MAX_RESPONSE_BYTES + 1)
                .read_to_end(&mut bytes);
            drop(response);
            gate.next_allowed = Instant::now().checked_add(gate.interval);
            read.context("reading LRCLib response")?;
            if bytes.len() as u64 > MAX_RESPONSE_BYTES {
                bail!("LRCLib response exceeds {MAX_RESPONSE_BYTES} bytes");
            }
            return serde_json::from_slice(&bytes)
                .map(Some)
                .context("decoding LRCLib response");
        }
        unreachable!()
    }
}

fn retry_after(value: Option<&str>, now: SystemTime) -> Duration {
    value
        .and_then(|value| {
            value
                .trim()
                .parse::<u64>()
                .ok()
                .map(Duration::from_secs)
                .or_else(|| {
                    httpdate::parse_http_date(value)
                        .ok()
                        .map(|deadline| deadline.duration_since(now).unwrap_or(Duration::ZERO))
                })
        })
        .unwrap_or(Duration::from_secs(30))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrclibResponse {
    id: u64,
    artist_name: String,
    track_name: String,
    album_name: String,
    duration: f64,
    #[serde(default)]
    instrumental: bool,
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
}

fn select_records(
    tags: &TrackTags,
    records: Vec<LrclibResponse>,
    clean: bool,
    relax_album: bool,
) -> LyricsResult {
    let wanted = if clean { tags.cleaned() } else { tags.clone() }.normalized();
    let mut matches = Vec::new();
    for record in records {
        let candidate = TrackTags {
            artist: record.artist_name.clone(),
            title: record.track_name.clone(),
            album: record.album_name.clone(),
            duration_secs: 0,
        };
        let candidate = if clean {
            candidate.cleaned()
        } else {
            candidate
        }
        .normalized();
        let album_matches = wanted.album.is_empty() || wanted.album == candidate.album;
        if record.id == 0
            || !record.duration.is_finite()
            || record.duration <= 0.0
            || candidate.artist != wanted.artist
            || candidate.title != wanted.title
            || (tags.duration_secs > 0 && (record.duration - tags.duration_secs as f64).abs() > 2.0)
            || (wanted.album.is_empty()
                && has_recording_context(&record.album_name)
                && !has_recording_context(&tags.title))
            || (!album_matches
                && (!relax_album
                    || has_recording_context(&tags.album)
                    || has_recording_context(&record.album_name)))
        {
            debug!(
                id = record.id,
                "rejecting LRCLib recording that does not match the track signature"
            );
            continue;
        }
        let result = if record.instrumental {
            LyricsResult::Instrumental
        } else {
            let Some(lyrics) = record
                .synced_lyrics
                .filter(|value| !value.trim().is_empty())
                .or_else(|| record.plain_lyrics.filter(|value| !value.trim().is_empty()))
            else {
                continue;
            };
            LyricsResult::Found(lyrics)
        };
        matches.push((album_matches, result));
    }
    if matches.iter().any(|(exact, _)| *exact) {
        matches.retain(|(exact, _)| *exact);
    }
    let mut selected = None;
    for (_, result) in matches {
        if selected
            .as_ref()
            .is_some_and(|previous| *previous != result)
        {
            return LyricsResult::Ambiguous;
        }
        selected = Some(result);
    }
    selected.unwrap_or(LyricsResult::NotFound)
}

fn has_recording_context(value: &str) -> bool {
    let value = normalize(value);
    value.split(|c: char| !c.is_alphanumeric()).any(|word| {
        matches!(
            word,
            "live"
                | "remix"
                | "mix"
                | "edit"
                | "version"
                | "demo"
                | "acoustic"
                | "unplugged"
                | "concert"
                | "instrumental"
                | "karaoke"
                | "session"
                | "sessions"
                | "rerecorded"
                | "mono"
                | "stereo"
        )
    }) || value.contains("re-recorded")
        || value.contains("radio edit")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Response, TestServer, lyrics, tags};
    use std::{sync::Barrier, thread};

    fn client(server: &TestServer, interval: Duration) -> LrclibClient {
        let mut client =
            LrclibClient::new(interval, Duration::from_secs(2), Shutdown::default()).unwrap();
        client.base_url = server.url.clone();
        client
    }

    #[test]
    fn blank_synced_lyrics_fall_back_to_plain() {
        let server = TestServer::new(|_| {
            let mut body = lyrics(&tags(), " \n\t ");
            body["plainLyrics"] = "plain fixture lyrics".into();
            Response::json(200, body)
        });
        assert_eq!(
            client(&server, Duration::ZERO).fetch(&tags(), true),
            LyricsResult::Found("plain fixture lyrics".into())
        );
        assert!(
            server.requests.lock().unwrap()[0]
                .user_agent
                .contains(env!("CARGO_PKG_REPOSITORY"))
        );
    }

    #[test]
    fn album_miss_uses_only_a_unique_matching_recording() {
        let server = TestServer::new(|request| {
            if request.path == "/get" {
                return Response::json(404, serde_json::json!({}));
            }
            assert!(!request.query.contains_key("album_name"));
            let mut recording = tags();
            recording.album = "Fixture Single".into();
            Response::json(
                200,
                serde_json::json!([lyrics(&recording, "matching lyrics")]),
            )
        });
        assert_eq!(
            client(&server, Duration::ZERO).fetch(&tags(), true),
            LyricsResult::Found("matching lyrics".into())
        );
    }

    #[test]
    fn missing_album_requires_an_unambiguous_search_even_with_fallback_disabled() {
        let server = TestServer::new(|request| {
            assert_eq!(request.path, "/search");
            let first = lyrics(&tags(), "first recording");
            let mut second = first.clone();
            second["albumName"] = "Fixture Single".into();
            second["syncedLyrics"] = "another recording".into();
            Response::json(200, serde_json::json!([first, second]))
        });
        let mut track = tags();
        track.album.clear();
        assert_eq!(
            client(&server, Duration::ZERO).fetch(&track, false),
            LyricsResult::Ambiguous
        );
    }

    #[test]
    fn rejects_wrong_title_duration_and_conflicting_recordings() {
        for variant in ["title", "duration", "conflict", "live-album"] {
            let server = TestServer::new(move |request| {
                if request.path == "/get" {
                    return Response::json(404, serde_json::json!({}));
                }
                let mut first = lyrics(&tags(), "first recording");
                first["albumName"] = "Fixture Single".into();
                match variant {
                    "title" => first["trackName"] = "Different Song".into(),
                    "duration" => first["duration"] = 203.into(),
                    "live-album" => first["albumName"] = "Live at Fixture Hall".into(),
                    _ => {}
                }
                let mut rows = vec![first.clone()];
                if variant == "conflict" {
                    first["syncedLyrics"] = "another recording".into();
                    rows.push(first);
                }
                Response::json(200, serde_json::json!(rows))
            });
            let result = client(&server, Duration::ZERO).fetch(&tags(), true);
            assert_eq!(
                result,
                if variant == "conflict" {
                    LyricsResult::Ambiguous
                } else {
                    LyricsResult::NotFound
                }
            );
        }
    }

    #[test]
    fn validates_even_a_successful_exact_lookup() {
        let server = TestServer::new(|_| {
            let mut body = lyrics(&tags(), "wrong recording");
            body["trackName"] = "Different Song".into();
            Response::json(200, body)
        });
        assert_eq!(
            client(&server, Duration::ZERO).fetch(&tags(), false),
            LyricsResult::NotFound
        );
    }

    #[test]
    fn rejected_exact_candidate_uses_enabled_search() {
        let server = TestServer::new(|request| {
            if request.path == "/get" {
                let mut wrong = tags();
                wrong.duration_secs = 500;
                return Response::json(200, lyrics(&wrong, "wrong duration"));
            }
            Response::json(
                200,
                serde_json::json!([lyrics(&tags(), "validated recording")]),
            )
        });
        assert_eq!(
            client(&server, Duration::ZERO).fetch(&tags(), true),
            LyricsResult::Found("validated recording".into())
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn records_without_lyrics_do_not_conflict_with_usable_content() {
        let server = TestServer::new(|request| {
            let empty = lyrics(&tags(), "");
            if request.path == "/get" {
                return Response::json(200, empty);
            }
            let mut usable = lyrics(&tags(), "available lyrics");
            usable["id"] = 124.into();
            Response::json(200, serde_json::json!([empty, usable]))
        });
        assert_eq!(
            client(&server, Duration::ZERO).fetch(&tags(), true),
            LyricsResult::Found("available lyrics".into())
        );
    }

    #[test]
    fn empty_exact_album_cannot_hide_usable_relaxed_album_lyrics() {
        let server = TestServer::new(|request| {
            let empty = lyrics(&tags(), "");
            if request.path == "/get" {
                return Response::json(200, empty);
            }
            let mut usable = lyrics(&tags(), "available lyrics");
            usable["id"] = 124.into();
            usable["albumName"] = "Another Album".into();
            Response::json(200, serde_json::json!([empty, usable]))
        });
        assert_eq!(
            client(&server, Duration::ZERO).fetch(&tags(), true),
            LyricsResult::Found("available lyrics".into())
        );
    }

    #[test]
    fn unknown_duration_cannot_verify_a_recording() {
        let server =
            TestServer::new(|_| Response::json(200, lyrics(&tags(), "unverified duration")));
        let mut track = tags();
        track.duration_secs = 0;
        assert_eq!(
            client(&server, Duration::ZERO).fetch(&track, true),
            LyricsResult::Ambiguous
        );
        assert!(server.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn instrumental_is_distinct_from_missing() {
        let server = TestServer::new(|_| {
            let mut body = lyrics(&tags(), "");
            body["instrumental"] = true.into();
            Response::json(200, body)
        });
        assert_eq!(
            client(&server, Duration::ZERO).fetch(&tags(), true),
            LyricsResult::Instrumental
        );
    }

    #[test]
    fn long_tracks_omit_invalid_query_duration_but_validate_the_result() {
        let server = TestServer::new(|request| {
            assert!(!request.query.contains_key("duration"));
            assert_eq!(request.path, "/search");
            assert_eq!(request.query["album_name"], tags().album);
            let mut track = tags();
            track.duration_secs = 4000;
            Response::json(
                200,
                serde_json::json!([lyrics(&track, "long fixture lyrics")]),
            )
        });
        let mut track = tags();
        track.duration_secs = 4000;
        assert!(matches!(
            client(&server, Duration::ZERO).fetch(&track, true),
            LyricsResult::Found(_)
        ));
    }

    #[test]
    fn shared_gate_waits_for_response_completion() {
        let times = Arc::new(Mutex::new(Vec::new()));
        let observed = times.clone();
        let server = TestServer::new(move |_| {
            observed.lock().unwrap().push(Instant::now());
            let mut response = Response::json(200, lyrics(&tags(), "fixture lyrics"));
            response.delay = Duration::from_millis(150);
            response
        });
        let client = client(&server, Duration::from_millis(30));
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let client = client.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    client.fetch(&tags(), false)
                })
            })
            .collect();
        barrier.wait();
        for handle in handles {
            assert!(matches!(handle.join().unwrap(), LyricsResult::Found(_)));
        }
        let times = times.lock().unwrap();
        assert!(times[1].duration_since(times[0]) >= Duration::from_millis(175));
    }

    #[test]
    fn failed_body_read_still_delays_the_next_request() {
        let server = TestServer::new(|_| {
            let mut response = Response::json(200, lyrics(&tags(), "fixture lyrics"));
            response.body_delay = Duration::from_millis(250);
            response
        });
        let mut client = client(&server, Duration::from_millis(120));
        client.client = Client::builder()
            .timeout(Duration::from_millis(150))
            .build()
            .unwrap();
        assert!(matches!(
            client.fetch(&tags(), false),
            LyricsResult::TemporaryFailure(_)
        ));
        let failed_at = Instant::now();
        assert!(matches!(
            client.fetch(&tags(), false),
            LyricsResult::TemporaryFailure(_)
        ));
        assert!(
            failed_at.elapsed() >= Duration::from_millis(250),
            "the second body timeout must start after the shared interval"
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn final_429_retains_a_shared_cooldown_and_cancel_interrupts_it() {
        let server = TestServer::new(|_| {
            let mut response = Response::json(429, serde_json::json!({}));
            response.headers.push(("Retry-After".into(), "1".into()));
            response
        });
        let client = client(&server, Duration::ZERO);
        assert!(matches!(
            client.fetch(&tags(), false),
            LyricsResult::TemporaryFailure(_)
        ));
        let requests = server.requests.lock().unwrap().len();
        let another = client.clone();
        let handle = thread::spawn(move || another.fetch(&tags(), false));
        thread::sleep(Duration::from_millis(30));
        client.shutdown.cancel();
        assert_eq!(handle.join().unwrap(), LyricsResult::Cancelled);
        assert_eq!(server.requests.lock().unwrap().len(), requests);
    }

    #[test]
    fn retry_after_supports_dates_and_does_not_cap_delays() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(retry_after(Some("600"), now), Duration::from_secs(600));
        let date = httpdate::fmt_http_date(now + Duration::from_secs(300));
        assert_eq!(retry_after(Some(&date), now), Duration::from_secs(300));
    }

    #[test]
    fn http_status_is_preserved_in_temporary_failure() {
        let server = TestServer::new(|_| Response::json(503, serde_json::json!({})));
        match client(&server, Duration::ZERO).fetch(&tags(), true) {
            LyricsResult::TemporaryFailure(reason) => assert!(reason.contains("503")),
            other => panic!("unexpected result {other:?}"),
        }
    }
}
