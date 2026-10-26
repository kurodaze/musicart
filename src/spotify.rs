//! Spotify's own audio for Spotify links, through librespot, on a Premium account. Without a login
//! that works, the log shows a link to sign in with.

use crate::recent::{KEPT, KEPT_FOR, Recent};
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::audio_key::AudioKey;
use librespot_core::authentication::Credentials;
use librespot_core::config::SessionConfig;
use librespot_core::error::ErrorKind;
use librespot_core::spotify_uri::SpotifyUri;
use librespot_core::{FileId, Session, SpotifyId};
use librespot_metadata::Metadata;
use librespot_metadata::audio::AudioFileFormat;
use librespot_metadata::track::Track;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use symphonia::core::io::MediaSource;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Where the login is kept, in the working directory's data folder.
pub const LOGIN: &str = "data/spotify-login.json";
/// Spotify's desktop app, which can sign in another device through spotify.com/pair.
const CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const DEVICE_AUTH_URL: &str = "https://accounts.spotify.com/oauth2/device/authorize";
const APRESOLVE_URL: &str = "https://apresolve.spotify.com/?type=spclient";
const SPCLIENT_HOST: &str = "spclient.wg.spotify.com";
/// Past this, a search goes the usual way.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Spotify's top tracks for a search that are weighed against it.
const SEARCH_RESULTS: usize = 5;
/// Spotify puts this many bytes of its own before the Ogg stream.
const SPOTIFY_HEADER: usize = 0xA7;
/// Best first, with kbps. Discord gets Opus at 144 kbps, so FLAC would only be a bigger download.
const FORMATS: &[(AudioFileFormat, usize)] = &[
    (AudioFileFormat::OGG_VORBIS_320, 320),
    (AudioFileFormat::OGG_VORBIS_160, 160),
    (AudioFileFormat::OGG_VORBIS_96, 96),
];
/// After the session fails to connect, Spotify links play from YouTube this long.
const COOLDOWN: Duration = Duration::from_secs(300);
/// Most a request to Spotify's web endpoints takes, so one that never answers can't hold up
/// everything waiting on the session.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Spotify {
    login: PathBuf,
    http: reqwest::Client,
    /// the login's refresh token; none while waiting for someone to sign in
    refresh: Mutex<Option<String>>,
    token: tokio::sync::Mutex<Option<(String, Instant)>>,
    session: tokio::sync::Mutex<Option<Session>>,
    /// the login stopped working, so a new sign-in link is due
    lost: Notify,
    cooldown: Mutex<Option<Instant>>,
    /// formats the plan refuses (320 kbps on a lower tier), skipped until the next session
    refused: Mutex<HashSet<AudioFileFormat>>,
    /// so a replay asks for neither its metadata nor its key again
    playable: Recent<String, Playable>,
    keys: Recent<(SpotifyId, FileId), AudioKey>,
}

/// What the login file holds.
#[derive(Serialize, Deserialize)]
struct Login {
    refresh_token: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default = "an_hour")]
    expires_in: u64,
}

fn an_hour() -> u64 {
    3600
}

impl Spotify {
    pub fn new(login: PathBuf, http: reqwest::Client) -> Spotify {
        let refresh = std::fs::read_to_string(&login)
            .ok()
            .and_then(|text| serde_json::from_str::<Login>(&text).ok())
            .map(|login| login.refresh_token);
        Spotify {
            login,
            http,
            refresh: Mutex::new(refresh),
            token: tokio::sync::Mutex::default(),
            session: tokio::sync::Mutex::default(),
            lost: Notify::new(),
            cooldown: Mutex::default(),
            refused: Mutex::default(),
            playable: Recent::new(KEPT_FOR, KEPT),
            keys: Recent::new(KEPT_FOR, KEPT),
        }
    }

    /// Whether Spotify links may play from Spotify now.
    pub fn on(&self) -> bool {
        self.refresh.lock().unwrap().is_some()
            && !self
                .cooldown
                .lock()
                .unwrap()
                .is_some_and(|until| Instant::now() < until)
    }

    /// Signs in with the saved login, or logs a link to sign in with, and does that again whenever
    /// the login stops working.
    pub async fn run(&self) {
        loop {
            if self.refresh.lock().unwrap().is_none() {
                self.pair().await;
            }
            match self.session().await {
                Ok(_) => {
                    info!("Spotify: signed in with Premium, Spotify links play Spotify's audio")
                }
                Err(e) => warn!("Spotify: {e}, Spotify links play from YouTube for now"),
            }
            self.lost.notified().await;
        }
    }

    /// Forgets a login that no longer works, so `run` logs a new link.
    fn lose(&self) {
        *self.refresh.lock().unwrap() = None;
        self.lost.notify_one();
    }

    /// Keeps a refresh token, in the login file too so it lasts past a restart.
    fn keep(&self, refresh: &str) {
        *self.refresh.lock().unwrap() = Some(refresh.into());
        if let Err(e) = save(&self.login, refresh) {
            warn!(
                "Spotify: couldn't save the login to {}, so it only lasts until a restart: {e}",
                self.login.display()
            );
        }
    }

    /// Logs a spotify.com/pair link and waits until someone signs in with it. A link lasts an
    /// hour, then a new one replaces it.
    async fn pair(&self) {
        loop {
            match self.pair_once().await {
                Ok(true) => return,
                Ok(false) => {}
                Err(e) => {
                    warn!("Spotify: {e}");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }

    /// One sign-in link. False when it expired unused.
    async fn pair_once(&self) -> Result<bool, String> {
        let response = self
            .http
            .post(DEVICE_AUTH_URL)
            .form(&[("client_id", CLIENT_ID), ("scope", "streaming")])
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| format!("couldn't get a sign-in link: {e}"))?;
        let device: Value = json(response).await?;
        let text = |k: &str| device[k].as_str().unwrap_or_default().to_string();
        let code = text("device_code");
        let link = Some(text("verification_uri_complete"))
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| format!("https://spotify.com/pair (code {})", text("user_code")));
        warn!(
            "Spotify: to play Spotify links from Spotify, open {link} and sign in with a Premium account"
        );
        let expires =
            Instant::now() + Duration::from_secs(device["expires_in"].as_u64().unwrap_or(600));
        let mut every = Duration::from_secs(device["interval"].as_u64().unwrap_or(5));
        let token: TokenResponse = loop {
            tokio::time::sleep(every).await;
            if Instant::now() > expires {
                return Ok(false);
            }
            let poll = self
                .http
                .post(TOKEN_URL)
                .form(&[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", &code),
                ])
                .timeout(HTTP_TIMEOUT)
                .send()
                .await;
            // a dropped connection just waits for the next try
            let Ok(response) = poll else { continue };
            if response.status().is_success() {
                break json(response).await?;
            }
            let error: Value = json(response).await.unwrap_or_default();
            match error["error"].as_str() {
                Some("authorization_pending") => {}
                Some("slow_down") => every += Duration::from_secs(5),
                Some("expired_token") => return Ok(false),
                Some("access_denied") => return Err("the sign-in was declined".into()),
                _ => return Err(format!("signing in failed: {error}")),
            }
        };
        if !premium(&self.http, &token.access_token).await? {
            return Err("that account isn't Premium, which librespot needs".into());
        }
        let refresh = token.refresh_token.ok_or("Spotify gave no refresh token")?;
        self.keep(&refresh);
        *self.token.lock().await = Some((
            token.access_token,
            Instant::now() + Duration::from_secs(token.expires_in),
        ));
        *self.cooldown.lock().unwrap() = None;
        Ok(true)
    }

    /// The track's audio file, decrypted, from where its Ogg stream starts, and how long it is.
    pub async fn open(&self, id: &str) -> Result<(SpotifyFile, Duration), String> {
        let session = self.session().await?;
        let mut playable = match self.playable.get(id) {
            Some(playable) => playable,
            None => playable(&session, id).await?,
        };
        let mut opened = self.first_that_opens(&session, &playable.files).await;
        // a region's own version of the track may play where the main entry won't
        if opened.is_err() && !playable.alternatives.is_empty() {
            let mut files = Vec::new();
            for alternative in std::mem::take(&mut playable.alternatives) {
                if let SpotifyUri::Track { id } = alternative
                    && let Ok(other) = Track::get(&session, &alternative).await
                {
                    files.extend(files_of(&other, id));
                }
            }
            opened = self.first_that_opens(&session, &files).await;
            playable.files.extend(files);
        }
        self.playable.put(id.to_string(), playable.clone());
        let (key, audio) = opened?;

        let controller = audio
            .get_stream_loader_controller()
            .map_err(|e| e.to_string())?;
        controller.set_stream_mode();
        let size = controller.len() as u64;
        // reads wait on the download
        let file = tokio::task::spawn_blocking(move || SpotifyFile::new(key, audio, size))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("couldn't read its audio: {e}"))?;
        Ok((file, playable.length))
    }

    /// The best of these files the plan grants a key for.
    async fn first_that_opens(
        &self,
        session: &Session,
        files: &[(SpotifyId, FileId, AudioFileFormat, usize)],
    ) -> Result<(AudioKey, AudioFile), String> {
        if files.is_empty() {
            return Err("it has no Ogg Vorbis audio".into());
        }
        let skipped = self.refused.lock().unwrap().clone();
        let mut refused_here = Vec::new();
        let mut last = String::from("it refused every format");
        let mut opened = None;
        for &(track_id, file, format, kbps) in files {
            if skipped.contains(&format) {
                continue;
            }
            let key = match self.keys.get(&(track_id, file)) {
                Some(key) => key,
                None => match session.audio_key().request(track_id, file).await {
                    Ok(key) => {
                        self.keys.put((track_id, file), key);
                        key
                    }
                    Err(e) => {
                        // a refusal, not a timeout or a dropped connection
                        if e.kind == ErrorKind::Unavailable {
                            refused_here.push(format);
                        }
                        last = format!("no key for {format:?}: {e}");
                        continue;
                    }
                },
            };
            match AudioFile::open(session, file, kbps * 1000 / 8).await {
                Ok(audio) => {
                    debug!("Spotify: {format:?}");
                    opened = Some((key, audio));
                    break;
                }
                Err(e) => last = format!("couldn't open {format:?}: {e}"),
            }
        }
        let opened = opened.ok_or(last)?;
        // with another format granted, a refusal is the plan's, not the track's
        let mut refused = self.refused.lock().unwrap();
        for format in refused_here {
            if refused.insert(format) {
                info!("Spotify: this plan refuses {format:?}, skipping it until the next session");
            }
        }
        Ok(opened)
    }

    /// The librespot session, signing in again if it dropped. Checks Premium first: librespot
    /// ends the whole process on any other plan.
    async fn session(&self) -> Result<Session, String> {
        if !self.on() {
            return Err("not signed in".into());
        }
        let mut slot = self.session.lock().await;
        if let Some(session) = slot.as_ref().filter(|s| !s.is_invalid()) {
            return Ok(session.clone());
        }
        let connected = async {
            let token = self.access_token().await?;
            if !premium(&self.http, &token).await? {
                self.lose();
                return Err("the account isn't Premium".to_string());
            }
            let session = Session::new(SessionConfig::default(), None);
            session
                .connect(Credentials::with_access_token(token), false)
                .await
                .map_err(|e| format!("couldn't connect: {e}"))?;
            Ok(session)
        }
        .await;
        match connected {
            Ok(session) => {
                // a new session may come with another plan
                self.refused.lock().unwrap().clear();
                *slot = Some(session.clone());
                Ok(session)
            }
            Err(e) => {
                *self.cooldown.lock().unwrap() = Some(Instant::now() + COOLDOWN);
                Err(e)
            }
        }
    }

    /// Spotify's tracks for a typed search, best first, with their IDs. This is the search the
    /// app plays from, through the session: Spotify throttles its public search for this app's ID.
    pub async fn search(&self, text: &str) -> Result<Vec<(String, togematch::Track)>, String> {
        tokio::time::timeout(SEARCH_TIMEOUT, self.search_now(text))
            .await
            .map_err(|_| "Spotify took too long".to_string())?
    }

    async fn search_now(&self, text: &str) -> Result<Vec<(String, togematch::Track)>, String> {
        let session = self.session().await?;
        let query: String = form_urlencoded::byte_serialize(text.as_bytes()).collect();
        let context = session
            .spclient()
            .get_context(&format!("spotify:search:{query}"))
            .await
            .map_err(|e| e.to_string())?;
        let uris: Vec<SpotifyUri> = context
            .pages
            .iter()
            .flat_map(|page| &page.tracks)
            .filter_map(|t| SpotifyUri::from_uri(t.uri()).ok())
            .filter(|uri| matches!(uri, SpotifyUri::Track { .. }))
            .take(SEARCH_RESULTS)
            .collect();
        let mut asked = tokio::task::JoinSet::new();
        for (i, uri) in uris.into_iter().enumerate() {
            let session = session.clone();
            asked.spawn(async move { (i, Track::get(&session, &uri).await.ok(), uri) });
        }
        let mut found = Vec::new();
        while let Some(joined) = asked.join_next().await {
            let Ok((i, Some(track), SpotifyUri::Track { id })) = joined else {
                continue;
            };
            let Ok(base62) = id.to_base62() else {
                continue;
            };
            // the pick plays without asking for its metadata again
            self.playable.put(base62.clone(), playable_of(&track, id));
            let artists: Vec<&str> = track.artists.iter().map(|a| a.name.as_str()).collect();
            let wanted = togematch::Track {
                name: track.name.clone(),
                artist: artists.join(", "),
                album: track.album.name.clone(),
                duration: i64::from(track.duration),
                explicit: track.is_explicit,
            };
            found.push((i, base62, wanted));
        }
        found.sort_by_key(|(i, ..)| *i);
        Ok(found.into_iter().map(|(_, id, t)| (id, t)).collect())
    }

    /// A current access token, from the login's refresh token.
    async fn access_token(&self) -> Result<String, String> {
        let mut cached = self.token.lock().await;
        if let Some((token, until)) = cached.as_ref()
            && Instant::now() + Duration::from_secs(60) < *until
        {
            return Ok(token.clone());
        }
        let refresh = self
            .refresh
            .lock()
            .unwrap()
            .clone()
            .ok_or("not signed in")?;
        let response = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
            ])
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("Spotify didn't answer: {e}"))?;
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            if body.contains("invalid_grant") {
                *cached = None;
                self.lose();
                return Err("the login no longer works".into());
            }
            return Err(format!("couldn't refresh the login: {body}"));
        }
        let token: TokenResponse = json(response).await?;
        // Spotify may hand out a new refresh token, and the old one then stops working
        if let Some(renewed) = token.refresh_token.filter(|r| *r != refresh) {
            self.keep(&renewed);
        }
        *cached = Some((
            token.access_token.clone(),
            Instant::now() + Duration::from_secs(token.expires_in),
        ));
        Ok(token.access_token)
    }
}

/// What a track's metadata says about playing it.
#[derive(Clone)]
struct Playable {
    files: Vec<(SpotifyId, FileId, AudioFileFormat, usize)>,
    /// its regional versions, asked for only when its own files won't play
    alternatives: Vec<SpotifyUri>,
    length: Duration,
}

async fn playable(session: &Session, id: &str) -> Result<Playable, String> {
    let uri = SpotifyUri::from_uri(&format!("spotify:track:{id}")).map_err(|e| e.to_string())?;
    let track = Track::get(session, &uri)
        .await
        .map_err(|e| format!("no metadata: {e}"))?;
    let SpotifyUri::Track { id: track_id } = uri else {
        return Err("not a track".into());
    };
    Ok(playable_of(&track, track_id))
}

fn playable_of(track: &Track, id: SpotifyId) -> Playable {
    Playable {
        files: files_of(track, id),
        alternatives: track.alternatives.0.clone(),
        length: Duration::from_millis(track.duration.max(0) as u64),
    }
}

/// The track's Ogg Vorbis files, best first.
fn files_of(track: &Track, id: SpotifyId) -> Vec<(SpotifyId, FileId, AudioFileFormat, usize)> {
    FORMATS
        .iter()
        .filter_map(|&(format, kbps)| Some((id, *track.files.get(&format)?, format, kbps)))
        .collect()
}

/// The plan, from the account's product state. librespot needs `type` to say "premium" too.
async fn premium(http: &reqwest::Client, token: &str) -> Result<bool, String> {
    let host = async {
        let response = http
            .get(APRESOLVE_URL)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .ok()?;
        let hosts: Value = json(response).await.ok()?;
        hosts["spclient"]
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .find(|h| !h.trim().is_empty())
            .map(str::to_string)
    }
    .await
    .unwrap_or_else(|| SPCLIENT_HOST.into());
    let response = http
        .get(format!("https://{host}/melody/v1/product_state"))
        .bearer_auth(token)
        .header("Accept", "application/json")
        .header("App-Platform", "WebPlayer")
        .header("Client-Id", CLIENT_ID)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("Spotify didn't answer: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "couldn't check the plan: Spotify said {}",
            response.status()
        ));
    }
    let state: Value = json(response).await?;
    Ok(plan_is_premium(&state))
}

fn plan_is_premium(state: &Value) -> bool {
    let field = |k: &str| state[k].as_str();
    field("product").is_some_and(|p| p.eq_ignore_ascii_case("premium"))
        && field("type").is_none_or(|t| t == "premium")
}

async fn json<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T, String> {
    let text = response.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

/// Writes the login readable by this user only.
fn save(path: &Path, refresh_token: &str) -> io::Result<()> {
    let text = serde_json::to_string(&Login {
        refresh_token: refresh_token.into(),
    })?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // an existing file keeps its mode otherwise
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())
}

/// A decrypted Spotify audio file, seen from where its Ogg stream starts.
pub struct SpotifyFile {
    audio: AudioDecrypt<AudioFile>,
    start: u64,
    len: u64,
}

impl SpotifyFile {
    fn new(key: AudioKey, audio: AudioFile, size: u64) -> io::Result<SpotifyFile> {
        let mut audio = AudioDecrypt::new(Some(key), audio);
        let mut head = Vec::new();
        audio.by_ref().take(64 * 1024).read_to_end(&mut head)?;
        let start = ogg_start(&head).ok_or_else(|| io::Error::other("no Ogg stream in it"))? as u64;
        audio.seek(SeekFrom::Start(start))?;
        Ok(SpotifyFile {
            audio,
            start,
            len: size.saturating_sub(start),
        })
    }
}

fn ogg_start(head: &[u8]) -> Option<usize> {
    if head.get(SPOTIFY_HEADER..SPOTIFY_HEADER + 4) == Some(b"OggS") {
        return Some(SPOTIFY_HEADER);
    }
    let vorbis = head.windows(7).position(|w| w == b"\x01vorbis")?;
    head[..vorbis].windows(4).rposition(|w| w == b"OggS")
}

impl Read for SpotifyFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.audio.read(buf)
    }
}

impl Seek for SpotifyFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let to = match pos {
            SeekFrom::Start(at) => self.start + at,
            SeekFrom::End(back) => (self.start + self.len).saturating_add_signed(back),
            SeekFrom::Current(by) => self
                .audio
                .stream_position()?
                .saturating_add_signed(by)
                .max(self.start),
        };
        Ok(self.audio.seek(SeekFrom::Start(to))? - self.start)
    }
}

impl MediaSource for SpotifyFile {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ogg_stream_starts_after_spotifys_header() {
        let mut file = vec![0u8; SPOTIFY_HEADER];
        file.extend_from_slice(b"OggS\x00\x02");
        assert_eq!(ogg_start(&file), Some(SPOTIFY_HEADER));
        let mut shifted = vec![0u8; 10];
        shifted.extend_from_slice(b"OggS....\x01vorbis");
        assert_eq!(ogg_start(&shifted), Some(10));
        assert_eq!(ogg_start(b"no audio here"), None);
    }
}
