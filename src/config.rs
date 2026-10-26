//! Settings from the TOML config file. Anything left out uses its default.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub log_level: LogLevel,
    /// optional togematch-api on the local network for Spotify links and searches
    pub togematch_api: String,
    /// Spotify's own audio for Spotify links, on a Premium account; signing in is a link in the log
    pub spotify: bool,
    /// seconds without playing to anyone before leaving voice, 0 = never
    pub idle_timeout: u64,
    /// show the playing track as the bot's status
    pub track_status: bool,
    /// post "Now playing" when a track starts by itself
    pub announce_tracks: bool,
    /// roles that always control playback; none = everyone does
    pub dj_role_ids: Vec<u64>,
    /// STUN server ("host:port") to find the public address with, for networks that block
    /// Discord's own address discovery; empty = Discord's discovery
    pub stun: String,
    pub bot: BTreeMap<String, Bot>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bot {
    pub token: String,
    /// for text commands like "d.play"; none = slash commands only
    #[serde(default)]
    pub prefix: String,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Info,
            togematch_api: String::new(),
            spotify: false,
            idle_timeout: 900,
            track_status: true,
            announce_tracks: false,
            dj_role_ids: Vec::new(),
            stun: String::new(),
            bot: BTreeMap::new(),
        }
    }
}

/// Reads the file named by the first argument or MUSICART_CONFIG, else musicart.toml.
pub fn load() -> Result<Config, String> {
    let path: PathBuf = std::env::args_os()
        .nth(1)
        .or_else(|| std::env::var_os("MUSICART_CONFIG"))
        .unwrap_or_else(|| "musicart.toml".into())
        .into();
    let error = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let text = std::fs::read_to_string(&path).map_err(|e| error(&e))?;
    let config: Config = toml::from_str(&text).map_err(|e| error(&e))?;
    if config.bot.is_empty() {
        return Err(error(&"no bots, add a [bot.<name>] section with a token"));
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    #[test]
    fn example_config_is_valid() {
        let config: super::Config = toml::from_str(include_str!("../example.toml")).unwrap();
        assert_eq!(config.bot.len(), 2);
    }
}
