//! Per-server state and playback: who's in which voice channel, the queue and what's playing.

use crate::bot::Bot;
use crate::eq::Preset;
use crate::source::{self, Track};
use serenity::all::{ActivityData, ChannelId, GuildId, MessageId, UserId};
use serenity::async_trait;
use songbird::tracks::{PlayMode, TrackHandle};
use songbird::{Event, EventContext, TrackEvent};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Tracks kept for prev.
const HISTORY: usize = 20;
/// Most tracks a server's queue holds, so playlists can't fill the host's memory.
pub const QUEUE_MAX: usize = 1000;

pub struct Guild {
    /// everyone in a voice channel here, and whether they're a bot
    pub voice: HashMap<UserId, (ChannelId, bool)>,
    pub queue: VecDeque<Track>,
    pub current: Option<Current>,
    /// the next track is being looked up
    pub loading: bool,
    /// bumped by stop and disconnect, so a track still loading from before is dropped
    pub generation: u64,
    idle_since: Option<Instant>,
    /// 1.0 = as is, which lets songbird pass Opus through without re-encoding
    pub volume: f32,
    pub looping: Loop,
    pub eq: Preset,
    /// what played before, newest last
    pub history: VecDeque<Track>,
    /// who brought the bot into voice
    pub summoner: Option<UserId>,
    /// where play was last used, for track announcements
    pub announce_in: Option<ChannelId>,
    pub announcement: Option<(ChannelId, MessageId)>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Loop {
    Off,
    Track,
    Queue,
}

impl Default for Guild {
    fn default() -> Self {
        Self {
            voice: HashMap::new(),
            queue: VecDeque::new(),
            current: None,
            loading: false,
            generation: 0,
            idle_since: None,
            volume: 1.0,
            looping: Loop::Off,
            eq: Preset::Off,
            history: VecDeque::new(),
            summoner: None,
            announce_in: None,
            announcement: None,
        }
    }
}

pub struct Current {
    pub track: Track,
    pub handle: TrackHandle,
    pub paused: bool,
    pub started: Instant,
    pub at: Duration,
}

impl Current {
    pub fn position(&self, reported: Duration) -> Duration {
        let at = if reported >= self.at {
            reported
        } else if self.paused {
            self.at
        } else {
            self.at.saturating_add(self.started.elapsed())
        };
        match self.track.length() {
            Some(len) => at.min(len),
            None => at,
        }
    }
}

impl Guild {
    /// The voice channel someone is in.
    pub fn channel_of(&self, user: UserId) -> Option<ChannelId> {
        self.voice.get(&user).map(|&(channel, _)| channel)
    }

    /// People (not bots) in a voice channel.
    pub fn listeners(&self, channel: ChannelId) -> usize {
        self.voice
            .values()
            .filter(|&&(c, bot)| c == channel && !bot)
            .count()
    }

    pub fn remember(&mut self, track: Track) {
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(track);
    }
}

impl Bot {
    /// Adds the tracks that fit under [`QUEUE_MAX`] and starts playing if nothing is. Returns the
    /// first one's place in the queue, 0 = playing now, and how many were added.
    pub async fn enqueue(self: &Arc<Self>, guild: GuildId, tracks: Vec<Track>) -> (usize, usize) {
        let (place, added, start) = {
            let mut guilds = self.guilds.lock().unwrap();
            let g = guilds.entry(guild).or_default();
            let place = g.queue.len() + 1;
            let added = extend_capped(&mut g.queue, tracks, QUEUE_MAX);
            let start = added > 0 && g.current.is_none() && !g.loading;
            g.loading |= start;
            debug!(
                "{}: enqueued {added} track(s) in guild {guild} (queue total: {}, place: {place}, start: {start})",
                self.name,
                g.queue.len()
            );
            (if start { 0 } else { place }, added, start)
        };
        if start {
            self.play_next(guild, false).await;
        } else {
            self.preload(guild);
        }
        (place, added)
    }

    /// Plays the next track in the queue that works, if any, and announces it if asked and that's
    /// turned on. Only called with `loading` set.
    pub async fn play_next(self: &Arc<Self>, guild: GuildId, announce: bool) {
        while let Some((mut track, generation, eq)) =
            self.with_guild(guild, |g| Some((g.queue.pop_front()?, g.generation, g.eq)))
        {
            debug!(
                "{}: play_next in guild {guild}: {:?} (generation {generation}, eq: {eq:?})",
                self.name, track.title
            );
            let input = match track.input(&self.settings.finder, eq).await {
                Ok(input) => input,
                Err(e) => {
                    warn!("{}: skipping {}: {e}", self.name, track.title);
                    // stopped or disconnected meanwhile: nobody waits for this track any more, and
                    // what's queued now is newer
                    if self.with_guild(guild, |g| Some(g.generation == generation)) != Some(true) {
                        continue;
                    }
                    // the rest from YouTube would fail the same way, each with its own lookup and
                    // message; Spotify's own audio and SoundCloud still play
                    let cleared = if source::fails_all(&e) {
                        let finder = &self.settings.finder;
                        self.with_guild(guild, |g| {
                            let queued = g.queue.len();
                            g.queue.retain(|t| {
                                t.on_spotify(finder)
                                    || matches!(t.lookup(), source::Lookup::SoundCloud(_))
                            });
                            Some(queued - g.queue.len())
                        })
                        .unwrap_or(0)
                    } else {
                        0
                    };
                    if cleared > 0 {
                        info!(
                            "{}: cleared {cleared} queued tracks from YouTube",
                            self.name
                        );
                    }
                    self.dropped(guild, &track, &e, cleared);
                    continue;
                }
            };
            let Some(call) = self.songbird.get(guild) else {
                break;
            };
            let mut call = call.lock().await;
            let Some((current, volume)) =
                self.with_guild(guild, |g| Some((g.generation == generation, g.volume)))
            else {
                break;
            };
            // stopped or disconnected while this was loading: it's dropped, and whatever was queued
            // since is next, since nothing else would start it
            if !current {
                continue;
            }
            let handle = call.play_only(songbird::tracks::Track::new(input).volume(volume));
            // a failed track fires End too, so End covers both
            let _ = handle.add_event(
                Event::Track(TrackEvent::End),
                TrackEnd {
                    bot: self.clone(),
                    guild,
                },
            );
            let resumed = track.resume_at.take();
            let mut with = match eq {
                Preset::Off => String::new(),
                eq => format!(" with EQ {}", eq.describe()),
            };
            if track.plays_spotify() {
                with.push_str(" from Spotify");
            }
            let start_pos = resumed.unwrap_or_default();
            match resumed {
                Some(at) => {
                    let _ = handle.seek(at);
                    info!(
                        "{}: playing {} from {}{with}",
                        self.name,
                        track.title,
                        source::clock(at)
                    );
                }
                None => info!("{}: playing {}{with}", self.name, track.title),
            }
            self.with_guild(guild, |g| {
                g.current = Some(Current {
                    track,
                    handle,
                    paused: false,
                    started: Instant::now(),
                    at: start_pos,
                });
                Some(())
            });
            self.preload(guild);
            if announce && resumed.is_none() && self.settings.announce_tracks {
                self.announce(guild);
            }
            break;
        }
        self.with_guild(guild, |g| {
            g.loading = false;
            Some(())
        });
        self.refresh(guild);
    }

    /// Looks up the next track's stream now, so it starts without a gap.
    fn preload(self: &Arc<Self>, guild: GuildId) {
        let finder = &self.settings.finder;
        let next = self.with_guild(guild, |g| {
            g.queue
                .front()
                .filter(|t| !t.ready() && !t.on_spotify(finder))
                .map(|t| (t.id, t.lookup(), t.title.clone()))
        });
        let Some((id, lookup, title)) = next else {
            return;
        };
        debug!(
            "{}: preloading next track {:?} in guild {guild}",
            self.name, title
        );
        let bot = self.clone();
        tokio::spawn(async move {
            match source::stream(&bot.settings.finder, &lookup, &|_| {}).await {
                Ok((page, stream)) => {
                    bot.with_guild(guild, |g| {
                        g.queue
                            .iter_mut()
                            .find(|t| t.id == id)?
                            .set_stream(page, stream);
                        Some(())
                    });
                }
                Err(e) => warn!("{}: looking up {title} ahead: {e}", bot.name),
            }
        });
    }

    /// Clears the queue and stops playing, but stays in voice.
    pub fn stop(self: &Arc<Self>, guild: GuildId) -> bool {
        let stopped = self.with_guild(guild, |g| {
            g.queue.clear();
            g.generation += 1;
            // taken first, so its End event finds nothing to move on from
            let current = g.current.take()?;
            let _ = current.handle.stop();
            Some(())
        });
        self.refresh(guild);
        stopped.is_some()
    }

    /// Plays the current track again from where it is, to apply a new EQ.
    pub async fn restart(self: &Arc<Self>, guild: GuildId) {
        let Some(handle) = self.with_guild(guild, |g| Some(g.current.as_ref()?.handle.clone()))
        else {
            return;
        };
        let reported = handle
            .get_info()
            .await
            .map(|i| i.position)
            .unwrap_or_default();
        let restart = self.with_guild(guild, |g| {
            if g.loading {
                return None;
            }
            let at = g.current.as_ref()?.position(reported);
            // taken first, so its End event finds nothing to move on from
            let mut current = g.current.take_if(|c| c.handle.uuid() == handle.uuid())?;
            current.track.resume_at = Some(at);
            g.queue.push_front(current.track);
            g.loading = true;
            Some(current.paused)
        });
        let Some(paused) = restart else { return };
        let _ = handle.stop();
        self.play_next(guild, false).await;
        if paused {
            self.with_guild(guild, |g| {
                let current = g.current.as_mut()?;
                current.paused = true;
                current.handle.pause().ok()
            });
            self.refresh(guild);
        }
    }

    /// Clears the queue and leaves voice.
    pub async fn leave(self: &Arc<Self>, guild: GuildId) {
        debug!("{}: leaving voice channel in guild {guild}", self.name);
        self.with_guild(guild, |g| {
            g.queue.clear();
            g.history.clear();
            g.summoner = None;
            g.current = None;
            g.generation += 1;
            g.idle_since = None;
            g.looping = Loop::Off;
            Some(())
        });
        if let Err(e) = self.songbird.remove(guild).await {
            warn!("{}: leaving voice: {e}", self.name);
        }
        self.update_status();
    }

    /// Runs `f` on a server's state, if there is any.
    pub fn with_guild<T>(
        &self,
        guild: GuildId,
        f: impl FnOnce(&mut Guild) -> Option<T>,
    ) -> Option<T> {
        self.guilds.lock().unwrap().get_mut(&guild).and_then(f)
    }

    /// Starts or stops the idle timer and updates the status. Call after anything that changes them.
    pub fn refresh(self: &Arc<Self>, guild: GuildId) {
        let started = self.with_guild(guild, |g| {
            let me = self.user.get().and_then(|&me| g.channel_of(me));
            let active = g.current.as_ref().is_some_and(|c| !c.paused)
                && me.is_some_and(|c| g.listeners(c) > 0);
            if active || me.is_none() {
                g.idle_since = None;
                None
            } else if g.idle_since.is_none() {
                g.idle_since = Some(Instant::now());
                g.idle_since
            } else {
                None
            }
        });
        let timeout = self.settings.idle_timeout;
        if let Some(since) = started.filter(|_| !timeout.is_zero()) {
            let bot = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                if bot.with_guild(guild, |g| g.idle_since) == Some(since) {
                    info!("{}: left voice after {}s idle", bot.name, timeout.as_secs());
                    bot.leave(guild).await;
                }
            });
        }
        self.update_status();
    }

    /// "Listening to <track>" with the newest track playing in any server, if that's turned on.
    fn update_status(&self) {
        if !self.settings.track_status {
            return;
        }
        let Some(shard) = self.shard.get() else {
            return;
        };
        let title = {
            let guilds = self.guilds.lock().unwrap();
            let playing = guilds.values().filter_map(|g| g.current.as_ref());
            playing
                .max_by_key(|c| c.started)
                .map(|c| c.track.title.chars().take(128).collect::<String>())
        };
        let mut status = self.status.lock().unwrap();
        if *status != title {
            shard.set_activity(title.as_deref().map(ActivityData::listening));
            *status = title;
        }
    }
}

/// Moves on when a track ends or fails: loops it, retries it once, or plays the next one.
struct TrackEnd {
    bot: Arc<Bot>,
    guild: GuildId,
}

#[async_trait]
impl songbird::EventHandler for TrackEnd {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let EventContext::Track(tracks) = ctx else {
            return None;
        };
        for (state, handle) in *tracks {
            let name = &self.bot.name;
            let mut dropped = None;
            // only the track that's playing now: ignore ones replaced or cleared on purpose
            let next = self.bot.with_guild(self.guild, |g| {
                let Current { mut track, .. } =
                    g.current.take_if(|c| c.handle.uuid() == handle.uuid())?;
                debug!(
                    "{name}: track end event for {:?} in guild {} (state: {:?}, pos: {:?})",
                    track.title, self.guild, state.playing, state.position
                );
                let failure = match &state.playing {
                    PlayMode::Errored(e) => Some(e.to_string()),
                    // songbird ends a track whose stream dies instead of failing it
                    PlayMode::End if track.cut_short(state.position) => Some(format!(
                        "its stream stopped at {}",
                        source::clock(state.position)
                    )),
                    _ => None,
                };
                match failure {
                    Some(e) if track.retried => {
                        warn!("{name}: {} failed again, skipping: {e}", track.title);
                        dropped = Some((track, e));
                    }
                    Some(e) => {
                        // YouTube sometimes refuses a stream link; a fresh one usually works
                        warn!("{name}: {} failed, retrying: {e}", track.title);
                        track.retried = true;
                        track.resume_at = Some(state.position).filter(|p| !p.is_zero());
                        track.forget_stream(&self.bot.settings.finder);
                        g.queue.push_front(track);
                        // the same track again needs no announcement
                        g.loading = true;
                        return Some(false);
                    }
                    None => {
                        track.retried = false;
                        match g.looping {
                            Loop::Track => {
                                g.queue.push_front(track);
                                // the same track again needs no announcement
                                g.loading = true;
                                return Some(false);
                            }
                            Loop::Queue => {
                                g.remember(track.clone());
                                g.queue.push_back(track);
                            }
                            _ => g.remember(track),
                        }
                    }
                }
                g.loading = true;
                Some(true)
            });
            if let Some((track, e)) = dropped {
                self.bot.dropped(self.guild, &track, &e, 0);
            }
            if let Some(announce) = next {
                let bot = self.bot.clone();
                let guild = self.guild;
                tokio::spawn(async move { bot.play_next(guild, announce).await });
            }
        }
        None
    }
}

/// Appends as many of `items` as fit under `max` and returns how many that was.
fn extend_capped<T>(queue: &mut VecDeque<T>, items: Vec<T>, max: usize) -> usize {
    let added = items.len().min(max.saturating_sub(queue.len()));
    queue.extend(items.into_iter().take(added));
    added
}

/// Fisher-Yates with xorshift, seeded by std's random hasher keys (no rand crate needed).
pub fn shuffle<T>(items: &mut VecDeque<T>) {
    use std::hash::BuildHasher;
    let mut seed = std::collections::hash_map::RandomState::new().hash_one(0u8) | 1;
    for i in (1..items.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        items.swap(i, (seed % (i as u64 + 1)) as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_takes_what_fits() {
        let mut queue: VecDeque<u32> = (0..8).collect();
        assert_eq!(extend_capped(&mut queue, vec![8, 9, 10, 11], 10), 2);
        assert_eq!(queue.len(), 10);
        assert_eq!(queue.back(), Some(&9));
        // full: nothing more, and no underflow when it somehow holds more than the cap
        assert_eq!(extend_capped(&mut queue, vec![1], 10), 0);
        assert_eq!(extend_capped(&mut queue, vec![1], 5), 0);
        assert_eq!(queue.len(), 10);
        let mut empty = VecDeque::new();
        assert_eq!(extend_capped(&mut empty, vec![1, 2, 3], 10), 3);
    }
}
