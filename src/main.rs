mod bot;
mod config;
mod eq;
mod player;
mod recent;
mod source;
mod spotify;
mod tools;

use config::LogLevel;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinSet;
use tracing::{Level, error, info, warn};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::prelude::*;

/// Where the Spotify login and the cookies file are kept. It holds logged-in sessions, so only
/// this user gets in.
const DATA: &str = "data";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let config = match config::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let dev = cfg!(debug_assertions);
    let level = match std::env::var("RUST_LOG").ok().as_deref() {
        Some(s) if s.eq_ignore_ascii_case("trace") => Level::TRACE,
        Some(s) if s.eq_ignore_ascii_case("debug") => Level::DEBUG,
        Some(s) if s.eq_ignore_ascii_case("info") => Level::INFO,
        Some(s) if s.eq_ignore_ascii_case("warn") => Level::WARN,
        Some(s) if s.eq_ignore_ascii_case("error") => Level::ERROR,
        _ if dev && matches!(config.log_level, LogLevel::Info) => Level::DEBUG,
        _ => match config.log_level {
            LogLevel::Error => Level::ERROR,
            LogLevel::Warn => Level::WARN,
            LogLevel::Info => Level::INFO,
            LogLevel::Debug => Level::DEBUG,
        },
    };
    let filter = if level >= Level::DEBUG {
        Targets::new()
            .with_default(Level::WARN)
            .with_target("musicart", level)
            .with_target("songbird", Level::DEBUG)
            .with_target("serenity", Level::INFO)
    } else {
        Targets::new()
            .with_default(Level::WARN)
            .with_target("musicart", level)
    };
    tracing_subscriber::fmt()
        .with_target(false)
        .with_max_level(level)
        .finish()
        .with(filter)
        .init();

    if level >= Level::DEBUG {
        tracing::debug!("debug logging enabled (dev profile: {dev}, level: {level})");
    }

    if let Err(e) = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(DATA)
    {
        warn!("couldn't create {DATA}/, so a Spotify login or cookies can't be kept there: {e}");
    }

    let ytdlp = source::Ytdlp;
    if let Some(path) = ytdlp.cookies_file() {
        info!("YouTube cookies found at {path} (used when challenged)");
    } else {
        tracing::debug!("no YouTube cookies found in data/");
    }

    let settings = Arc::new(bot::Settings {
        finder: source::Finder::new(
            ytdlp,
            Some(config.togematch_api).filter(|t| !t.is_empty()),
            config.spotify.then(|| spotify::LOGIN.into()),
        ),
        idle_timeout: Duration::from_secs(config.idle_timeout),
        track_status: config.track_status,
        announce_tracks: config.announce_tracks,
        dj_roles: config
            .dj_role_ids
            .into_iter()
            .filter(|&id| id != 0)
            .map(serenity::all::RoleId::new)
            .collect(),
        stun: Some(config.stun).filter(|s| !s.is_empty()),
    });
    let signing_in = settings.clone();
    tokio::spawn(async move {
        if let Some(spotify) = &signing_in.finder.spotify {
            spotify.run().await;
        }
    });
    let updates = settings.clone();
    tokio::spawn(async move {
        // downloads yt-dlp and deno on the first start, so the first lookup doesn't wait for them
        if let Err(e) = tools::ready().await {
            error!("yt-dlp: {e}");
        }
        loop {
            updates.finder.ytdlp.update(Duration::ZERO).await;
            tokio::time::sleep(source::UPDATE_EVERY).await;
        }
    });
    let mut running = JoinSet::new();
    let mut shards = Vec::new();
    for (name, bot) in config.bot {
        match bot::client(&name, &bot, settings.clone()).await {
            Ok(mut client) => {
                shards.push(client.shard_manager.clone());
                running.spawn(async move {
                    if let Err(e) = client.start().await {
                        error!("{name}: stopped: {e}");
                    }
                });
            }
            Err(e) => error!("{name}: couldn't log in: {e}"),
        }
    }

    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
        _ = async { while running.join_next().await.is_some() {} } => {
            error!("no bots left running");
            return ExitCode::FAILURE;
        }
    }
    info!("shutting down");
    for shard_manager in shards {
        shard_manager.shutdown_all().await;
    }
    ExitCode::SUCCESS
}

/// Looks for a YouTube cookies file in `dir` (e.g. cookies.txt, youtube-cookies.txt,
/// www.youtube.com_cookies.txt, or any file containing "cookie").
pub fn find_cookies(dir: &str) -> Option<PathBuf> {
    for name in [
        "cookies.txt",
        "youtube-cookies.txt",
        "www.youtube.com_cookies.txt",
        "cookies",
    ] {
        let path = Path::new(dir).join(name);
        if path.is_file() && path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            return Some(path);
        }
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_lowercase();
                if name.contains("cookie") && !name.ends_with(".tmp") {
                    candidates.push(path);
                }
            }
        }
        candidates.sort();
        if let Some(path) = candidates.into_iter().next() {
            return Some(path);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_cookies_in_dir() {
        let dir = std::env::temp_dir().join(format!("musicart-cookie-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(find_cookies(dir.to_str().unwrap()), None);

        let tmp_cookie = dir.join("cookies.txt.tmp");
        std::fs::write(&tmp_cookie, "# partial write\n").unwrap();
        assert_eq!(find_cookies(dir.to_str().unwrap()), None);

        let cookie_file = dir.join("www.youtube.com_cookies.txt");
        std::fs::write(&cookie_file, "# Netscape HTTP Cookie File\n").unwrap();
        assert_eq!(find_cookies(dir.to_str().unwrap()), Some(cookie_file));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
