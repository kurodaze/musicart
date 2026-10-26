//! Finding what to play: yt-dlp turns links and searches into tracks and their audio streams.
//! Spotify tracks are matched to their recording on YouTube with the togematch library, or by the
//! optional togematch-api, or play Spotify's own audio with the optional Spotify login.

use crate::eq::{self, Preset};
use crate::recent::{KEPT, KEPT_FOR, LINKS_KEPT_FOR, Recent};
use crate::spotify::{HTTP_TIMEOUT, Spotify};
use crate::tools;
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RANGE};
use serde::Deserialize;
use serde_json::Value;
use serenity::all::UserId;
use songbird::input::{AudioStream, Compose, HttpRequest, Input, LiveInput};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use symphonia::core::io::MediaSource;
use togematch::Candidate;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

/// Opus first: songbird sends it to Discord as is, without decoding and re-encoding.
const FORMAT: &str = "bestaudio[acodec=opus]/bestaudio/best";
/// Highest bitrate for SoundCloud (e.g. 160k AAC or HQ streams over 64k Opus).
const SOUNDCLOUD_FORMAT: &str = "bestaudio/best";
const YTDLP_TIMEOUT: Duration = Duration::from_secs(30);
/// A first match can take a few searches.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(90);
/// togematch-api is on the local network, so a connection that takes longer isn't coming.
const API_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest to wait for any other server to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// After togematch-api can't be reached, lookups search YouTube themselves this long.
const API_DOWN_FOR: Duration = Duration::from_secs(60);
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// A track that stops this much before its end had its stream fail.
const CUT_SHORT: Duration = Duration::from_secs(5);
const PLAYLIST_MAX: &str = "500";
/// Said for a link that isn't YouTube's, SoundCloud's or Spotify's.
const OTHER_LINK: &str =
    "it's not a YouTube or SoundCloud link, or a Spotify track, album or playlist";
/// Said for every lookup while YouTube wants the server to prove it isn't a bot.
const BOT_CHECK: &str = "YouTube wants this server to prove it isn't a bot, so nothing plays for now. That usually passes within a few hours";
/// Said for every lookup while yt-dlp can't be run.
const NO_YTDLP: &str = "yt-dlp isn't working, check the bot's log";
/// How long the cookies are used after YouTube asks to sign in.
const LOGGED_IN_FOR: Duration = Duration::from_secs(3600);
/// Least time between yt-dlp starts, so a burst of requests reaches YouTube spaced out.
const LOOKUP_GAP: Duration = Duration::from_secs(1);
/// Lookups for a stream link that serves the whole audio; the last one is kept either way.
const LINK_TRIES: usize = 3;
/// When the last yt-dlp lookup started.
static LAST_LOOKUP: tokio::sync::Mutex<Option<Instant>> = tokio::sync::Mutex::const_new(None);

pub static ACTIVE_COOKIES: Mutex<Option<String>> = Mutex::new(None);

/// yt-dlp, and the cookies of a logged-in YouTube account if set. YouTube trusts a
/// logged-in session much more, but its lookups are slower and its links take a moment to work,
/// so the cookies are only used for a while after YouTube asks to sign in.
pub struct Ytdlp {
    pub cookies: Option<String>,
}

impl Ytdlp {
    pub fn cookies_file(&self) -> Option<String> {
        if let Some(file) = &self.cookies {
            return Some(file.clone());
        }
        crate::find_cookies(crate::DATA).map(|p| p.to_string_lossy().into_owned())
    }
}

/// Set once the log has said so, to say it only once.
static COOKIES_MISSING: AtomicBool = AtomicBool::new(false);
static COOKIES_DEAD: AtomicBool = AtomicBool::new(false);
/// When YouTube last asked to sign in.
static ASKED: Mutex<Option<Instant>> = Mutex::new(None);
/// yt-dlp rewrites its cookies file when it's done, so a lookup starting meanwhile could read it
/// half written. Each lookup gets its own copy, and copies go in and out one at a time.
static COOKIES_TURN: Mutex<()> = Mutex::new(());

/// How often yt-dlp checks for a new release of itself, besides at startup. YouTube changes
/// often, and an old yt-dlp stops working.
pub const UPDATE_EVERY: Duration = Duration::from_secs(6 * 3600);
/// A lookup that fails asking for an update checks again at most this often.
const UPDATE_GAP: Duration = Duration::from_secs(600);
const UPDATE_TIMEOUT: Duration = Duration::from_secs(120);
/// yt-dlp's own words when a newer version may fix a failure.
const OUTDATED: &str = "Confirm you are on the latest version";
/// yt-dlp's reasons it can't update itself, like a copy from a package manager.
const CANT_UPDATE: &[&str] = &[
    "Use that to update",
    "Re-download the latest release",
    "cannot be updated",
    "Use git to pull",
    "Build the executable again",
    "Unable to write to",
];
/// When yt-dlp last checked for a new release, and last installed one.
static UPDATES: tokio::sync::Mutex<(Option<Instant>, Option<Instant>)> =
    tokio::sync::Mutex::const_new((None, None));
/// Set once yt-dlp has said it can't update itself.
static UPDATES_OFF: AtomicBool = AtomicBool::new(false);

impl Ytdlp {
    /// Runs yt-dlp's own updater, unless it ran less than `gap` ago. True if a new version came
    /// in, then or within `gap`. One not downloaded yet comes new on first use anyway.
    pub async fn update(&self, gap: Duration) -> bool {
        if UPDATES_OFF.load(Ordering::Relaxed) || !Path::new(tools::YTDLP).is_file() {
            return false;
        }
        let mut last = UPDATES.lock().await;
        let recent = |at: Option<Instant>| at.is_some_and(|at| at.elapsed() < gap);
        if recent(last.0) {
            return recent(last.1);
        }
        last.0 = Some(Instant::now());
        let run = Command::new(tools::YTDLP)
            .arg("-U")
            .kill_on_drop(true)
            .output();
        let out = match tokio::time::timeout(UPDATE_TIMEOUT, run).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                warn!("yt-dlp: couldn't check for a new version: {e}");
                return false;
            }
            Err(_) => {
                warn!("yt-dlp: checking for a new version took too long");
                return false;
            }
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = stdout.lines().find(|l| l.starts_with("Updated yt-dlp to")) {
            info!("{line}");
            last.1 = Some(Instant::now());
            return true;
        }
        if !out.status.success() {
            let why = failure(&out);
            if CANT_UPDATE.iter().any(|reason| why.contains(reason)) {
                UPDATES_OFF.store(true, Ordering::Relaxed);
                warn!(
                    "yt-dlp can't update itself, so delete {} to get a new one. It says: {why}",
                    tools::YTDLP
                );
            } else {
                warn!("yt-dlp: couldn't update, trying again later: {why}");
            }
        }
        false
    }

    /// Lookups use the cookies now.
    pub fn logged_in(&self) -> bool {
        self.cookies_file().is_some()
            && ASKED
                .lock()
                .unwrap()
                .is_some_and(|t| t.elapsed() < LOGGED_IN_FOR)
    }

    /// A yt-dlp command running JavaScript with `js`, with its own copy of the cookies if
    /// `logged_in`.
    fn command(&self, logged_in: bool, js: &str) -> (Command, Option<Cookies>) {
        let mut command = Command::new(tools::YTDLP);
        command.args(["--js-runtimes", js]);
        let cookies_path = self.cookies_file();
        let file = cookies_path.as_deref().filter(|_| logged_in);
        if logged_in {
            let mut active = ACTIVE_COOKIES.lock().unwrap_or_else(|e| e.into_inner());
            if *active != cookies_path {
                if let Some(path) = &cookies_path {
                    info!("cookies: loaded from {path}");
                }
                *active = cookies_path.clone();
            }
        }
        let cookies = file.and_then(|file| match Cookies::copy(file) {
            Ok(cookies) => Some(cookies),
            Err(e) => {
                if !COOKIES_MISSING.swap(true, Ordering::Relaxed) {
                    warn!("cookies: couldn't read {file} ({e}), looking things up logged out");
                }
                None
            }
        });
        if let Some(cookies) = &cookies {
            command.arg("--cookies").arg(&cookies.copy);
        }
        (command, cookies)
    }

    /// Logs once when YouTube stops accepting the cookies. yt-dlp then carries on logged out.
    fn check_cookies(&self, out: &Output) {
        let dead = String::from_utf8_lossy(&out.stderr).contains("cookies are no longer valid");
        if dead && !COOKIES_DEAD.swap(true, Ordering::Relaxed) {
            let cookies_path = self.cookies_file();
            let file = cookies_path.as_deref().unwrap_or_default();
            warn!(
                "cookies: YouTube no longer accepts the ones in {file}, looking things up logged out. Export new ones, see the README"
            );
        }
    }
}

/// One lookup's copy of the cookies file, removed when dropped.
struct Cookies {
    file: String,
    copy: PathBuf,
    /// the file as copied
    read: Vec<u8>,
}

impl Cookies {
    fn copy(file: &str) -> io::Result<Self> {
        static RUNS: AtomicU64 = AtomicU64::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let copy =
            std::env::temp_dir().join(format!("musicart-{}-{run}.cookies", std::process::id()));
        let read = {
            let _turn = COOKIES_TURN.lock().unwrap();
            fs::read(file)?
        };
        let cookies = Cookies {
            file: file.to_string(),
            copy,
            read,
        };
        // one left behind by a run that was killed
        let _ = fs::remove_file(&cookies.copy);
        // only for us: it holds a logged-in session
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&cookies.copy)?
            .write_all(&cookies.read)?;
        Ok(cookies)
    }

    /// Hands what yt-dlp learned back to the file, unless it was replaced meanwhile, by hand or
    /// by another lookup: that copy is as new.
    fn save(&self) {
        let Ok(learned) = fs::read(&self.copy) else {
            return;
        };
        if learned == self.read {
            return;
        }
        let _turn = COOKIES_TURN.lock().unwrap();
        if fs::read(&self.file).is_ok_and(|now| now == self.read)
            && let Err(e) = fs::write(&self.file, learned)
        {
            warn!("cookies: couldn't save {}: {e}", self.file);
        }
    }
}

impl Drop for Cookies {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.copy);
    }
}

/// Everything finding audio needs.
pub struct Finder {
    pub ytdlp: Ytdlp,
    pub http: reqwest::Client,
    /// optional togematch-api, e.g. `http://192.168.1.10:7094`
    pub api: Option<String>,
    /// for togematch-api, giving up on connecting sooner
    api_http: reqwest::Client,
    /// togematch-api couldn't be reached, so it's skipped until then
    api_down: Mutex<Option<Instant>>,
    /// optional Spotify login, for Spotify's own audio
    pub spotify: Option<Spotify>,
    /// matched tracks, so a replay doesn't search again; none if it isn't on YouTube
    matches: Recent<String, Option<String>>,
    /// stream links by YouTube page, so a replay doesn't look its audio up again
    links: Recent<String, Stream>,
    /// the page, title and length a search or YouTube link found
    found: Recent<String, (String, String, Option<Duration>)>,
    /// the track a Spotify search means, if any
    spotify_searches: Recent<String, Option<(String, togematch::Track)>>,
    /// Spotify track links, as their embed page tells
    spotify_links: Recent<String, togematch::Track>,
    /// videos YouTube no longer has, and why
    gone: Recent<String, String>,
    /// lookups running now, so the same one waits for it rather than running twice
    running: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

impl Finder {
    pub fn new(ytdlp: Ytdlp, api: Option<String>, spotify: Option<PathBuf>) -> Finder {
        let api = api.map(|url| url.trim_end_matches('/').to_string());
        // no total timeout: this client also streams audio for as long as a track lasts
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_default();
        let api_http = reqwest::Client::builder()
            .connect_timeout(API_CONNECT_TIMEOUT)
            .build()
            .unwrap_or_default();
        Finder {
            ytdlp,
            spotify: spotify.map(|login| Spotify::new(login, http.clone())),
            http,
            api,
            api_http,
            api_down: Mutex::default(),
            matches: Recent::new(KEPT_FOR, KEPT),
            links: Recent::new(LINKS_KEPT_FOR, KEPT),
            found: Recent::new(KEPT_FOR, KEPT),
            spotify_searches: Recent::new(KEPT_FOR, KEPT),
            spotify_links: Recent::new(KEPT_FOR, KEPT),
            gone: Recent::new(KEPT_FOR, KEPT),
            running: Mutex::default(),
        }
    }

    /// togematch-api, unless it couldn't be reached a moment ago.
    fn api_up(&self) -> Option<&str> {
        let down = self
            .api_down
            .lock()
            .unwrap()
            .is_some_and(|until| Instant::now() < until);
        self.api.as_deref().filter(|_| !down)
    }

    /// Waits while the same lookup runs elsewhere, so this one finds its result instead of
    /// asking again.
    async fn one_at_a_time(&self, key: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let turn = {
            let mut running = self.running.lock().unwrap();
            running.retain(|_, turn| turn.strong_count() > 0);
            match running.get(key).and_then(Weak::upgrade) {
                Some(turn) => turn,
                None => {
                    let turn = Arc::new(tokio::sync::Mutex::new(()));
                    running.insert(key.to_string(), Arc::downgrade(&turn));
                    turn
                }
            }
        };
        turn.lock_owned().await
    }

    /// A stream link found earlier for this page that's still good.
    fn link(&self, page: &str) -> Option<Stream> {
        self.links.get(page)
    }

    /// Drops a link YouTube refused, so the next lookup gets a new one.
    fn forget_link(&self, page: &str) {
        self.links.remove(page);
    }

    /// Forgets a video YouTube no longer has, so whatever led to it is looked up again, and keeps
    /// why, so the video itself isn't.
    fn forget_page(&self, page: &str, why: &str) {
        self.links.remove(page);
        self.matches.remove_where(|p| p.as_deref() == Some(page));
        self.found.remove_where(|(p, ..)| p == page);
        self.gone.put(page.to_string(), why.to_string());
    }

    /// What a search or YouTube link found before, with its stream link if that's still good.
    fn found_before(&self, key: &str) -> Option<Track> {
        let (page, title, duration) = self.found.get(key)?;
        let mut track = Track::new(title, page.clone(), duration, Lookup::Page(page.clone()));
        track.stream = self.link(&page);
        Some(track)
    }

    /// Keeps what a search or YouTube link found, and its stream link.
    fn remember(&self, key: String, track: Track) -> Found {
        let page = track.page().to_string();
        if let Some(stream) = &track.stream {
            self.links.put(page.clone(), stream.clone());
        }
        self.found
            .put(key, (page, track.title.clone(), track.duration));
        track.into()
    }
}

#[derive(Clone)]
pub struct Track {
    /// tells tracks apart while one is looked up ahead
    pub id: u64,
    pub title: String,
    /// shown in replies: the YouTube or Spotify page
    pub url: String,
    pub duration: Option<Duration>,
    lookup: Lookup,
    stream: Option<Stream>,
    /// already looked up again once after its stream failed
    pub retried: bool,
    /// where to start: after its stream failed, or when the EQ changed
    pub resume_at: Option<Duration>,
    /// who queued it
    pub requester: Option<UserId>,
    /// its Spotify track id, for Spotify's own audio
    spotify: Option<String>,
}

/// How to find a track's audio.
#[derive(Clone)]
pub enum Lookup {
    /// its YouTube page
    Page(String),
    /// a Spotify track, for the togematch library's matcher
    Match(togematch::Track),
    /// a list entry togematch-api hasn't matched yet (`spotify:track:<id>`), and the track
    /// itself for when the API doesn't answer
    Api(String, togematch::Track),
    /// a SoundCloud track link
    SoundCloud(String),
}

impl Lookup {
    /// Tells lookups apart, so the same one runs once at a time.
    fn key(&self) -> String {
        match self {
            Lookup::Page(page) => page.clone(),
            Lookup::Match(wanted) => match_key(wanted),
            Lookup::Api(query, _) => query.clone(),
            Lookup::SoundCloud(url) => url.clone(),
        }
    }
}

#[derive(Clone)]
pub struct Stream {
    url: String,
    headers: HeaderMap,
    size: Option<u64>,
    length: Option<Duration>,
    found: Instant,
    /// served its last byte when found
    checked: bool,
}

/// A playlist found by a link or search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Playlist {
    pub title: String,
    pub url: String,
    pub kind: &'static str,
}

/// What a link or search turned up: one track, or a playlist's.
pub struct Found {
    pub playlist: Option<Playlist>,
    pub tracks: Vec<Track>,
}

impl From<Track> for Found {
    fn from(track: Track) -> Self {
        Found {
            playlist: None,
            tracks: vec![track],
        }
    }
}

/// What yt-dlp says about a video, a playlist, or a playlist entry (which has its page as `url`).
#[derive(Deserialize)]
struct Info {
    #[serde(rename = "_type")]
    kind: Option<String>,
    title: Option<String>,
    webpage_url: Option<String>,
    url: Option<String>,
    duration: Option<f64>,
    #[serde(default)]
    http_headers: HashMap<String, String>,
    filesize: Option<u64>,
    uploader: Option<String>,
    channel: Option<String>,
    artists: Option<Vec<String>>,
    artist: Option<String>,
    track: Option<String>,
    #[serde(default)]
    entries: Vec<Info>,
}

impl Info {
    /// "Artist - Track" like Spotify's: from YouTube's music details, or else the channel's name
    /// before the title unless the title already names it.
    fn shown_title(&self) -> String {
        let title = self.title.clone().unwrap_or_default();
        let artists = self
            .artists
            .as_ref()
            .filter(|a| !a.is_empty())
            .map(|a| a.join(", "));
        if let (Some(artists), Some(track)) = (artists.or(self.artist.clone()), &self.track) {
            return format!("{artists} - {track}");
        }
        let channel = self
            .uploader
            .as_deref()
            .or(self.channel.as_deref())
            .unwrap_or_default();
        let channel = channel.trim_end_matches(" - Topic").trim();
        let named = [" - ", " \u{2013} ", " \u{2014} "]
            .iter()
            .any(|d| title.contains(d))
            || title.to_lowercase().contains(&channel.to_lowercase());
        if channel.is_empty() || named {
            title
        } else {
            format!("{channel} - {title}")
        }
    }
}

/// A YouTube link (a video, or a playlist whose tracks are looked up as they come up), a
/// Spotify link, or a search.
pub async fn find(finder: &Finder, query: &str) -> Result<Found, String> {
    debug!("lookup: query={query:?}");
    let spotify = spotify_link(&finder.http, query).await;
    let is_link = query.starts_with("https://")
        || query.starts_with("http://")
        || query.starts_with("spotify:");
    if spotify.is_none()
        && !is_link
        && let Some(track) = spotify_search(finder, query).await
    {
        debug!("lookup: matched Spotify track {:?}", track.title);
        return Ok(track.into());
    }
    // Spotify's own links play from Spotify, so togematch-api is asked only if one won't
    let spotify_on = finder.spotify.as_ref().is_some_and(Spotify::on);
    if ((spotify.is_some() && !spotify_on) || !is_link)
        && let Some(api) = finder.api_up()
    {
        debug!("lookup: asking togematch-api at {api} for {query:?}");
        match ask_api(finder, api, query).await {
            Ok(Some(found)) => return Ok(found),
            // it searched already, so searching again here would find the same
            Ok(None) if spotify.is_some() => return Err("Not on YouTube".into()),
            Ok(None) => return Err("Nothing found".into()),
            Err(e) => warn!("togematch-api at {api} didn't answer ({e}), searching YouTube"),
        }
    }
    if let Some(link) = spotify {
        debug!("lookup: matched Spotify link {:?}", link);
        return spotify_tracks(finder, link, spotify_on && finder.api.is_some()).await;
    }
    if let Some(url) = soundcloud_link(query) {
        debug!("lookup: matched SoundCloud link {url}");
        if let Some((page, title, duration)) = finder.found.get(&url) {
            debug!("lookup: found in cache ({title:?}, {page})");
            let track = Track::new(title, page.clone(), duration, Lookup::SoundCloud(page));
            return Ok(track.into());
        }
        let args = [
            "-J",
            "--flat-playlist",
            "--no-playlist",
            "--playlist-end",
            PLAYLIST_MAX,
            "-f",
            SOUNDCLOUD_FORMAT,
            "--",
            &url,
        ];
        let info = ytdlp_info(&finder.ytdlp, &args).await?;
        if info.kind.as_deref() != Some("playlist") {
            let title = info.shown_title();
            let page = info.webpage_url.unwrap_or_else(|| url.clone());
            let duration = info.duration.map(Duration::from_secs_f64);
            debug!("lookup SoundCloud: {title:?} ({duration:?}, page: {page})");
            finder
                .found
                .put(url.clone(), (page.clone(), title.clone(), duration));
            if page != url {
                finder
                    .found
                    .put(page.clone(), (page.clone(), title.clone(), duration));
            }
            let track = Track::new(title, page.clone(), duration, Lookup::SoundCloud(page));
            return Ok(track.into());
        }
        let tracks: Vec<Track> = info
            .entries
            .into_iter()
            .filter_map(|e| {
                let duration = e.duration.map(Duration::from_secs_f64);
                e.title.as_ref()?;
                let title = e.shown_title();
                let url = e.url?;
                Some(Track::new(
                    title,
                    url.clone(),
                    duration,
                    Lookup::SoundCloud(url),
                ))
            })
            .collect();
        if tracks.is_empty() {
            return Err("Nothing found".into());
        }
        debug!(
            "lookup SoundCloud playlist: {:?}, {} tracks",
            info.title,
            tracks.len()
        );
        let title = info.title.unwrap_or_else(|| "SoundCloud playlist".into());
        let playlist_url = info.webpage_url.unwrap_or_else(|| url.clone());
        return Ok(Found {
            playlist: Some(Playlist {
                title,
                url: playlist_url,
                kind: "SoundCloud playlist",
            }),
            tracks,
        });
    }
    // yt-dlp would fetch any link, from inside this network, so only YouTube's go to it
    let link = if is_link {
        Some(youtube_link(query).ok_or(OTHER_LINK)?)
    } else {
        None
    };
    // a video's page whatever link it came as, a search's own words whatever their case
    let key = match &link {
        Some(link) => youtube_page(link).unwrap_or_else(|| link.clone()),
        None => query.to_lowercase(),
    };
    if let Some(track) = finder.found_before(&key) {
        debug!("lookup: found in cache ({:?}, {key})", track.title);
        return Ok(track.into());
    }
    let Some(link) = link else {
        let search = format!("ytsearch1:{query}");
        debug!("lookup: searching YouTube {search:?}");
        let info = ytdlp_info(
            &finder.ytdlp,
            &["-J", "--no-playlist", "-f", FORMAT, "--", &search],
        )
        .await?;
        let video = info.entries.into_iter().next().ok_or("Nothing found")?;
        return Ok(finder.remember(key, Track::from_video(video)?));
    };
    debug!("lookup: YouTube link {link:?}");
    let args = [
        "-J",
        "--flat-playlist",
        "--no-playlist",
        "--playlist-end",
        PLAYLIST_MAX,
        "-f",
        FORMAT,
        "--",
        &link,
    ];
    let info = ytdlp_info(&finder.ytdlp, &args).await?;
    if info.kind.as_deref() != Some("playlist") {
        return Ok(finder.remember(key, Track::from_video(info)?));
    }
    let tracks: Vec<Track> = info
        .entries
        .into_iter()
        .filter_map(|e| {
            let duration = e.duration.map(Duration::from_secs_f64);
            e.title.as_ref()?;
            let title = e.shown_title();
            let url = e.url?;
            Some(Track::new(title, url.clone(), duration, Lookup::Page(url)))
        })
        .collect();
    if tracks.is_empty() {
        return Err("Nothing found".into());
    }
    let title = info.title.unwrap_or_else(|| "YouTube playlist".into());
    let playlist_url = info.webpage_url.unwrap_or_else(|| link.clone());
    Ok(Found {
        playlist: Some(Playlist {
            title,
            url: playlist_url,
            kind: "YouTube playlist",
        }),
        tracks,
    })
}

/// A fresh stream, and the YouTube page it's from.
/// `status` hears what it's doing, for a reply that keeps the user posted.
pub async fn stream(
    finder: &Finder,
    lookup: &Lookup,
    status: &(dyn Fn(&'static str) + Sync),
) -> Result<(String, Stream), String> {
    let _turn = finder.one_at_a_time(&lookup.key()).await;
    if !matches!(lookup, Lookup::Page(_)) {
        status("Finding it on YouTube...");
    }
    let page = page_of(finder, lookup).await?;
    if let Some(link) = finder.link(&page) {
        return Ok((page, link));
    }
    if let Some(why) = finder.gone.get(&page) {
        return Err(why);
    }
    status("Getting the audio...");
    let mut tries = 0;
    let link = loop {
        let info = ytdlp_info(
            &finder.ytdlp,
            &["-J", "--no-playlist", "-f", FORMAT, "--", &page],
        )
        .await
        .inspect_err(|e| {
            if gone(e) {
                finder.forget_page(&page, e);
            }
        })?;
        let mut link = Stream::from_video(info)?;
        tries += 1;
        link.checked = link.serves_all(&finder.http).await;
        if link.checked || tries == LINK_TRIES {
            break link;
        }
        if !finder.ytdlp.logged_in() && finder.ytdlp.cookies_file().is_some() {
            info!("YouTube refused stream link, activating cookies for lookups");
            *ASKED.lock().unwrap() = Some(Instant::now());
        }
        info!("YouTube cut the stream link for {page} short, getting another");
    };
    if link.checked {
        finder.links.put(page.clone(), link.clone());
    }
    Ok((page, link))
}

/// The YouTube page a track plays from: its own, or the one found for a Spotify track.
async fn page_of(finder: &Finder, lookup: &Lookup) -> Result<String, String> {
    Ok(match lookup {
        Lookup::Page(url) | Lookup::SoundCloud(url) => url.clone(),
        Lookup::Match(wanted) => match_here(finder, wanted).await?,
        Lookup::Api(query, wanted) => match finder.api_up() {
            Some(api) => match ask_api(finder, api, query).await {
                Ok(found) => found
                    .and_then(|f| f.tracks.into_iter().next())
                    .map(|t| t.lookup)
                    .and_then(|l| match l {
                        Lookup::Page(url) => Some(url),
                        _ => None,
                    })
                    .ok_or("Not on YouTube")?,
                Err(e) => {
                    warn!("togematch-api at {api} didn't answer ({e}), searching YouTube");
                    match_here(finder, wanted).await?
                }
            },
            None => match_here(finder, wanted).await?,
        },
    })
}

fn match_key(wanted: &togematch::Track) -> String {
    format!("{}|{}|{}", wanted.artist, wanted.name, wanted.duration)
}

/// The track's recording on YouTube, picked by the togematch library from one flat search.
/// Nothing matching means YouTube's top result for the plain search, if it's the artist's at the
/// same length. A track that isn't on YouTube is remembered too.
async fn match_here(finder: &Finder, wanted: &togematch::Track) -> Result<String, String> {
    let key = match_key(wanted);
    if let Some(page) = finder.matches.get(&key) {
        return page.ok_or_else(|| "Not on YouTube".into());
    }
    let results = flat_search(&finder.ytdlp, &togematch::queries(wanted)).await?;
    // someone else's upload only when YouTube Music has no official one either
    let hit = match togematch::best(wanted, &results).cloned() {
        Some(hit) if togematch::is_official(wanted, &hit) => Some(hit),
        other => music_songs(&finder.ytdlp, wanted)
            .await
            .ok()
            .and_then(|songs| togematch::best(wanted, &songs).cloned())
            .or(other),
    };
    let page = hit
        .or_else(|| togematch::top(wanted, &results).cloned())
        .map(|hit| format!("https://www.youtube.com/watch?v={}", hit.id));
    finder.matches.put(key, page.clone());
    page.ok_or_else(|| "Not on YouTube".into())
}

impl Track {
    fn new(title: String, url: String, duration: Option<Duration>, lookup: Lookup) -> Track {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        Track {
            id,
            title,
            url,
            duration,
            lookup,
            stream: None,
            retried: false,
            resume_at: None,
            requester: None,
            spotify: None,
        }
    }

    fn from_video(info: Info) -> Result<Track, String> {
        let title = info.shown_title();
        let url = info.webpage_url.clone().ok_or("yt-dlp gave no link")?;
        let duration = info.duration.map(Duration::from_secs_f64);
        let mut track = Track::new(title, url.clone(), duration, Lookup::Page(url));
        track.stream = Some(Stream::from_video(info)?);
        Ok(track)
    }

    pub fn lookup(&self) -> Lookup {
        self.lookup.clone()
    }

    /// Finds the YouTube page a Spotify track plays from, without its audio yet.
    pub async fn locate(
        &mut self,
        finder: &Finder,
        status: &(dyn Fn(&'static str) + Sync),
    ) -> Result<(), String> {
        if !matches!(self.lookup, Lookup::Page(_) | Lookup::SoundCloud(_))
            && !self.on_spotify(finder)
        {
            status("Finding it on YouTube...");
            let _turn = finder.one_at_a_time(&self.lookup.key()).await;
            self.lookup = Lookup::Page(page_of(finder, &self.lookup).await?);
        }
        Ok(())
    }

    /// Looks up the stream now if it's missing, so the track can start at once.
    pub async fn prepare(
        &mut self,
        finder: &Finder,
        status: &(dyn Fn(&'static str) + Sync),
    ) -> Result<(), String> {
        if !self.ready() && !self.on_spotify(finder) {
            let (page, stream) = stream(finder, &self.lookup, status).await?;
            self.set_stream(page, stream);
        }
        Ok(())
    }

    /// Plays Spotify's own audio rather than a YouTube upload.
    pub fn on_spotify(&self, finder: &Finder) -> bool {
        self.spotify.is_some() && finder.spotify.as_ref().is_some_and(Spotify::on)
    }

    /// Playing, or about to play, Spotify's own audio.
    pub fn plays_spotify(&self) -> bool {
        self.stream.is_none() && self.spotify.is_some()
    }

    /// The YouTube page once the track has one, else the link it came from.
    pub fn page(&self) -> &str {
        match &self.lookup {
            Lookup::Page(page) => page,
            Lookup::SoundCloud(url) => url,
            _ => &self.url,
        }
    }

    /// How long what's playing is once its stream is known, else what its source said.
    pub fn length(&self) -> Option<Duration> {
        self.stream
            .as_ref()
            .and_then(|s| s.length)
            .or(self.duration)
    }

    /// Has a stream that's still good.
    pub fn ready(&self) -> bool {
        matches!(self.lookup, Lookup::SoundCloud(_))
            || self
                .stream
                .as_ref()
                .is_some_and(|s| s.found.elapsed() < LINKS_KEPT_FOR)
    }

    /// Keeps a found stream; later lookups start from its YouTube page.
    pub fn set_stream(&mut self, page: String, stream: Stream) {
        self.lookup = Lookup::Page(page);
        self.stream = Some(stream);
    }

    /// Drops a stream that failed, here and in the finder's links.
    pub fn forget_stream(&mut self, finder: &Finder) {
        finder.forget_link(self.page());
        self.stream = None;
    }

    /// Stopped well before the end of its audio, which is how a failed stream ends.
    pub fn cut_short(&self, position: Duration) -> bool {
        let length = match &self.stream {
            Some(stream) => stream.length,
            // Spotify's and SoundCloud's audio length is as their metadata says
            None => self.duration,
        };
        length.is_some_and(|length| position + CUT_SHORT < length)
    }

    /// The audio, looking up its stream first if it's missing, about to expire or cut short.
    pub async fn input(&mut self, finder: &Finder, eq: Preset) -> Result<Input, String> {
        if let (true, Some(spotify), Some(id)) =
            (self.on_spotify(finder), &finder.spotify, &self.spotify)
        {
            debug!("Spotify: opening audio for track {id} ({:?})", self.title);
            match spotify.open(id).await {
                Ok((file, length)) => {
                    self.stream = None;
                    self.duration = Some(length).filter(|l| !l.is_zero()).or(self.duration);
                    let audio = AudioStream {
                        input: Box::new(file) as Box<dyn MediaSource>,
                    };
                    if eq == Preset::Off {
                        return Ok(Input::Live(LiveInput::Raw(audio), None));
                    }
                    debug!("EQ: applying preset {eq:?} to {:?}", self.title);
                    return tokio::task::spawn_blocking(move || eq::open(audio, eq))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Err(e) => {
                    warn!(
                        "Spotify won't play {}: {e}, playing it from YouTube",
                        self.title
                    );
                    self.spotify = None;
                }
            }
        }
        if let Lookup::SoundCloud(ref url) = self.lookup {
            debug!("SoundCloud: streaming {:?} ({url})", self.title);
            tools::ready().await.map_err(|e| {
                error!("yt-dlp: {e}");
                NO_YTDLP
            })?;
            let mut command = std::process::Command::new(tools::YTDLP);
            command
                .args(["-q", "-o", "-", "-f", SOUNDCLOUD_FORMAT, "--", url])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            let child = command
                .spawn()
                .map_err(|e| format!("couldn't start yt-dlp: {e}"))?;
            let container = songbird::input::ChildContainer::from(child);
            if eq == Preset::Off {
                return Ok(container.into());
            }
            debug!("EQ: applying preset {eq:?} to {:?}", self.title);
            let audio = AudioStream {
                input: Box::new(symphonia::core::io::ReadOnlySource::new(container))
                    as Box<dyn MediaSource>,
            };
            return tokio::task::spawn_blocking(move || eq::open(audio, eq))
                .await
                .map_err(|e| e.to_string())?;
        }
        // stream() checks its links; a video played by its link wasn't
        let cut = match &self.stream {
            Some(link) if self.ready() && !link.checked => !link.serves_all(&finder.http).await,
            _ => false,
        };
        if cut {
            info!(
                "YouTube cut {}'s stream link short, getting another",
                self.title
            );
            self.forget_stream(finder);
        }
        if !self.ready() {
            let (page, stream) = stream(finder, &self.lookup, &|_| {}).await?;
            self.set_stream(page, stream);
        }
        let stream = self.stream.as_ref().expect("just set");
        debug!(
            "YouTube: opening HTTP stream for {:?} (size: {:?})",
            self.title, stream.size
        );
        let mut input = HttpRequest::new_with_headers(
            finder.http.clone(),
            stream.url.clone(),
            stream.headers.clone(),
        );
        // with the size known every request asks for a byte range, which YouTube doesn't throttle
        input.content_length = stream.size;
        if eq == Preset::Off {
            return Ok(input.into());
        }
        debug!("EQ: applying preset {eq:?} to {:?}", self.title);
        let audio = input.create_async().await.map_err(|e| e.to_string())?;
        tokio::task::spawn_blocking(move || eq::open(audio, eq))
            .await
            .map_err(|e| e.to_string())?
    }
}

impl Stream {
    /// Whether YouTube serves the link's last byte. Some links serve only the first megabyte or
    /// so, and stay that way. A new link can also answer 403 for several seconds after it's made
    /// (e.g. YouTube server-side pacing or TV ad cooldowns up to ~15s), so a fresh one gets enough
    /// time to start working before we give up.
    async fn serves_all(&self, http: &reqwest::Client) -> bool {
        let last = self.size.unwrap_or(1).saturating_sub(1);
        let max_age = Duration::from_secs(18);
        loop {
            if !self.refuses(http, last).await {
                return true;
            }
            debug!("stream link refused its last byte");
            // If byte 0 is served but the last byte was refused, YouTube truncated the stream.
            if !self.refuses(http, 0).await {
                return false;
            }
            // If both byte 0 and the last byte are refused, the link is in YouTube's initial
            // pacing cooldown. Wait until it starts serving or max_age expires.
            if self.found.elapsed() >= max_age {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
        false
    }

    async fn refuses(&self, http: &reqwest::Client, byte: u64) -> bool {
        http.get(&self.url)
            .headers(self.headers.clone())
            .header(RANGE, format!("bytes={byte}-{byte}"))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .is_ok_and(|r| r.status() == StatusCode::FORBIDDEN)
    }

    fn from_video(info: Info) -> Result<Stream, String> {
        let headers = info
            .http_headers
            .iter()
            .filter_map(|(k, v)| {
                Some((
                    HeaderName::try_from(k).ok()?,
                    HeaderValue::try_from(v).ok()?,
                ))
            })
            .collect();
        let url = info.url.ok_or("yt-dlp found no audio")?;
        let length = info.duration.map(Duration::from_secs_f64);
        Ok(Stream {
            url,
            headers,
            size: info.filesize,
            length,
            found: Instant::now(),
            checked: false,
        })
    }
}

/// Asks togematch-api about a Spotify link or a search: none if it found nothing. One it can't
/// reach is skipped for [`API_DOWN_FOR`], so lookups don't each wait for it.
async fn ask_api(finder: &Finder, api: &str, query: &str) -> Result<Option<Found>, String> {
    let response = finder
        .api_http
        .get(format!("{api}/match"))
        .query(&[("q", query)])
        .timeout(SEARCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            if e.is_connect() {
                *finder.api_down.lock().unwrap() = Some(Instant::now() + API_DOWN_FOR);
            }
            e.to_string()
        })?;
    let status = response.status();
    let body = response.text().await.map_err(|e| e.to_string())?;
    let answer: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(answer["error"]
            .as_str()
            .unwrap_or(status.as_str())
            .to_string());
    }
    Ok(from_api(&answer, query))
}

/// togematch-api's answer as tracks, none if it found nothing. List entries it hasn't matched yet
/// are asked for again when they come up; tracks it found nothing for are dropped.
fn from_api(answer: &Value, query: &str) -> Option<Found> {
    let tracks: Vec<Track> = answer["tracks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let text = |k: &str| s[k].as_str().unwrap_or_default().to_string();
            let (title, artist, query, source) =
                (text("title"), text("artist"), text("query"), text("source"));
            let duration = s["duration"].as_i64().unwrap_or(0);
            let shown = match source.as_str() {
                "youtube" | "" => title.clone(),
                _ if artist.is_empty() => title.clone(),
                _ => format!("{artist} - {title}"),
            };
            let length = (duration > 0).then(|| Duration::from_millis(duration as u64));
            let id = query.strip_prefix("spotify:track:").map(str::to_string);
            let spotify = id
                .as_ref()
                .map(|id| format!("https://open.spotify.com/track/{id}"));
            if let Some(page) = s["youtube"].as_str() {
                let url = spotify.unwrap_or_else(|| page.to_string());
                let mut track = Track::new(shown, url, length, Lookup::Page(page.into()));
                track.spotify = id;
                return Some(track);
            }
            if source != "pending" {
                return None;
            }
            let wanted = togematch::Track {
                name: title,
                artist,
                duration,
                ..Default::default()
            };
            let mut track = Track::new(
                shown,
                spotify.unwrap_or_default(),
                length,
                Lookup::Api(query, wanted),
            );
            track.spotify = id;
            Some(track)
        })
        .collect();
    let playlist = answer["playlist"].as_str().map(|title| {
        let (url, kind) = if query.contains("/album/") || query.starts_with("spotify:album:") {
            let id = query
                .split('/')
                .next_back()
                .unwrap_or(query)
                .strip_prefix("spotify:album:")
                .unwrap_or(query);
            (
                format!("https://open.spotify.com/album/{id}"),
                "Spotify album",
            )
        } else {
            let id = query
                .split('/')
                .next_back()
                .unwrap_or(query)
                .strip_prefix("spotify:playlist:")
                .unwrap_or(query);
            (
                format!("https://open.spotify.com/playlist/{id}"),
                "Spotify playlist",
            )
        };
        Playlist {
            title: title.to_string(),
            url,
            kind,
        }
    });
    (!tracks.is_empty()).then(|| Found { playlist, tracks })
}

/// YouTube's hosts, the only ones whose links go to yt-dlp.
const YOUTUBE: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
    "youtu.be",
];

/// SoundCloud's hosts, the only other ones whose links go to yt-dlp.
const SOUNDCLOUD: &[&str] = &[
    "soundcloud.com",
    "www.soundcloud.com",
    "m.soundcloud.com",
    "on.soundcloud.com",
];

/// A SoundCloud link as parsed here, without a user name or password, for yt-dlp to read the same
/// way.
fn soundcloud_link(link: &str) -> Option<String> {
    let url = reqwest::Url::parse(link).ok()?;
    let soundcloud = matches!(url.scheme(), "https" | "http")
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .host_str()
            .is_some_and(|host| SOUNDCLOUD.contains(&host));
    soundcloud.then(|| url.into())
}

/// A YouTube link as parsed here, without a user name or password, for yt-dlp to read the same
/// way: another parser can take `https://youtube.com\@192.168.1.1/` to be that address.
fn youtube_link(link: &str) -> Option<String> {
    let url = reqwest::Url::parse(link).ok()?;
    let youtube = matches!(url.scheme(), "https" | "http")
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|host| YOUTUBE.contains(&host));
    youtube.then(|| url.into())
}

/// A YouTube video link as its www.youtube.com page, whether it came from YouTube Music,
/// youtu.be, Shorts or with extra parameters, so they all share what was found for it.
fn youtube_page(link: &str) -> Option<String> {
    let url = reqwest::Url::parse(link).ok()?;
    let mut parts = url.path_segments()?;
    let id = match url.host_str()? {
        "youtu.be" => parts.next()?.to_string(),
        host if YOUTUBE.contains(&host) => match parts.next()? {
            "watch" => url.query_pairs().find(|(k, _)| k == "v")?.1.into_owned(),
            "shorts" | "live" | "embed" => parts.next()?.to_string(),
            _ => return None,
        },
        _ => return None,
    };
    let valid = id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    valid.then(|| format!("https://www.youtube.com/watch?v={id}"))
}

/// A Spotify link's kind ("track", "album" or "playlist") and id. Short spotify.link links are followed.
async fn spotify_link(http: &reqwest::Client, query: &str) -> Option<(String, String)> {
    // the id goes into Spotify's URLs as it is, so it has to be one: 22 letters and digits
    let item = |kind: &str, id: &str| {
        let valid = matches!(kind, "track" | "album" | "playlist")
            && id.len() == 22
            && id.bytes().all(|b| b.is_ascii_alphanumeric());
        valid.then(|| (kind.into(), id.into()))
    };
    if let Some(uri) = query.strip_prefix("spotify:") {
        let (kind, id) = uri.split_once(':')?;
        return item(kind, id);
    }
    let mut url = reqwest::Url::parse(query).ok()?;
    if url.host_str() == Some("spotify.link") {
        url = http
            .get(url)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .ok()?
            .url()
            .clone();
    }
    if url.host_str() != Some("open.spotify.com") {
        return None;
    }
    // "/intl-de/track/<id>", "/track/<id>"
    let mut parts = url.path_segments()?.skip_while(|p| p.starts_with("intl-"));
    let (kind, id) = (parts.next()?, parts.next()?);
    item(kind, id)
}

/// With Spotify on, the Spotify track a search means, as if its link was given.
async fn spotify_search(finder: &Finder, query: &str) -> Option<Track> {
    let spotify = finder.spotify.as_ref().filter(|s| s.on())?;
    let key = query.to_lowercase();
    let meant = match finder.spotify_searches.get(&key) {
        Some(meant) => meant,
        None => {
            let found = spotify
                .search(query)
                .await
                .inspect_err(|e| warn!("Spotify's search failed ({e}), searching as usual"))
                .ok()?;
            let meant = togematch::searched(query, found.iter().map(|(_, t)| t))
                .into_iter()
                .find(|(_, fit)| fit.unconfirmed.is_empty())
                .map(|(i, _)| found[i].clone());
            finder.spotify_searches.put(key, meant.clone());
            meant
        }
    };
    let (id, wanted) = meant?;
    Some(spotify_track(&id, wanted, finder.api.is_some()))
}

/// A Spotify track: Spotify's audio while Spotify is on, else its YouTube match, through
/// togematch-api if `api`.
fn spotify_track(id: &str, wanted: togematch::Track, api: bool) -> Track {
    let shown = format!("{} - {}", wanted.artist, wanted.name);
    let length = (wanted.duration > 0).then(|| Duration::from_millis(wanted.duration as u64));
    let lookup = if api {
        Lookup::Api(format!("spotify:track:{id}"), wanted)
    } else {
        Lookup::Match(wanted)
    };
    let mut track = Track::new(
        shown,
        format!("https://open.spotify.com/track/{id}"),
        length,
        lookup,
    );
    track.spotify = Some(id.into());
    track
}

/// The tracks behind a Spotify link, from its public embed page (no API keys; playlists stop at 100).
/// Their YouTube matches go through togematch-api if `api`.
async fn spotify_tracks(
    finder: &Finder,
    (kind, id): (String, String),
    api: bool,
) -> Result<Found, String> {
    if kind == "track"
        && let Some(wanted) = finder.spotify_links.get(&id)
    {
        return Ok(spotify_track(&id, wanted, api).into());
    }
    let page = finder
        .http
        .get(format!("https://open.spotify.com/embed/{kind}/{id}"))
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0")
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| "Spotify didn't answer".to_string())?
        .text()
        .await
        .map_err(|_| "Spotify didn't answer".to_string())?;
    let entity = spotify_entity(&page).ok_or("Couldn't read that Spotify page")?;
    if kind == "track" {
        let artists: Vec<&str> = entity["artists"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a["name"].as_str())
            .collect();
        let wanted = togematch::Track {
            name: entity["name"]
                .as_str()
                .ok_or("Couldn't read that Spotify page")?
                .into(),
            artist: artists.join(", "),
            album: String::new(),
            duration: entity["duration"].as_i64().unwrap_or(0),
            explicit: entity["isExplicit"].as_bool().unwrap_or(false),
        };
        finder.spotify_links.put(id.clone(), wanted.clone());
        return Ok(spotify_track(&id, wanted, api).into());
    }
    let name = entity["name"].as_str().unwrap_or_default().to_string();
    // album tracks share the album's name; playlist entries don't say
    let album = if kind == "album" {
        name.clone()
    } else {
        String::new()
    };
    let tracks: Vec<Track> = entity["trackList"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let id = t["uri"].as_str()?.strip_prefix("spotify:track:")?;
            let wanted = togematch::Track {
                name: t["title"].as_str()?.into(),
                // "A,\u{a0}B"
                artist: t["subtitle"].as_str()?.replace('\u{a0}', " "),
                album: album.clone(),
                duration: t["duration"].as_i64().unwrap_or(0),
                explicit: t["isExplicit"].as_bool().unwrap_or(false),
            };
            Some(spotify_track(id, wanted, api))
        })
        .collect();
    if tracks.is_empty() {
        return Err("That Spotify list is empty".into());
    }
    let kind_str = match kind.as_str() {
        "album" => "Spotify album",
        _ => "Spotify playlist",
    };
    Ok(Found {
        playlist: Some(Playlist {
            title: name,
            url: format!("https://open.spotify.com/{kind}/{id}"),
            kind: kind_str,
        }),
        tracks,
    })
}

/// The JSON Spotify's embed page renders from.
fn spotify_entity(page: &str) -> Option<Value> {
    let start = page.find(r#"<script id="__NEXT_DATA__" type="application/json">"#)?;
    let json = &page[start..];
    let json = &json[json.find('>')? + 1..json.find("</script>")?];
    let mut data: Value = serde_json::from_str(json).ok()?;
    Some(data["props"]["pageProps"]["state"]["data"]["entity"].take()).filter(Value::is_object)
}

/// Results of flat YouTube searches (titles, channels, lengths, no streams), in order.
async fn flat_search(ytdlp: &Ytdlp, targets: &[String]) -> Result<Vec<Candidate>, String> {
    let mut args = vec!["--flat-playlist", "--dump-json", "--no-download", "--"];
    args.extend(targets.iter().map(String::as_str));
    let out = run(ytdlp, &args, SEARCH_TIMEOUT).await?;
    let rows: Vec<Candidate> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(candidate)
        .collect();
    if rows.is_empty() && !out.status.success() {
        let last = failure(&out);
        warn!("yt-dlp: {last}");
        return Err(reason(&last));
    }
    Ok(rows)
}

fn candidate(line: &str) -> Option<Candidate> {
    let v: Value = serde_json::from_str(line).ok()?;
    let text = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let id = text("id");
    let uploader = Some(text("uploader"))
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| text("channel"));
    (!id.is_empty()).then(|| Candidate {
        id,
        title: text("title"),
        uploader,
        description: text("description"),
        album: text("album"),
        duration: (v["duration"].as_f64().unwrap_or(0.0) * 1000.0) as i64,
    })
}

async fn ytdlp_info(ytdlp: &Ytdlp, args: &[&str]) -> Result<Info, String> {
    let out = run(ytdlp, args, YTDLP_TIMEOUT).await?;
    if !out.status.success() {
        let last = failure(&out);
        warn!("yt-dlp: {last}");
        return Err(reason(&last));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| {
        error!("unexpected yt-dlp output: {e}");
        "yt-dlp gave something unexpected".into()
    })
}

/// YouTube Music's first songs for the track, with the channel, credit and length the matcher
/// needs, which its flat rows lack. It reaches official uploads a signed-out search can hide.
async fn music_songs(ytdlp: &Ytdlp, wanted: &togematch::Track) -> Result<Vec<Candidate>, String> {
    let end = togematch::MUSIC_SEARCH_RESULTS.to_string();
    let target = togematch::music_search(wanted);
    let args = [
        "--dump-json",
        "--no-download",
        "--playlist-end",
        &end,
        "--",
        &target,
    ];
    let out = run(ytdlp, &args, SEARCH_TIMEOUT).await?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(candidate)
        .collect())
}

/// Waits until [`LOOKUP_GAP`] has passed since the last yt-dlp lookup started. Lookups take turns
/// in the order they came.
async fn wait_turn() {
    let mut last = LAST_LOOKUP.lock().await;
    if let Some(at) = *last {
        tokio::time::sleep_until(tokio::time::Instant::from_std(at + LOOKUP_GAP)).await;
    }
    *last = Some(Instant::now());
}

/// Runs yt-dlp, and once more after a moment if it failed in a way that may pass, like a dropped
/// connection.
async fn run(ytdlp: &Ytdlp, args: &[&str], limit: Duration) -> Result<Output, String> {
    let js = tools::ready().await.map_err(|e| {
        error!("yt-dlp: {e}");
        NO_YTDLP
    })?;
    let (mut retried, mut updated) = (false, false);
    loop {
        wait_turn().await;
        let logged_in = ytdlp.logged_in();
        let (mut command, cookies) = ytdlp.command(logged_in, &js);
        debug!("yt-dlp: running args {args:?}");
        let run = command.args(args).kill_on_drop(true).output();
        let out = match tokio::time::timeout(limit, run).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                error!("couldn't run {}: {e}", tools::YTDLP);
                return Err(NO_YTDLP.into());
            }
            Err(_) => return Err("yt-dlp took too long".into()),
        };
        if let Some(cookies) = &cookies {
            cookies.save();
        }
        ytdlp.check_cookies(&out);
        if out.status.success() {
            debug!("yt-dlp: completed (stdout: {} bytes)", out.stdout.len());
            return Ok(out);
        }
        if !logged_in
            && ytdlp.cookies_file().is_some()
            && failure(&out).contains("Sign in to confirm you")
        {
            info!("YouTube asked to sign in, so lookups use the cookies for the next hour");
            *ASKED.lock().unwrap() = Some(Instant::now());
            continue;
        }
        if !updated
            && String::from_utf8_lossy(&out.stderr).contains(OUTDATED)
            && ytdlp.update(UPDATE_GAP).await
        {
            info!(
                "yt-dlp: trying again with its new version, after: {}",
                failure(&out)
            );
            updated = true;
            continue;
        }
        if retried || lasting(&failure(&out)) {
            return Ok(out);
        }
        warn!("yt-dlp: {}, trying again", failure(&out));
        retried = true;
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

/// yt-dlp's last error.
fn failure(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    stderr
        .lines()
        .rfind(|l| l.starts_with("ERROR:"))
        .unwrap_or("yt-dlp failed")
        .to_string()
}

/// yt-dlp errors that trying again won't fix.
fn lasting(error: &str) -> bool {
    const LASTING: &[&str] = &[
        "Sign in to confirm you",
        "This live event will begin",
        "Premieres in",
        "Unsupported URL",
        "is not a valid URL",
        "Incomplete YouTube ID",
    ];
    gone(error) || LASTING.iter().any(|l| error.contains(l))
}

/// yt-dlp errors saying the video itself is gone: taken down, private or paid.
fn gone(error: &str) -> bool {
    const GONE: &[&str] = &[
        "Video unavailable",
        "This video is unavailable",
        "Private video",
        "This video is private",
        "members-only",
        "requires payment",
        "does not exist",
    ];
    GONE.iter().any(|g| error.contains(g))
}

/// A lookup's failure that every other lookup would hit too, for now.
pub fn fails_all(why: &str) -> bool {
    why == BOT_CHECK || why == NO_YTDLP
}

/// A yt-dlp error as one short sentence for chat, without its links.
fn reason(error: &str) -> String {
    if error.contains("Sign in to confirm you") {
        return BOT_CHECK.into();
    }
    // "ERROR: [youtube] abc123: Video unavailable. This video is private" -> "Video unavailable"
    let reason = error.trim_start_matches("ERROR: ");
    let reason = match reason.strip_prefix('[') {
        Some(rest) => rest.split_once(": ").map_or(rest, |(_, r)| r),
        None => reason,
    };
    let sentence = reason
        .split(". ")
        .next()
        .unwrap_or(reason)
        .trim_end_matches('.');
    if sentence.contains("://") {
        "yt-dlp failed, see the bot's log".into()
    } else {
        sentence.chars().take(150).collect()
    }
}

/// 3:07, or 1:02:03 for an hour or more.
pub fn clock(d: Duration) -> String {
    let s = d.as_secs();
    match s / 3600 {
        0 => format!("{}:{:02}", s / 60, s % 60),
        h => format!("{h}:{:02}:{:02}", s / 60 % 60, s % 60),
    }
}

/// "90", "1:30" or "1:02:03".
pub fn parse_clock(text: &str) -> Option<Duration> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    let secs = parts.iter().try_fold(0u64, |acc, p| {
        acc.checked_mul(60)?.checked_add(p.parse().ok()?)
    })?;
    Some(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_name_the_artist() {
        let title = |json: Value| serde_json::from_value::<Info>(json).unwrap().shown_title();
        let duo = serde_json::json!({"title": "LIVING DEAD DINER GIRLS -HALLOWEEN-", "uploader": "Tommy heavenly6 - Topic", "artists": ["Tommy heavenly6", "奥田俊作"], "track": "LIVING DEAD DINER GIRLS -HALLOWEEN-"});
        assert_eq!(
            title(duo),
            "Tommy heavenly6, 奥田俊作 - LIVING DEAD DINER GIRLS -HALLOWEEN-"
        );
        let named = serde_json::json!({"title": "Basshunter - DotA (Official Video)", "uploader": "Basshunter"});
        assert_eq!(title(named), "Basshunter - DotA (Official Video)");
        assert_eq!(
            title(serde_json::json!({"title": "DotA (Radio Edit)", "uploader": "Basshunter"})),
            "Basshunter - DotA (Radio Edit)"
        );
        assert_eq!(
            title(
                serde_json::json!({"title": "DotA (Radio Edit)", "channel": "Basshunter - Topic"})
            ),
            "Basshunter - DotA (Radio Edit)"
        );
        assert_eq!(
            title(serde_json::json!({"title": "funny cats by bob", "uploader": "Bob"})),
            "funny cats by bob"
        );
        assert_eq!(title(serde_json::json!({"title": "Untitled"})), "Untitled");
    }

    #[test]
    fn errors_for_chat() {
        let bot = "ERROR: [youtube] PWSMDkADxZs: Sign in to confirm you’re not a bot. Use --cookies-from-browser or --cookies for the authentication. See  https://github.com/yt-dlp/yt-dlp/wiki/FAQ for how";
        assert!(reason(bot).starts_with("YouTube wants this server to prove"));
        assert!(fails_all(&reason(bot)));
        assert!(!fails_all(&reason(
            "ERROR: [youtube] abc: Video unavailable. This video is private"
        )));
        assert_eq!(
            reason("ERROR: [youtube] abc: Video unavailable. This video is private"),
            "Video unavailable"
        );
        assert_eq!(
            reason("ERROR: Unsupported URL: https://example.com/x"),
            "yt-dlp failed, see the bot's log"
        );
    }

    #[test]
    fn errors_worth_retrying() {
        assert!(lasting(
            "ERROR: [youtube] PWSMDkADxZs: Sign in to confirm you’re not a bot. Use --cookies"
        ));
        assert!(lasting(
            "ERROR: [youtube] abc: Video unavailable. This video is private"
        ));
        assert!(!lasting(
            "ERROR: [youtube] abc: Unable to download API page: HTTP Error 503: Service Unavailable"
        ));
        assert!(!lasting(
            "ERROR: [youtube] abc: Unable to download webpage: <urlopen error [Errno -3] Temporary failure in name resolution>"
        ));
        // the stream lookup only sees the reason
        assert!(gone(&reason(
            "ERROR: [youtube] aaaaaaaaaaa: This video is unavailable"
        )));
        assert!(!gone(&reason(
            "ERROR: [youtube] PWSMDkADxZs: Sign in to confirm you’re not a bot. Use --cookies"
        )));
    }

    #[test]
    fn cut_short_tracks() {
        let mut track = Track::new("x".into(), "x".into(), None, Lookup::Page("x".into()));
        let secs = Duration::from_secs;
        assert!(!track.cut_short(secs(10)));
        let stream = |length| Stream {
            url: "x".into(),
            headers: HeaderMap::new(),
            size: None,
            length,
            found: Instant::now(),
            checked: false,
        };
        track.stream = Some(stream(Some(secs(200))));
        assert!(track.cut_short(secs(60)));
        assert!(!track.cut_short(secs(199)));
        track.stream = Some(stream(None));
        assert!(!track.cut_short(secs(60)));
        let sc_track = Track::new(
            "x".into(),
            "x".into(),
            Some(secs(200)),
            Lookup::SoundCloud("x".into()),
        );
        assert!(sc_track.cut_short(secs(60)));
        assert!(!sc_track.cut_short(secs(199)));
    }

    #[test]
    fn only_youtube_links_go_to_ytdlp() {
        let playlist = "https://www.youtube.com/playlist?list=PL123";
        assert_eq!(youtube_link(playlist).as_deref(), Some(playlist));
        // YouTube here, and passed on in a form Python's parser doesn't read as 192.168.1.1
        assert_eq!(
            youtube_link(r"https://youtube.com\@192.168.1.1/").as_deref(),
            Some("https://youtube.com/@192.168.1.1/")
        );
        for other in [
            "http://192.168.1.1/reboot",
            "http://localhost:7094/match?q=x",
            "https://youtube.com.example.com/watch?v=2y-L8CURYBg",
            "https://user@youtube.com/watch?v=2y-L8CURYBg",
            "https://soundcloud.com/a/b",
        ] {
            assert_eq!(youtube_link(other), None, "{other}");
        }
    }

    #[tokio::test]
    async fn other_links_are_refused() {
        let ytdlp = Ytdlp { cookies: None };
        let finder = Finder::new(ytdlp, None, None);
        for other in [
            "http://192.168.1.1/reboot",
            "spotify:artist:0gxyHStUsqpMadRV0Di1Qt",
        ] {
            let refused = find(&finder, other).await.err();
            assert_eq!(refused.as_deref(), Some(OTHER_LINK), "{other}");
        }
    }

    #[test]
    fn api_answers() {
        let answer = serde_json::json!({
            "playlist": "Global Warming",
            "tracks": [
                {"title": "Global Warming", "artist": "Pitbull", "duration": 85000, "youtube": "https://www.youtube.com/watch?v=9ohd3dSydY8", "source": "match", "cached": true, "query": "spotify:track:6OmhkSOpvYBokMKQxpIGx2"},
                {"title": "Feel This Moment", "artist": "Pitbull", "duration": 229506, "youtube": null, "source": "pending", "cached": false, "query": "spotify:track:abc"},
                {"title": "Gone", "artist": "Nobody", "duration": 1000, "youtube": null, "source": "none", "cached": true, "query": "spotify:track:def"}
            ]
        });
        let found = from_api(&answer, "spotify:playlist:abc").unwrap();
        assert_eq!(
            found.playlist.as_ref().map(|p| p.title.as_str()),
            Some("Global Warming")
        );
        assert_eq!(found.tracks.len(), 2);
        assert_eq!(found.tracks[0].title, "Pitbull - Global Warming");
        assert_eq!(
            found.tracks[0].url,
            "https://open.spotify.com/track/6OmhkSOpvYBokMKQxpIGx2"
        );
        assert!(matches!(&found.tracks[0].lookup, Lookup::Page(p) if p.ends_with("9ohd3dSydY8")));
        assert!(matches!(&found.tracks[1].lookup, Lookup::Api(q, _) if q == "spotify:track:abc"));
        let nothing = serde_json::json!({"playlist": null, "tracks": [{"title": "x", "artist": "", "duration": 0, "youtube": null, "source": "none", "query": "x"}]});
        assert!(from_api(&nothing, "").is_none());
    }

    #[test]
    fn cookies_go_back_unless_replaced() {
        let file = std::env::temp_dir().join(format!("musicart-test-{}.txt", std::process::id()));
        let name = file.to_str().unwrap();
        fs::write(&file, "exported").unwrap();
        let run = Cookies::copy(name).unwrap();
        fs::write(&run.copy, "learned").unwrap();
        run.save();
        assert_eq!(fs::read_to_string(&file).unwrap(), "learned");

        let run = Cookies::copy(name).unwrap();
        fs::write(&run.copy, "stale").unwrap();
        fs::write(&file, "exported again").unwrap();
        run.save();
        assert_eq!(fs::read_to_string(&file).unwrap(), "exported again");
        let copy = run.copy.clone();
        drop(run);
        assert!(!copy.exists());
        fs::remove_file(&file).unwrap();
    }

    /// Real Spotify and YouTube lookups: `cargo test -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn spotify_online() {
        let finder = Finder::new(Ytdlp { cookies: None }, None, None);
        let found = find(
            &finder,
            "https://open.spotify.com/track/08Hty7Sm9hIExaAXcQRtdl",
        )
        .await
        .unwrap();
        let track = &found.tracks[0];
        assert_eq!(
            track.title,
            "Tommy heavenly6, 奥田俊作 - LIVING DEAD DINER GIRLS -HALLOWEEN-"
        );
        let (page, _) = stream(&finder, &track.lookup(), &|_| {}).await.unwrap();
        assert_eq!(page, "https://www.youtube.com/watch?v=KsvxxSUn7Ow");
        let found = find(&finder, "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M")
            .await
            .unwrap();
        assert!(found.playlist.is_some() && found.tracks.len() > 10);
    }

    /// Against a running togematch-api: `MUSICART_TOGEMATCH_API=url cargo test -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn api_online() {
        let api = std::env::var("MUSICART_TOGEMATCH_API").unwrap_or("http://127.0.0.1:7094".into());
        let finder = Finder::new(Ytdlp { cookies: None }, Some(api), None);
        let found = find(&finder, "basshunter dota").await.unwrap();
        let track = &found.tracks[0];
        assert_eq!(track.title, "Basshunter - DotA (Radio Edit)");
        let (page, _) = stream(&finder, &track.lookup(), &|_| {}).await.unwrap();
        assert_eq!(page, "https://www.youtube.com/watch?v=AWmKSuiFJik");
    }

    /// Real SoundCloud lookup, stream creation and audio decoding: `cargo test -- --ignored soundcloud_online`
    #[tokio::test]
    #[ignore]
    async fn soundcloud_online() {
        let finder = Finder::new(Ytdlp { cookies: None }, None, None);
        let found = find(
            &finder,
            "https://soundcloud.com/coldfleshcolony/bitterblossom",
        )
        .await
        .unwrap();
        assert_eq!(found.tracks.len(), 1);
        let mut track = found.tracks[0].clone();
        assert_eq!(track.title, "ColdFleshColony - Bitterblossom");
        assert!(track.duration.is_some());
        assert!(matches!(track.lookup(), Lookup::SoundCloud(_)));
        let input = track.input(&finder, Preset::Off).await.unwrap();
        let songbird::input::Input::Live(live, _) = input else {
            panic!("expected Input::Live");
        };
        let songbird::input::LiveInput::Parsed(mut parsed) =
            tokio::task::spawn_blocking(move || {
                live.promote(
                    songbird::input::codecs::get_codec_registry(),
                    songbird::input::codecs::get_probe(),
                )
            })
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("expected LiveInput::Parsed");
        };
        let packet = parsed.format.next_packet().unwrap();
        let decoded = parsed.decoder.decode(&packet).unwrap();
        assert!(decoded.frames() > 0);
    }

    #[tokio::test]
    async fn api_down_fails_fast() {
        let api = "http://127.0.0.1:9";
        let finder = Finder::new(Ytdlp { cookies: None }, Some(api.into()), None);
        let started = Instant::now();
        assert!(ask_api(&finder, api, "anything").await.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(finder.api_up(), None);
    }

    #[test]
    fn spotify_page() {
        let page = r#"<html><script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"state":{"data":{"entity":{"type":"track","name":"Track"}}}}}}</script></html>"#;
        assert_eq!(spotify_entity(page).unwrap()["name"], "Track");
        assert!(spotify_entity("<html></html>").is_none());
    }
}
