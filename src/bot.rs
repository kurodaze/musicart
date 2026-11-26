//! One Discord bot: its gateway connection, voice manager, and the commands, as slash or prefix commands.

use crate::config;
use crate::eq::{self, Preset};
use crate::player::{self, Current, Guild, Loop};
use crate::source::{self, Found, clock};
use serenity::all::{
    ButtonStyle, ChannelId, Client, Command, CommandDataOptionValue, CommandInteraction,
    CommandOptionType, ComponentInteraction, Context, CreateActionRow, CreateAllowedMentions,
    CreateButton, CreateCommand, CreateCommandOption, CreateEmbed, CreateEmbedFooter,
    CreateInteractionResponse, CreateInteractionResponseFollowup, CreateInteractionResponseMessage,
    CreateMessage, EditInteractionResponse, EditMessage, EventHandler, GatewayIntents, GuildId,
    Http, Interaction, Message, MessageId, Ready, RoleId, ShardManager, ShardMessenger,
    UnavailableGuild, UserId, VoiceState,
};
use serenity::async_trait;
use songbird::driver::Bitrate;
use songbird::{SerenityInit, Songbird};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

/// Only used when songbird has to re-encode (Spotify, volume, EQ): above YouTube's ~130 kbps, so it adds no audible loss.
const REENCODE_BITRATE: i32 = 144_000;

/// Shared by all bots.
pub struct Settings {
    pub finder: source::Finder,
    pub idle_timeout: Duration,
    pub track_status: bool,
    pub announce_tracks: bool,
    /// empty = everyone controls playback
    pub dj_roles: Vec<RoleId>,
    pub stun: Option<String>,
}

pub struct Bot {
    pub name: String,
    prefix: String,
    pub settings: Arc<Settings>,
    pub songbird: Arc<Songbird>,
    pub guilds: Mutex<HashMap<GuildId, Guild>>,
    pub user: OnceLock<UserId>,
    pub shard: OnceLock<ShardMessenger>,
    http: OnceLock<Arc<Http>>,
    pub status: Mutex<Option<String>>,
    shards: OnceLock<Arc<ShardManager>>,
}

struct Handler(Arc<Bot>);

/// A command and where it came from.
struct Request {
    guild: GuildId,
    channel: ChannelId,
    user: UserId,
    roles: Vec<RoleId>,
    args: String,
}

const ALIASES: &[(&str, &str)] = &[
    ("p", "play"),
    ("s", "skip"),
    ("rewind", "replay"),
    ("q", "queue"),
    ("np", "nowplaying"),
    ("repeat", "loop"),
    ("die", "disconnect"),
    ("leave", "disconnect"),
];
/// Open to everyone when DJ roles are set.
const OPEN: &[&str] = &["play", "queue", "nowplaying", "help", "ping"];
/// Every command name, verified against [`commands`] by `command_lists_match`.
const COMMANDS: &[&str] = &[
    "play",
    "skip",
    "prev",
    "replay",
    "queue",
    "nowplaying",
    "pause",
    "resume",
    "stop",
    "clear",
    "remove",
    "shuffle",
    "loop",
    "seek",
    "volume",
    "eq",
    "disconnect",
    "help",
    "ping",
];

fn commands() -> Vec<CreateCommand> {
    let command = |name: &str, description: &str| CreateCommand::new(name).description(description);
    let option =
        |kind, name: &str, description: &str| CreateCommandOption::new(kind, name, description);
    vec![
        command("play", "Play a track or playlist, or add it to the queue").add_option(
            option(CommandOptionType::String, "track", "Link or search").required(true),
        ),
        command("skip", "Skip the current track"),
        command("prev", "Play the previous track again"),
        command(
            "replay",
            "Start this track over, or play the last one again",
        ),
        command("queue", "Show what's playing and what's next"),
        command("nowplaying", "Show the current track"),
        command("pause", "Pause"),
        command("resume", "Resume"),
        command("stop", "Stop and clear the queue, but stay in voice"),
        command("clear", "Clear the queue but keep playing"),
        command("remove", "Remove a track from the queue").add_option(
            option(
                CommandOptionType::Integer,
                "number",
                "Its number in the queue",
            )
            .required(true)
            .min_int_value(1),
        ),
        command("shuffle", "Shuffle the queue"),
        command("loop", "Loop this track or the queue").add_option(
            option(
                CommandOptionType::String,
                "mode",
                "Leave out to switch to the next mode",
            )
            .add_string_choice("off", "off")
            .add_string_choice("this track", "track")
            .add_string_choice("the queue", "queue"),
        ),
        command("seek", "Jump to a time in the current track").add_option(
            option(
                CommandOptionType::String,
                "time",
                "Like 1:30, 90, +30 or -10",
            )
            .required(true),
        ),
        command("volume", "Show or set the volume").add_option(
            option(
                CommandOptionType::Integer,
                "percent",
                "0 to 200, 100 is as is",
            )
            .min_int_value(0)
            .max_int_value(200),
        ),
        command("eq", "Show or set the EQ")
            .add_option(Preset::ALL.into_iter().fold(
                option(
                    CommandOptionType::String,
                    "preset",
                    "Leave out to see the current one",
                ),
                |option, p| option.add_string_choice(p.name(), p.name()),
            ))
            .add_option(option(
                CommandOptionType::String,
                "bands",
                "Your own, in dB for bass, low, mid, high and treble, like 6 2 0 -1 3",
            )),
        command("disconnect", "Clear the queue and leave voice"),
        command("help", "List the commands"),
        command("ping", "Check that the bot is alive"),
    ]
}

#[allow(clippy::result_large_err)] // serenity's error type
pub async fn client(
    name: &str,
    config: &config::Bot,
    settings: Arc<Settings>,
) -> serenity::Result<Client> {
    let mut intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
    if !config.prefix.is_empty() {
        intents |= GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    }
    let songbird = Songbird::serenity_from_config(
        songbird::Config::default().stun_server(settings.stun.clone()),
    );
    let bot = Arc::new(Bot {
        name: name.into(),
        prefix: config.prefix.clone(),
        settings,
        songbird: songbird.clone(),
        guilds: Mutex::default(),
        user: OnceLock::new(),
        shard: OnceLock::new(),
        http: OnceLock::new(),
        status: Mutex::default(),
        shards: OnceLock::new(),
    });
    let client = Client::builder(&config.token, intents)
        .event_handler(Handler(bot.clone()))
        .register_songbird_with(songbird)
        .await?;
    let _ = bot.shards.set(client.shard_manager.clone());
    Ok(client)
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        let bot = &self.0;
        info!(
            "{}: logged in as {}, in {} servers",
            bot.name,
            ready.user.tag(),
            ready.guilds.len()
        );
        let _ = bot.user.set(ready.user.id);
        let _ = bot.shard.set(ctx.shard.clone());
        let _ = bot.http.set(ctx.http.clone());
        if let Err(e) = Command::set_global_commands(&ctx.http, commands()).await {
            error!("{}: couldn't register commands: {e}", bot.name);
        }
    }

    async fn guild_create(&self, _: Context, guild: serenity::all::Guild, _: Option<bool>) {
        let is_bot = |user| guild.members.get(user).is_some_and(|m| m.user.bot);
        let voice = guild
            .voice_states
            .iter()
            .filter_map(|(user, v)| Some((*user, (v.channel_id?, is_bot(user)))));
        self.0
            .guilds
            .lock()
            .unwrap()
            .entry(guild.id)
            .or_default()
            .voice = voice.collect();
    }

    async fn guild_delete(
        &self,
        _: Context,
        guild: UnavailableGuild,
        _: Option<serenity::all::Guild>,
    ) {
        if !guild.unavailable {
            self.0.guilds.lock().unwrap().remove(&guild.id);
        }
    }

    async fn voice_state_update(&self, _: Context, _: Option<VoiceState>, new: VoiceState) {
        let bot = &self.0;
        let Some(guild) = new.guild_id else { return };
        let is_bot = new.member.as_ref().is_some_and(|m| m.user.bot);
        let gone = bot.with_guild(guild, |g| {
            match new.channel_id {
                Some(channel) => g.voice.insert(new.user_id, (channel, is_bot)),
                None => g.voice.remove(&new.user_id),
            };
            Some(new.channel_id.is_none() && bot.user.get() == Some(&new.user_id))
        });
        // kicked from voice: drop the queue too
        if gone == Some(true) && bot.songbird.get(guild).is_some() {
            bot.leave(guild).await;
        } else {
            bot.refresh(guild);
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(cmd) => {
                let Some(guild) = cmd.guild_id else { return };
                let name = cmd.data.name.clone();
                let args: Vec<String> = cmd
                    .data
                    .options
                    .iter()
                    .filter_map(|o| match &o.value {
                        CommandDataOptionValue::String(s) => Some(s.clone()),
                        CommandDataOptionValue::Integer(i) => Some(i.to_string()),
                        _ => None,
                    })
                    .collect();
                let args = args.join(" ");
                let request = Request {
                    guild,
                    channel: cmd.channel_id,
                    user: cmd.user.id,
                    roles: cmd
                        .member
                        .as_ref()
                        .map(|m| m.roles.clone())
                        .unwrap_or_default(),
                    args,
                };
                let reply = Reply::Slash {
                    http: ctx.http.clone(),
                    cmd: Box::new(cmd),
                    sent: false,
                };
                self.0.answer(&ctx, &name, request, reply).await;
            }
            Interaction::Component(component) => {
                self.0.handle_component(&ctx, component).await;
            }
            _ => {}
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        let bot = &self.0;
        let (Some(guild), false) = (msg.guild_id, msg.author.bot) else {
            return;
        };
        // case-insensitive, since phones like to capitalize "T.p"
        let n = bot.prefix.len();
        if n == 0
            || !msg
                .content
                .get(..n)
                .is_some_and(|p| p.eq_ignore_ascii_case(&bot.prefix))
        {
            return;
        }
        let rest = msg.content[n..].trim_start();
        let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let name = name.to_lowercase();
        let name = ALIASES
            .iter()
            .find(|(alias, _)| *alias == name)
            .map_or(name.as_str(), |(_, n)| n)
            .to_string();
        let request = Request {
            guild,
            channel: msg.channel_id,
            user: msg.author.id,
            roles: msg
                .member
                .as_ref()
                .map(|m| m.roles.clone())
                .unwrap_or_default(),
            args: args.trim().into(),
        };
        let reply = Reply::Text {
            http: ctx.http.clone(),
            channel: msg.channel_id,
            to: msg.id,
            sent: None,
        };
        bot.answer(&ctx, &name, request, reply).await;
    }
}

/// A command's answer. A slash command shows a private one only to whoever used it: mistakes,
/// refusals, and looking at things, which change nothing for anyone else.
enum Said {
    Public(String),
    Private(String),
    Temporary(String, Duration),
    Queue(Box<CreateEmbed>, Option<CreateActionRow>),
}

fn private(text: impl Into<String>) -> Said {
    Said::Private(text.into())
}

impl From<String> for Said {
    fn from(text: String) -> Said {
        Said::Public(text)
    }
}

impl From<&str> for Said {
    fn from(text: &str) -> Said {
        Said::Public(text.into())
    }
}

/// Where a command's answer goes: a reply to its message, or the slash command's response.
enum Reply {
    Text {
        http: Arc<Http>,
        channel: ChannelId,
        to: MessageId,
        sent: Option<MessageId>,
    },
    Slash {
        http: Arc<Http>,
        cmd: Box<CommandInteraction>,
        sent: bool,
    },
}

impl Reply {
    /// Shows that a slow command is working: its status for a text command, and Discord's own
    /// "thinking..." for a slash command, whose status isn't shown since text would end that.
    #[allow(clippy::result_large_err)] // serenity's error type
    async fn working(&mut self, status: String) -> serenity::Result<()> {
        match self {
            Reply::Slash { http, cmd, sent } => {
                cmd.defer(&**http).await?;
                *sent = true;
                Ok(())
            }
            Reply::Text { .. } => self.show(status.into()).await,
        }
    }

    /// Sends the answer, or changes it once it's sent. Text replies are always public.
    #[allow(clippy::result_large_err)] // serenity's error type
    async fn show(&mut self, said: Said) -> serenity::Result<()> {
        let none = CreateAllowedMentions::new;
        let (text, embed, row, private, delete_after) = match said {
            Said::Public(text) => (Some(text), None, None, false, None),
            Said::Private(text) => (Some(text), None, None, true, None),
            Said::Temporary(text, duration) => (Some(text), None, None, true, Some(duration)),
            Said::Queue(embed, row) => (None, Some(*embed), row, true, None),
        };
        let has_buttons = row.is_some();
        let rows = row.into_iter().collect::<Vec<_>>();
        match self {
            Reply::Text {
                http,
                channel,
                to,
                sent,
            } => {
                let message_id = match *sent {
                    Some(message) => {
                        let mut edit = EditMessage::new()
                            .content(text.unwrap_or_default())
                            .allowed_mentions(none());
                        if let Some(e) = embed {
                            edit = edit.embed(e).components(rows);
                        }
                        channel.edit_message(&**http, message, edit).await?;
                        message
                    }
                    None => {
                        let mut reply = CreateMessage::new()
                            .reference_message((*channel, *to))
                            .allowed_mentions(none());
                        if let Some(t) = text {
                            reply = reply.content(t);
                        }
                        if let Some(e) = embed {
                            reply = reply.embed(e).components(rows);
                        }
                        let message_id = channel.send_message(&**http, reply).await?.id;
                        *sent = Some(message_id);
                        message_id
                    }
                };
                if has_buttons {
                    let (http, channel) = (http.clone(), *channel);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(120)).await;
                        let _ = channel
                            .edit_message(&http, message_id, EditMessage::new().components(vec![]))
                            .await;
                    });
                }
                if let Some(duration) = delete_after {
                    let (http, channel) = (http.clone(), *channel);
                    tokio::spawn(async move {
                        tokio::time::sleep(duration).await;
                        let _ = channel.delete_message(&http, message_id).await;
                    });
                }
                Ok(())
            }
            // "thinking..." was public, so a private answer takes its place
            Reply::Slash {
                http,
                cmd,
                sent: true,
            } if private => {
                cmd.delete_response(&**http).await?;
                let mut message = CreateInteractionResponseFollowup::new()
                    .ephemeral(true)
                    .allowed_mentions(none());
                if let Some(t) = text {
                    message = message.content(t);
                }
                if let Some(e) = embed {
                    message = message.embed(e).components(rows);
                }
                cmd.create_followup(&**http, message).await.map(drop)
            }
            Reply::Slash {
                http,
                cmd,
                sent: true,
            } => {
                let mut edit = EditInteractionResponse::new()
                    .content(text.unwrap_or_default())
                    .allowed_mentions(none());
                if let Some(e) = embed {
                    edit = edit.embed(e).components(rows);
                }
                cmd.edit_response(&**http, edit).await.map(drop)
            }
            Reply::Slash { http, cmd, sent } => {
                let mut message = CreateInteractionResponseMessage::new()
                    .ephemeral(private)
                    .allowed_mentions(none());
                if let Some(t) = text {
                    message = message.content(t);
                }
                if let Some(e) = embed {
                    message = message.embed(e).components(rows);
                }
                cmd.create_response(&**http, CreateInteractionResponse::Message(message))
                    .await?;
                *sent = true;
                Ok(())
            }
        }
    }
}

impl Bot {
    /// Runs a command and answers it. A slow one answers at once: a text command with what it's
    /// doing, changed as it goes, and a slash command with Discord's "thinking...".
    async fn answer(self: &Arc<Self>, ctx: &Context, name: &str, req: Request, mut reply: Reply) {
        if !COMMANDS.contains(&name) {
            return;
        }
        if let Err(why) = self.allowed(name, &req) {
            debug!(
                "{}: command {name} not allowed for {}: {why}",
                self.name, req.user
            );
            if let Err(e) = reply.show(private(why)).await {
                error!("{}: answering {name}: {e}", self.name);
            }
            return;
        }
        let first = first_status(name, &req.args).or_else(|| {
            (name == "skip" && self.skip_needs_lookup(req.guild)).then(|| "Skipping...".into())
        });
        let (status, mut changes) = watch::channel(first.clone().unwrap_or_default());
        let text = matches!(reply, Reply::Text { .. });
        if let Some(first) = first
            && let Err(e) = reply.working(first).await
        {
            error!("{}: answering {name}: {e}", self.name);
        }
        let work = self.run(ctx, name, req, &status);
        tokio::pin!(work);
        let said = loop {
            tokio::select! {
                said = &mut work => break said,
                Ok(()) = changes.changed() => {
                    let now = changes.borrow_and_update().clone();
                    if text && let Err(e) = reply.show(now.into()).await {
                        warn!("{}: updating the answer to {name}: {e}", self.name);
                    }
                }
            }
        };
        if let Some(said) = said
            && let Err(e) = reply.show(said).await
        {
            error!("{}: answering {name}: {e}", self.name);
        }
    }

    /// The reply to a command, or None if there's no such command.
    async fn run(
        self: &Arc<Self>,
        ctx: &Context,
        name: &str,
        req: Request,
        status: &watch::Sender<String>,
    ) -> Option<Said> {
        debug!(
            "{}: running command {name} (args: {:?}) for {}",
            self.name, req.args, req.user
        );
        Some(match name {
            "play" => self.play(req, status).await,
            "skip" => self.skip(req.guild).await,
            "prev" => self.prev(req.guild, "Back to").await,
            "replay" => self.replay(req.guild).await,
            "clear" => self.clear(req.guild),
            "eq" => self.eq(req).await,
            "help" => self.help(ctx, &req).await,
            "queue" => self.queue(req.guild).await,
            "nowplaying" => private(
                self.now_playing(req.guild)
                    .await
                    .unwrap_or_else(|| "Nothing is playing.".into()),
            ),
            "pause" => self.pause(req.guild, true),
            "resume" => self.pause(req.guild, false),
            "stop" => match self.stop(req.guild) {
                true => "Stopped and cleared the queue.".into(),
                false => private("Nothing is playing."),
            },
            "remove" => self.remove(req),
            "shuffle" => self.shuffle(req.guild),
            "loop" => self.set_loop(req),
            "seek" => self.seek(req).await,
            "volume" => self.volume(req),
            "disconnect" => {
                self.leave(req.guild).await;
                "Bye!".into()
            }
            "ping" => private(self.ping(ctx).await),
            _ => return None,
        })
    }

    async fn play(self: &Arc<Self>, req: Request, status: &watch::Sender<String>) -> Said {
        if req.args.is_empty() {
            return private("Play what? Give me a link or something to search for.");
        }
        let (Some(channel), mine) = self
            .with_guild(req.guild, |g| {
                let mine = self.user.get().and_then(|&me| g.channel_of(me));
                Some((g.channel_of(req.user), mine))
            })
            .unwrap_or_default()
        else {
            return private("Join a voice channel first.");
        };
        // busy: playing to people in another channel
        let busy = self.with_guild(req.guild, |g| {
            let mine = mine.filter(|&c| c != channel)?;
            ((g.current.is_some() || g.loading) && g.listeners(mine) > 0).then_some(mine)
        });
        if let Some(mine) = busy {
            return private(format!(
                "I'm busy in <#{mine}>. Join there, or ask another bot."
            ));
        }
        let idle = self
            .with_guild(req.guild, |g| Some(g.current.is_none() && !g.loading))
            .unwrap_or(true);
        let set = |text: &'static str| {
            status.send_replace(text.into());
        };
        let finder = &self.settings.finder;
        // joins while looking up, and when nothing plays, the first track's audio too
        let lookup = async {
            let mut found = source::find(finder, &req.args).await?;
            let single = found.playlist.is_none();
            if let Some(first) = found.tracks.first_mut() {
                if idle {
                    let ready = first.prepare(finder, &set).await;
                    if single {
                        ready?;
                    }
                } else if single {
                    // queued: its YouTube page now, for the reply's link, and so a track that
                    // isn't there is said now rather than when its turn comes
                    first.locate(finder, &set).await?;
                }
            }
            Ok::<_, String>(found)
        };
        let (joined, found) = tokio::join!(self.join(req.guild, channel), lookup);
        let Found {
            playlist,
            mut tracks,
        } = match found {
            Ok(found) => found,
            Err(e) => {
                // came in just for this
                let unused = self
                    .with_guild(req.guild, |g| {
                        Some(g.current.is_none() && g.queue.is_empty() && !g.loading)
                    })
                    .unwrap_or(true);
                if joined.is_ok() && mine != Some(channel) && unused {
                    self.leave(req.guild).await;
                }
                return private(format!("Couldn't play that: {e}"));
            }
        };
        for track in &mut tracks {
            track.requester = Some(req.user);
        }
        let single = playlist.is_none();
        let total = tracks.len();
        let first = describe(&tracks[0]);
        if let Err(e) = joined {
            warn!("{}: joining voice: {e}", self.name);
            return private("Couldn't join your voice channel.");
        }
        self.with_guild(req.guild, |g| {
            if mine != Some(channel) {
                g.summoner = Some(req.user);
            }
            g.announce_in = Some(req.channel);
            Some(())
        });
        if idle {
            set("Starting...");
        }
        let (place, added) = self.enqueue(req.guild, tracks).await;
        if added == 0 {
            return private(format!("The queue is full, {} tracks.", player::QUEUE_MAX));
        }
        let what = match playlist {
            Some(ref pl) if added < total => {
                format!(
                    "{added} of {total} tracks from {} (the queue is full)",
                    playlist_link(pl)
                )
            }
            Some(ref pl) => format!("{total} tracks from {}", playlist_link(pl)),
            None => first,
        };
        match place {
            0 => {
                // answered once the audio is going
                let handle =
                    self.with_guild(req.guild, |g| Some(g.current.as_ref()?.handle.clone()));
                if let Some(handle) = handle {
                    let _ =
                        tokio::time::timeout(Duration::from_secs(15), handle.make_playable_async())
                            .await;
                }
                // what started, now that a Spotify track has its YouTube page
                let now = self
                    .with_guild(req.guild, |g| Some(describe(&g.current.as_ref()?.track)))
                    .filter(|_| single);
                format!("Playing {}", now.unwrap_or(what)).into()
            }
            n => format!("Queued {what}, #{n} in line").into(),
        }
    }

    /// Joins a voice channel unless already there.
    async fn join(
        &self,
        guild: GuildId,
        channel: ChannelId,
    ) -> Result<(), songbird::error::JoinError> {
        if let Some(call) = self.songbird.get(guild)
            && call.lock().await.current_channel() == Some(channel.into())
        {
            return Ok(());
        }
        debug!(
            "{}: joining voice channel {channel} in guild {guild}",
            self.name
        );
        let call = self.songbird.join(guild, channel).await?;
        let mut call = call.lock().await;
        call.set_bitrate(Bitrate::Bits(REENCODE_BITRATE));
        // nobody's listening to what others say, so don't make Discord send it
        call.deafen(true).await
    }

    fn skip_needs_lookup(&self, guild: GuildId) -> bool {
        let finder = &self.settings.finder;
        self.with_guild(guild, |g| {
            g.current.as_ref()?;
            let next = g.queue.front()?;
            Some(!next.ready() && !next.on_spotify(finder))
        })
        .unwrap_or(false)
    }

    async fn skip(self: &Arc<Self>, guild: GuildId) -> Said {
        let skipped = self.with_guild(guild, |g| {
            if g.loading {
                return Some(Err("Still loading a track, try again in a moment."));
            }
            // taken first, so its End event finds nothing to move on from
            let Current {
                mut track, handle, ..
            } = g.current.take()?;
            let _ = handle.stop();
            let what = describe(&track);
            track.retried = false;
            match g.looping {
                Loop::Queue => {
                    g.remember(track.clone());
                    g.queue.push_back(track);
                }
                _ => g.remember(track),
            }
            let advance = !g.queue.is_empty();
            if advance {
                g.loading = true;
            }
            let old_announcement = g.announcement.take();
            Some(Ok((what, advance, old_announcement)))
        });
        let (what, advance, old_announcement) = match skipped {
            Some(Ok(v)) => v,
            Some(Err(why)) => return private(why),
            None => return private("Nothing is playing."),
        };
        if let Some((channel, message)) = old_announcement
            && let Some(http) = self.http.get().cloned()
        {
            tokio::spawn(async move {
                let _ = channel.delete_message(&http, message).await;
            });
        }
        if !advance {
            self.refresh(guild);
            return format!("Skipped {what}").into();
        }
        self.play_next(guild, false).await;
        let next = self.with_guild(guild, |g| Some(describe(&g.current.as_ref()?.track)));
        match next {
            Some(next) => format!("Skipped {what}\nNow playing {next}").into(),
            None => format!("Skipped {what}").into(),
        }
    }

    async fn now_playing(&self, guild: GuildId) -> Option<String> {
        let (handle, title, length, paused) = self.with_guild(guild, |g| {
            let c = g.current.as_ref()?;
            Some((c.handle.clone(), link(&c.track), c.track.length(), c.paused))
        })?;
        let reported = handle
            .get_info()
            .await
            .map(|info| info.position)
            .unwrap_or_default();
        let position = self
            .with_guild(guild, |g| Some(g.current.as_ref()?.position(reported)))
            .unwrap_or(reported);
        let position = clock(position);
        let length = length
            .map(|d| format!(" / {}", clock(d)))
            .unwrap_or_default();
        let state = if paused { "Paused at" } else { "Now playing" };
        Some(format!("{state} {title} ({position}{length})"))
    }

    async fn queue_embed(
        &self,
        guild: GuildId,
        page: usize,
    ) -> Option<(CreateEmbed, Option<CreateActionRow>)> {
        let mut desc = self.now_playing(guild).await?;
        let (total, page, pages, tracks) = self.with_guild(guild, |g| {
            match g.looping {
                Loop::Track => desc.push_str(", looping it"),
                Loop::Queue => desc.push_str(", looping the queue"),
                Loop::Off => {}
            }
            let total = g.queue.len();
            let pages = total.div_ceil(10).max(1);
            let page = page.clamp(1, pages);
            let start = (page - 1) * 10;
            let list = g
                .queue
                .iter()
                .skip(start)
                .take(10)
                .enumerate()
                .map(|(i, t)| format!("{}. {}", start + i + 1, describe(t)))
                .collect::<Vec<_>>();
            Some((total, page, pages, list))
        })?;
        if tracks.is_empty() {
            desc.push_str("\n\nNothing up next.");
        } else {
            desc.push_str("\n\n");
            desc.push_str(&tracks.join("\n"));
        }
        let embed =
            CreateEmbed::new()
                .title("Queue")
                .description(desc)
                .footer(CreateEmbedFooter::new(format!(
                    "Page {page} of {pages} • {total} tracks"
                )));
        let row = (pages > 1).then(|| {
            CreateActionRow::Buttons(vec![
                CreateButton::new(format!("queue:{guild}:{}", page.saturating_sub(1)))
                    .label("◀")
                    .style(ButtonStyle::Secondary)
                    .disabled(page <= 1),
                CreateButton::new(format!("queue:{guild}:{}", page + 1))
                    .label("▶")
                    .style(ButtonStyle::Secondary)
                    .disabled(page >= pages),
            ])
        });
        Some((embed, row))
    }

    async fn queue(&self, guild: GuildId) -> Said {
        match self.queue_embed(guild, 1).await {
            Some((embed, row)) => Said::Queue(Box::new(embed), row),
            None => private("Nothing is playing."),
        }
    }

    async fn handle_component(&self, ctx: &Context, comp: ComponentInteraction) {
        let Some((guild, page)) = comp
            .data
            .custom_id
            .strip_prefix("queue:")
            .and_then(|s| s.split_once(':'))
        else {
            return;
        };
        let (Ok(guild), Ok(page)) = (guild.parse().map(GuildId::new), page.parse()) else {
            return;
        };
        if comp.guild_id != Some(guild) {
            return;
        }
        let resp = match self.queue_embed(guild, page).await {
            Some((embed, row)) => CreateInteractionResponseMessage::new()
                .embed(embed)
                .components(row.into_iter().collect()),
            None => CreateInteractionResponseMessage::new()
                .content("Nothing is playing.")
                .embeds(vec![])
                .components(vec![]),
        };
        let _ = comp
            .create_response(&ctx.http, CreateInteractionResponse::UpdateMessage(resp))
            .await;
    }

    fn pause(self: &Arc<Self>, guild: GuildId, pause: bool) -> Said {
        let changed = self.with_guild(guild, |g| {
            let current = g.current.as_mut()?;
            if current.paused == pause {
                return Some(false);
            }
            let _ = if pause {
                current.handle.pause()
            } else {
                current.handle.play()
            };
            current.paused = pause;
            Some(true)
        });
        self.refresh(guild);
        match (changed, pause) {
            (None, _) => private("Nothing is playing."),
            (Some(true), true) => "Paused.".into(),
            (Some(true), false) => "Resumed.".into(),
            (Some(false), true) => private("Already paused."),
            (Some(false), false) => private("Not paused."),
        }
    }

    fn remove(&self, req: Request) -> Said {
        let Ok(n) = req.args.parse::<usize>() else {
            return private("Remove which? Give its number from the queue.");
        };
        let removed = self.with_guild(req.guild, |g| g.queue.remove(n.checked_sub(1)?));
        removed.map_or(private(format!("There's no #{n} in the queue.")), |track| {
            format!("Removed {}", describe(&track)).into()
        })
    }

    fn shuffle(&self, guild: GuildId) -> Said {
        let n = self.with_guild(guild, |g| {
            player::shuffle(&mut g.queue);
            Some(g.queue.len())
        });
        match n.unwrap_or(0) {
            0 | 1 => private("Nothing to shuffle."),
            n => format!("Shuffled {n} tracks.").into(),
        }
    }

    fn set_loop(&self, req: Request) -> Said {
        let wanted = match req.args.to_lowercase().as_str() {
            "" => None,
            "off" => Some(Loop::Off),
            "track" | "this" | "song" => Some(Loop::Track),
            "queue" | "all" => Some(Loop::Queue),
            _ => return private("Loop how? Use off, track or queue."),
        };
        let mode = self.with_guild(req.guild, |g| {
            g.looping = wanted.unwrap_or(match g.looping {
                Loop::Off => Loop::Track,
                Loop::Track => Loop::Queue,
                Loop::Queue => Loop::Off,
            });
            Some((g.looping, g.current.as_ref().map(|c| describe(&c.track))))
        });
        match mode.unwrap_or((Loop::Off, None)) {
            (Loop::Off, _) => "Not looping.".into(),
            (Loop::Track, Some(what)) => format!("Looping {what}").into(),
            (Loop::Track, None) => "Looping the next track.".into(),
            (Loop::Queue, _) => "Looping the queue.".into(),
        }
    }

    async fn seek(&self, req: Request) -> Said {
        let (relative, time) = match req.args.split_at_checked(1) {
            Some(("+", rest)) => (Some(true), rest),
            Some(("-", rest)) => (Some(false), rest),
            _ => (None, req.args.as_str()),
        };
        let Some(time) = source::parse_clock(time) else {
            return private("Jump where? Like 1:30, 90, +30 or -10.");
        };
        let Some((handle, length)) = self.with_guild(req.guild, |g| {
            let c = g.current.as_ref()?;
            Some((c.handle.clone(), c.track.length()))
        }) else {
            return private("Nothing is playing.");
        };
        let to = match relative {
            None => time,
            Some(forward) => {
                let reported = handle
                    .get_info()
                    .await
                    .map(|i| i.position)
                    .unwrap_or_default();
                let now = self
                    .with_guild(req.guild, |g| Some(g.current.as_ref()?.position(reported)))
                    .unwrap_or(reported);
                // `+` panics on overflow, which aborts every bot
                if forward {
                    now.saturating_add(time)
                } else {
                    now.saturating_sub(time)
                }
            }
        };
        if let Some(length) = length.filter(|&length| to > length) {
            return private(format!("It's only {} long.", clock(length)));
        }
        match handle.seek(to).result_async().await {
            Ok(at) => {
                self.with_guild(req.guild, |g| {
                    if let Some(c) = g
                        .current
                        .as_mut()
                        .filter(|c| c.handle.uuid() == handle.uuid())
                    {
                        c.at = at;
                        c.started = Instant::now();
                    }
                    Some(())
                });
                format!("Jumped to {}.", clock(at)).into()
            }
            Err(e) => {
                warn!("{}: seek to {} failed: {e}", self.name, clock(to));
                let why = if e.to_string().contains("end of stream") {
                    "this stream doesn't support seeking there."
                } else {
                    "the track couldn't be seeked."
                };
                private(format!("Couldn't jump there: {why}"))
            }
        }
    }

    fn volume(&self, req: Request) -> Said {
        let Ok(percent) = req.args.parse::<u16>() else {
            let volume = self
                .with_guild(req.guild, |g| Some(g.volume))
                .unwrap_or(1.0);
            return private(format!("Volume {}%", (volume * 100.0).round()));
        };
        let volume = f32::from(percent.min(200)) / 100.0;
        self.with_guild(req.guild, |g| {
            g.volume = volume;
            let _ = g.current.as_ref()?.handle.set_volume(volume);
            Some(())
        });
        format!("Volume {}%", percent.min(200)).into()
    }

    /// Playback is controlled by the summoner while they're in voice, or anyone in voice if they
    /// left, and by DJs regardless. Everyone can still play, look, and skip or remove their own
    /// tracks.
    fn allowed(&self, name: &str, req: &Request) -> Result<(), String> {
        let dj = &self.settings.dj_roles;
        if OPEN.contains(&name)
            || (name == "eq" && req.args.is_empty())
            || req.roles.iter().any(|r| dj.contains(r))
        {
            return Ok(());
        }
        self.with_guild(req.guild, |g| {
            let mine = |t: &source::Track| t.requester == Some(req.user);
            let own = match name {
                "skip" => g.current.as_ref().is_some_and(|c| mine(&c.track)),
                "remove" => req
                    .args
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| g.queue.get(n.checked_sub(1)?))
                    .is_some_and(mine),
                _ => false,
            };
            let bot = self.user.get().and_then(|&me| g.channel_of(me));
            let summoner = g
                .summoner
                .filter(|&s| bot.is_some() && g.channel_of(s) == bot);
            Some(may_control(
                req.user,
                own,
                bot,
                g.channel_of(req.user),
                summoner,
                !dj.is_empty(),
            ))
        })
        .unwrap_or(Ok(()))
    }

    /// Starts the current track over, or plays the last one again when nothing is playing.
    async fn replay(self: &Arc<Self>, guild: GuildId) -> Said {
        let current = self.with_guild(guild, |g| {
            let current = g.current.as_mut()?;
            if current.paused {
                let _ = current.handle.play();
                current.paused = false;
            }
            Some((current.handle.clone(), describe(&current.track)))
        });
        let Some((handle, what)) = current else {
            return self.prev(guild, "Replaying").await;
        };
        self.refresh(guild);
        match handle.seek(Duration::ZERO).result_async().await {
            Ok(_) => {
                self.with_guild(guild, |g| {
                    if let Some(c) = g
                        .current
                        .as_mut()
                        .filter(|c| c.handle.uuid() == handle.uuid())
                    {
                        c.at = Duration::ZERO;
                        c.started = Instant::now();
                    }
                    Some(())
                });
                format!("Replaying {what}").into()
            }
            Err(e) => {
                warn!("{}: replay failed: {e}", self.name);
                private("Couldn't start it over: this stream doesn't support seeking back.")
            }
        }
    }

    /// Plays the last track again; the current one goes back to the front of the queue.
    async fn prev(self: &Arc<Self>, guild: GuildId, done: &str) -> Said {
        if self.songbird.get(guild).is_none() {
            return private("I'm not in a voice channel.");
        }
        let back = self.with_guild(guild, |g| {
            if g.loading {
                return Some(Err("Still loading a track, try again in a moment."));
            }
            let Some(mut track) = g.history.pop_back() else {
                return Some(Err("Nothing played before this."));
            };
            // taken first, so its End event finds nothing to move on from
            if let Some(current) = g.current.take() {
                let _ = current.handle.stop();
                g.queue.push_front(current.track);
            }
            track.resume_at = None;
            let what = describe(&track);
            g.queue.push_front(track);
            g.loading = true;
            Some(Ok(what))
        });
        match back {
            Some(Ok(what)) => {
                self.play_next(guild, false).await;
                format!("{done} {what}").into()
            }
            Some(Err(why)) => private(why),
            None => private("Nothing played before this."),
        }
    }

    fn clear(&self, guild: GuildId) -> Said {
        let cleared = self.with_guild(guild, |g| Some(g.queue.drain(..).count()));
        match cleared.unwrap_or(0) {
            0 => private("The queue is already empty."),
            1 => "Cleared 1 track.".into(),
            n => format!("Cleared {n} tracks.").into(),
        }
    }

    async fn eq(self: &Arc<Self>, req: Request) -> Said {
        let usage = format!(
            "Presets: {}. Or your own, in dB from -{limit} to {limit} for bass, low, mid, high and treble, like `eq 6 2 0 -1 3`.",
            Preset::ALL.map(Preset::name).join(", "),
            limit = eq::BAND_LIMIT,
        );
        let now = self
            .with_guild(req.guild, |g| Some(g.eq))
            .unwrap_or(Preset::Off);
        if req.args.is_empty() {
            return private(format!("EQ {}. {usage}", now.describe()));
        }
        let Some(preset) = Preset::parse(&req.args) else {
            return private(format!("That's not an EQ. {usage}"));
        };
        if preset == now {
            return private(format!("EQ is already {}.", now.describe()));
        }
        self.with_guild(req.guild, |g| {
            g.eq = preset;
            Some(())
        });
        self.restart(req.guild).await;
        format!("EQ {}.", preset.describe()).into()
    }

    async fn help(&self, ctx: &Context, req: &Request) -> Said {
        let content = help(&self.prefix, !self.settings.dj_roles.is_empty());
        let message = CreateMessage::new()
            .content(content)
            .allowed_mentions(CreateAllowedMentions::new());
        match req.user.dm(&ctx.http, message).await {
            Ok(_) => Said::Temporary("📬 Sent to your DMs.".into(), Duration::from_secs(5)),
            Err(e) => {
                warn!("{}: couldn't DM help to {}: {e}", self.name, req.user);
                private("❌ I couldn't DM you. Enable DMs from server members.")
            }
        }
    }

    /// Posts "Now playing" where play was last used, in place of the previous announcement.
    pub fn announce(self: &Arc<Self>, guild: GuildId) {
        let Some(http) = self.http.get().cloned() else {
            return;
        };
        let Some((channel, old, text)) = self.with_guild(guild, |g| {
            let text = format!("Now playing {}", describe(&g.current.as_ref()?.track));
            Some((g.announce_in?, g.announcement.take(), text))
        }) else {
            return;
        };
        let bot = self.clone();
        tokio::spawn(async move {
            if let Some((channel, message)) = old {
                let _ = channel.delete_message(&http, message).await;
            }
            let message = CreateMessage::new()
                .content(text)
                .allowed_mentions(CreateAllowedMentions::new());
            match channel.send_message(&http, message).await {
                Ok(sent) => {
                    bot.with_guild(guild, |g| g.announcement.replace((channel, sent.id)));
                }
                Err(e) => warn!("{}: announcing a track: {e}", bot.name),
            }
        });
    }

    /// Says where play was last used that a track was dropped when its turn came, and how many
    /// other queued tracks went with it.
    pub fn dropped(
        self: &Arc<Self>,
        guild: GuildId,
        track: &source::Track,
        why: &str,
        cleared: usize,
    ) {
        let Some(http) = self.http.get().cloned() else {
            return;
        };
        let Some(channel) = self.with_guild(guild, |g| g.announce_in) else {
            return;
        };
        let mut text = format!("Couldn't play {}: {why}", describe(track));
        match cleared {
            0 => {}
            1 => text.push_str(". Cleared the queue's other YouTube track too."),
            n => drop(write!(
                text,
                ". Cleared the queue's {n} other YouTube tracks too."
            )),
        }
        let message = CreateMessage::new()
            .content(text)
            .allowed_mentions(CreateAllowedMentions::new());
        let bot = self.clone();
        tokio::spawn(async move {
            if let Err(e) = channel.send_message(&http, message).await {
                warn!("{}: telling about a dropped track: {e}", bot.name);
            }
        });
    }

    /// Gateway heartbeat latency, unknown until the first heartbeat.
    async fn ping(&self, ctx: &Context) -> String {
        let Some(shards) = self.shards.get() else {
            return "🏓 Pong!".into();
        };
        match shards
            .runners
            .lock()
            .await
            .get(&ctx.shard_id)
            .and_then(|r| r.latency)
        {
            Some(latency) => format!("🏓 Pong! Latency: **{}ms**", latency.as_millis()),
            None => "🏓 Pong!".into(),
        }
    }
}

/// Every command, from the slash command definitions.
fn help(prefix: &str, dj: bool) -> String {
    let mut text = match prefix {
        "" => "Commands, as slash commands:".to_string(),
        prefix => format!("Commands, as slash commands or text like `{prefix}play`:"),
    };
    for command in commands() {
        let json = serde_json::to_value(&command).unwrap_or_default();
        let name = json["name"].as_str().unwrap_or_default();
        let mut usage = name.to_string();
        for option in json["options"].as_array().into_iter().flatten() {
            let arg = option["name"].as_str().unwrap_or_default();
            let _ = match option["required"].as_bool() {
                Some(true) => write!(usage, " <{arg}>"),
                _ => write!(usage, " [{arg}]"),
            };
        }
        let aliases: Vec<String> = ALIASES
            .iter()
            .filter(|(_, n)| *n == name)
            .map(|(a, _)| format!("`{a}`"))
            .collect();
        let aliases = match aliases.is_empty() {
            true => String::new(),
            false => format!(" ({})", aliases.join(", ")),
        };
        let description = json["description"].as_str().unwrap_or_default();
        let _ = write!(text, "\n`{usage}`{aliases}: {description}");
    }
    if dj {
        text.push_str("\nOnly DJs, and whoever brought me in while they're in my channel, can control playback. Everyone can play, see the queue, and skip or remove their own tracks.");
    } else {
        text.push_str("\nWhoever brought me in while they're in my channel can control playback. Everyone can play, see the queue, and skip or remove their own tracks.");
    }
    text
}

/// What a slow command shows while it works, or None for one that answers right away.
fn first_status(name: &str, args: &str) -> Option<String> {
    let text = match name {
        "play" if args.is_empty() => return None,
        "play" if args.contains("spotify.com/") || args.starts_with("spotify:") => {
            "Reading the Spotify link..."
        }
        "play" if args.starts_with("https://") || args.starts_with("http://") => {
            "Looking up the link..."
        }
        "play" => "Searching...",
        "prev" => "Going back...",
        "replay" => "Starting over...",
        "eq" if !args.is_empty() => "Setting the EQ...",
        _ => return None,
    };
    Some(text.into())
}

#[cfg(test)]
fn command_name(command: &CreateCommand) -> String {
    let json = serde_json::to_value(command).unwrap_or_default();
    json["name"].as_str().unwrap_or_default().to_string()
}

/// Playback is controlled by whoever brought the bot in while they're in its channel, and
/// otherwise by anyone in its channel. DJs and own-track actions are let through before this.
fn may_control(
    user: UserId,
    own_track: bool,
    bot: Option<ChannelId>,
    user_in: Option<ChannelId>,
    summoner_here: Option<UserId>,
    has_dj: bool,
) -> Result<(), String> {
    match (bot, summoner_here) {
        _ if own_track => Ok(()),
        (None, _) => Ok(()),
        (Some(bot), _) if user_in != Some(bot) => Err(format!("Join <#{bot}> to control me.")),
        (_, Some(summoner)) if summoner != user => {
            let who = match has_dj {
                true => "only they and DJs",
                false => "only they",
            };
            Err(format!(
                "<@{summoner}> brought me in, so {who} can do that while they're here."
            ))
        }
        _ => Ok(()),
    }
}

/// **Title** (3:07)
fn describe(track: &source::Track) -> String {
    let length = track
        .length()
        .map(|d| format!(" ({})", clock(d)))
        .unwrap_or_default();
    link(track) + &length
}

/// Discord shows backslash escapes in link text as typed, so whatever could break the link or
/// bold becomes a look-alike. Long titles are cut so a queue fits in one message.
fn escape_title(title: &str) -> String {
    title
        .chars()
        .take(100)
        .map(|c| match c {
            '[' => '\u{ff3b}',
            ']' => '\u{ff3d}',
            '*' => '\u{2217}',
            '\n' | '\r' => ' ',
            c => c,
        })
        .collect()
}

/// The title in bold, linked without an embed.
fn link(track: &source::Track) -> String {
    format!("**[{}](<{}>)**", escape_title(&track.title), track.page())
}

/// The playlist title in bold, linked without an embed, followed by its kind (e.g. YouTube playlist).
fn playlist_link(playlist: &source::Playlist) -> String {
    let title = escape_title(&playlist.title);
    if playlist.url.is_empty() {
        format!("**{title}** {}", playlist.kind)
    } else {
        format!("**[{title}](<{}>)** {}", playlist.url, playlist.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dj_rules() {
        let (me, them) = (UserId::new(1), UserId::new(2));
        let (here, there) = (ChannelId::new(10), ChannelId::new(20));
        assert!(may_control(me, false, None, None, None, false).is_ok());
        assert!(may_control(me, false, Some(here), Some(here), Some(me), false).is_ok());
        // someone else brought it in and is still listening
        assert!(may_control(me, false, Some(here), Some(here), Some(them), false).is_err());
        assert!(may_control(me, false, Some(here), Some(here), Some(them), true).is_err());
        // they left, so it's up to whoever's in the channel
        assert!(may_control(me, false, Some(here), Some(here), None, false).is_ok());
        assert!(may_control(me, false, Some(here), Some(there), None, false).is_err());
        assert!(may_control(me, false, Some(here), None, Some(me), false).is_err());
        assert!(may_control(me, true, Some(here), Some(there), Some(them), false).is_ok());
    }

    #[test]
    fn command_lists_match() {
        let names: Vec<String> = commands().iter().map(command_name).collect();
        assert_eq!(names.len(), COMMANDS.len());
        let listed = COMMANDS
            .iter()
            .chain(ALIASES.iter().map(|(_, n)| n))
            .chain(OPEN);
        for name in listed {
            assert!(names.iter().any(|n| n == name), "{name} isn't a command");
        }
        for name in &names {
            assert!(
                COMMANDS.contains(&name.as_str()),
                "{name} missing from COMMANDS"
            );
        }
        let help = help("t.", true);
        assert!(help.contains("`seek <time>`") && help.contains("`play <track>` (`p`)"));
        assert!(
            help.len() < 2000,
            "help is {} bytes, more than a message",
            help.len()
        );
    }
}
