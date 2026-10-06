use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::config::{Config, OrphanAction, WatchMode};
use crate::tags::TrackTags;

pub fn config(root: &Path) -> Config {
    Config {
        music_dir: root.to_owned(),
        db_file: root.join("cache.sqlite3"),
        concurrency: 2,
        clean_fallback: true,
        retry_not_found_days: 7,
        request_interval: Duration::ZERO,
        request_timeout: Duration::from_secs(2),
        orphan_action: OrphanAction::Keep,
        follow_symlinks: false,
        startup_scan: true,
        fallback_interval: None,
        watch_mode: WatchMode::Native,
        poll_interval: Duration::from_secs(1),
        max_pending_dirs: 4,
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub path: String,
    pub query: HashMap<String, String>,
    pub user_agent: String,
}

pub struct Response {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
    pub delay: Duration,
    pub body_delay: Duration,
}

impl Response {
    pub fn json(status: u16, value: serde_json::Value) -> Self {
        Self {
            status,
            body: value.to_string(),
            headers: vec![],
            delay: Duration::ZERO,
            body_delay: Duration::ZERO,
        }
    }
}

pub struct TestServer {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    pub fn new(respond: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_requests = requests.clone();
        let worker_stop = stop.clone();
        let respond = Arc::new(respond);
        let handle = thread::spawn(move || {
            let mut connections = Vec::new();
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let respond = respond.clone();
                        let requests = worker_requests.clone();
                        connections.push(thread::spawn(move || {
                            serve(stream, requests, respond.as_ref())
                        }));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(err) => panic!("test server accept: {err}"),
                }
            }
            for handle in connections {
                handle.join().unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap();
    }
}

fn serve(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<Request>>>,
    respond: &impl Fn(&Request) -> Response,
) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") && bytes.len() < 16_384 {
        if stream.read(&mut byte).unwrap_or(0) == 0 {
            return;
        }
        bytes.push(byte[0]);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let target = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
    let request = Request {
        path: url.path().to_owned(),
        query: url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect(),
        user_agent: headers
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| key.eq_ignore_ascii_case("user-agent"))
                    .map(|(_, value)| value.trim().to_owned())
            })
            .unwrap_or_default(),
    };
    requests.lock().unwrap().push(request.clone());
    let response = respond(&request);
    thread::sleep(response.delay);
    let extra = response
        .headers
        .iter()
        .map(|(key, value)| format!("{key}: {value}\r\n"))
        .collect::<String>();
    let _ = write!(
        stream,
        "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
        response.status,
        response.body.len(),
        extra
    );
    let _ = stream.flush();
    thread::sleep(response.body_delay);
    let _ = stream.write_all(response.body.as_bytes());
}

pub fn tags() -> TrackTags {
    TrackTags {
        artist: "Fixture Artist".into(),
        title: "Fixture Song".into(),
        album: "Fixture Album".into(),
        duration_secs: 200,
    }
}

pub fn lyrics(tags: &TrackTags, text: &str) -> serde_json::Value {
    serde_json::json!({ "id": 123, "artistName": tags.artist, "trackName": tags.title, "albumName": tags.album, "duration": tags.duration_secs, "instrumental": false, "syncedLyrics": text, "plainLyrics": null })
}

/// A FLAC metadata fixture with STREAMINFO and Vorbis comments, without audio frames.
pub fn write_flac(path: &Path, tags: &TrackTags) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut data = b"fLaC".to_vec();
    data.extend([0, 0, 0, 34]);
    let mut info = [0u8; 34];
    info[0..2].copy_from_slice(&4096u16.to_be_bytes());
    info[2..4].copy_from_slice(&4096u16.to_be_bytes());
    let properties = (44_100u64 << 44) | (15 << 36) | (tags.duration_secs as u64 * 44_100);
    info[10..18].copy_from_slice(&properties.to_be_bytes());
    data.extend(info);
    let vendor = "lrc-sync tests";
    let comments = [
        format!("ARTIST={}", tags.artist),
        format!("TITLE={}", tags.title),
        format!("ALBUM={}", tags.album),
    ];
    let mut block = Vec::new();
    block.extend((vendor.len() as u32).to_le_bytes());
    block.extend(vendor.as_bytes());
    block.extend((comments.len() as u32).to_le_bytes());
    for comment in comments {
        block.extend((comment.len() as u32).to_le_bytes());
        block.extend(comment.as_bytes());
    }
    data.push(0x84);
    data.extend(&(block.len() as u32).to_be_bytes()[1..]);
    data.extend(block);
    fs::write(path, data).unwrap();
}
