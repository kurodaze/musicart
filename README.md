# musicart

A self-hosted Discord music bot in Rust. One process runs several bots, each with its own token, name and avatar, so a server can have more than one playing at once.

It's built to be small and light. Both bots together use about 12 MB of RAM while idle, and about 15 MB with one playing. Each yt-dlp lookup runs as its own process and peaks around 80 MB for a few seconds. YouTube's Opus audio goes to Discord exactly as YouTube sends it, never decoded and re-encoded, unless you change the volume or turn on the EQ.

## Setup

It runs on Linux, x86_64 or aarch64. It needs:

- a residential internet connection, or [YouTube cookies](#are-you-a-bot) (required for VPS/datacenter IPs, where YouTube blocks signed-out requests).

[yt-dlp](https://github.com/yt-dlp/yt-dlp/releases) and [deno](https://deno.com), which yt-dlp needs for YouTube, are downloaded into `data/bin/` on the first start and checked against the checksums their releases publish. A deno on the system path is used instead if there is one (the container has one). musicart keeps yt-dlp current, since an old one stops working whenever YouTube changes: it checks for a new version at startup, every 6 hours, and right away when yt-dlp says a newer version may fix a failure. Delete a file in `data/bin/` to get a fresh copy.

In the Discord developer portal, create an application per bot and copy its token. Turn on the **Message Content** intent if you'll use text commands like `t.play`. Invite each bot with the `bot` and `applications.commands` scopes and the **View Channel**, **Send Messages**, **Read Message History**, **Connect** and **Speak** permissions. Set its name and avatar in the portal.

Then make a `musicart.toml` from [example.toml](example.toml), with a `[bot.<name>]` section and token per bot.

### 1. Run via Podman / Docker (Recommended)

The image has deno, and fetches yt-dlp into its data folder on the first start. It runs as `1000:1000` (`user:` in `compose.yaml` and below), which must be able to read `musicart.toml` and own `data/`, where musicart keeps yt-dlp, the Spotify login and the cookies file: use your own `id -u`:`id -g` if those differ. Create `data/` before the first start, or Docker creates it owned by root.

```bash
mkdir -p data
podman compose up -d   # with compose.yaml from this repo, next to musicart.toml; rootless Podman: uncomment userns_mode
# or: docker compose up -d
```

Or run directly:
```bash
# Podman, rootless (SELinux compatible)
podman run -d --name musicart --restart unless-stopped --init --user 1000:1000 --userns=keep-id -v ./musicart.toml:/app/musicart.toml:ro,Z -v ./data:/app/data:Z ghcr.io/kurodaze/musicart:latest

# Docker
docker run -d --name musicart --restart unless-stopped --init --user 1000:1000 -v ./musicart.toml:/app/musicart.toml:ro -v ./data:/app/data ghcr.io/kurodaze/musicart:latest
```

`compose.dev.yaml` builds the image from the checkout instead, with the faster-to-build dev profile.

### 2. Or Install Native Binary

Download a static `musicart` binary (`-x86_64` / `-aarch64`) from [GitHub Releases](https://github.com/kurodaze/musicart/releases) and check it with `sha256sum -c --ignore-missing SHA256SUMS`, or build it:

```bash
# Runtime: yt-dlp, and deno unless one is on PATH, are downloaded into data/bin/ on first use.
# Building needs Rust and cmake (for libopus, the Opus encoder).
git clone https://github.com/kurodaze/musicart && cd musicart
./scripts/build.sh --release   # a static ./musicart, see --help
cp example.toml musicart.toml   # add a token per bot
./musicart   # Ctrl+C to stop
```

To run it as a systemd user service, with the checkout at `~/musicart`:

```sh
systemctl --user link ~/musicart/musicart.service
loginctl enable-linger "$USER"   # keeps it running after logout and starts it at boot
systemctl --user enable --now musicart
journalctl --user -u musicart -f
```

After editing `musicart.toml`, `systemctl --user restart musicart`. To update: `git pull && ./scripts/build.sh --release`, then restart.

## Settings

`example.toml` lists every setting with its default. A typo or a value of the wrong type stops musicart at startup with the line it's on. The config file is `musicart.toml` in the working directory, or the path given as the first argument or in `MUSICART_CONFIG`.

| Key | Default | What |
| --- | --- | --- |
| `log_level` | `"info"` | `error`, `warn`, `info` (a line per track) or `debug` |
| `idle_timeout` | `900` | Seconds without playing to anyone (nothing playing, paused, or only bots listening) before leaving voice. `0` = never |
| `track_status` | `true` | Show "Listening to <track>" as the bot's status, the newest track if it plays in several servers |
| `announce_tracks` | `false` | Post "Now playing" when a track starts by itself, in the channel where `play` was last used. The previous announcement is deleted, so only the current one stays |
| `dj_role_ids` | none | Roles that can always control playback, e.g. `[123456789012345678]`, see [DJ roles](#dj-roles) |
| `stun` | none | STUN server for [networks that filter Discord voice](#networks-that-filter-discord-voice) |
| `togematch_api` | none | Optional [togematch-api](https://github.com/kurodaze/togematch) on your network, e.g. `"http://192.168.1.10:7094"`, see [What it plays](#what-it-plays) |
| `spotify` | `false` | Plays Spotify links from Spotify on a Premium account, see [Spotify audio](#spotify-audio) |
| `[bot.<name>]` `token` | required | The bot's token; `<name>` is what the log calls it |
| `[bot.<name>]` `prefix` | none | Prefix for text commands, e.g. `"t."`; without one the bot only has slash commands |

## Commands

Every command is a slash command, and with a `prefix` also a text command. Prefixes ignore case, since phones like to capitalize `T.p`.

| Command | Alias | What |
| --- | --- | --- |
| `play <link or search>` | `p` | Joins your voice channel and plays, or adds to the queue |
| `skip` | `s` | Skips the current track |
| `prev` | | Plays the previous track again from the start; the current one goes back to the front of the queue |
| `replay` | | Starts the current track over, or plays the last one again when nothing is playing |
| `queue` | `q` | What's playing and the next 10 |
| `nowplaying` | `np` | The current track and how far in it is |
| `pause`, `resume` | | |
| `stop` | | Clears the queue and stops, but stays in voice |
| `clear` | | Clears the queue but keeps playing |
| `remove <number>` | | Removes a track from the queue |
| `shuffle` | | Shuffles the queue |
| `loop [off\|track\|queue]` | `repeat` | Loops the track or the queue; without a mode it switches to the next one |
| `seek <1:30, 90, +30 or -10>` | | Jumps to a time in the track, or ahead or back by some |
| `volume [0-200]` | | Shows or sets the volume. At 100% YouTube's audio passes through untouched; anything else is decoded and re-encoded at 144 kbps |
| `eq [preset or 5 gains]` | | Shows or sets the EQ: a preset, or your own like `eq 6 2 0 -1 3`, see [Sound quality](#sound-quality) |
| `disconnect` | `die`, `leave` | Clears the queue and leaves voice |
| `help` | | Lists the commands |
| `ping` | | `🏓 Pong! Latency: **42ms**` |

A bot that's already playing to people in another channel says so instead of moving; ask another bot. A server's queue holds up to 1000 tracks; a playlist that doesn't fit is cut short.

A command that has to look something up answers at once: a text command with what it's doing (Searching..., Getting the audio...), changed as it goes, and a slash command with Discord's "thinking...". Slash commands answer only you when nothing changes for anyone else: a mistake, a refusal, or looking at the queue.

### DJ roles

- Everyone can `play`, see the `queue` and `nowplaying`, check the `eq`, and `skip` or `remove` tracks they queued themselves.
- Whoever brought the bot into voice can control playback (and set the EQ) while they're in its channel. Once they leave, anyone in its channel can.
- With `dj_role_ids` set, members with a DJ role can always do everything regardless of who summoned the bot or whether they are in voice.

Copy a role's ID with Developer Mode on (User Settings → Advanced), then right-click the role.

## What it plays

- **YouTube links**: a video, or a playlist (up to 500 tracks). A video link with `&list=` plays just the video.
- **SoundCloud links**: a track, or a playlist/set.
- **Searches**: YouTube's top result.
- **Spotify links**: tracks, albums and playlists, read from Spotify's public embed page, so no API keys. When a track is about to play, the togematch library finds its own recording on YouTube, preferring the licensed "Provided to YouTube" upload over music videos, live versions and re-uploads. Spotify only shows the first 100 tracks of a playlist that way.

Links to other sites are refused, since yt-dlp would fetch them from inside your network.

With a [Spotify login](#spotify-audio), Spotify links and searches for a track play Spotify's own audio instead.

Optionally, set `togematch_api` and Spotify links and searches go to that togematch-api. It keeps every match per recording, so a track is searched for on YouTube once however it's asked for; a search that names a track plays that track's recording instead of YouTube's top result; and wrong matches can be fixed with its CLI. If it can't be reached, musicart searches YouTube itself for the next minute. With Spotify audio on, it's only asked about what Spotify doesn't play: searches Spotify finds no track for, and tracks Spotify won't play.

While a track plays, the next one is looked up, so tracks follow each other without a gap. Stream links are remembered in memory for 3 hours, and what searches and links found and YouTube matches for a day, so playing something again doesn't look it up again. Tracks that aren't on YouTube and videos that are gone are remembered for a day too, so they're skipped at once, and a match whose video is gone is looked up again. The same lookup asked for twice at once runs once. If a track's stream fails, the track gets a fresh one and carries on where it stopped, and if that fails too it's skipped. A lookup that fails in a way that may pass, like a dropped connection, is tried once more after 2 seconds.

## Spotify audio

With a Spotify Premium account signed in, Spotify links play Spotify's own audio through [librespot](https://github.com/librespot-org/librespot) instead of a YouTube upload. It's always the exact recording, and a track starts without searching YouTube first.

1. Set `spotify = true` in `musicart.toml`.
2. Start musicart. Its log shows a spotify.com/pair link: open it and sign in with the Premium account. Spotify links play from Spotify right away.

musicart keeps the login in `data/spotify-login.json`, readable only by you, and renews it there as Spotify asks. In a container, `data/` is the folder mounted from `compose.yaml`. If the login stops working, the log shows a new link. A free account can't be used, since librespot only works with Premium.

A search asks Spotify first. The first of Spotify's top 5 tracks that has every typed word in its artist and title plays, so "dota basshunter radio edit" plays Basshunter's DotA (Radio Edit). Live versions and remixes only play when typed, and karaoke and cover releases never do. A search no Spotify track fits, like "funny cat video", goes on as before. YouTube links and any track Spotify won't play still come from YouTube.

A track's details and audio key are remembered for a day too, so playing it again only downloads its audio.

Playing from Spotify uses slightly more CPU and memory than playing from YouTube. Its audio is decoded and re-encoded for Discord, and librespot keeps a connection open and the whole track in the system's temp folder while it plays. That folder is memory where it's a tmpfs.

Spotify sends Ogg Vorbis at 320 kbps, or what the plan allows, and it's re-encoded to Opus for Discord. Playing Spotify into a voice channel is against Spotify's terms, and Spotify has suspended accounts used with librespot tools, so a spare Premium account is safer.

## Networks that filter Discord voice

Some networks drop nearly all of a Discord voice connection, so the bot stutters or is silent while its log shows nothing wrong. The ones we've seen recognise the connection by the address discovery packet it starts with. With `stun = "stun.cloudflare.com:3478"`, the bot asks that STUN server for its public address instead and never sends that packet. On such a network that took delivery from 44 to 1,000 of 1,000 packets. This needs a small change to songbird, kept in `vendor/songbird` (see `VENDORED.md` there).

## Are you a bot?

YouTube blocks signed-out requests from datacenter/VPS addresses and sometimes answers "Sign in to confirm you're not a bot" to a busy home connection. Without cookies, the bot says it can't play anything for now and clears the queue's other YouTube tracks, which would fail the same way. A logged-in session is trusted much more, so give yt-dlp the cookies of a spare YouTube account (YouTube can flag accounts used like this).

The [togematch README](https://github.com/kurodaze/togematch#are-you-a-bot) explains how to export the cookies. Save them into `data/` (e.g. `data/cookies.txt` or `data/www.youtube.com_cookies.txt`), or stream them into a running container:
```bash
docker exec -i musicart sh -c 'cat > /app/data/cookies.txt && chmod 600 /app/data/cookies.txt' < path/to/cookies.txt
# or with Podman:
podman exec -i musicart sh -c 'cat > /app/data/cookies.txt && chmod 600 /app/data/cookies.txt' < path/to/cookies.txt
```
musicart automatically detects `cookies.txt` (or any `*cookie*.txt`) in `data/`, including when added or replaced while running.

The bot still looks things up logged out, which is faster, and only uses the cookies for an hour after YouTube asks to sign in.

Cookies don't last forever. When YouTube stops accepting them, or the file is gone, the bot carries on logged out and says so once in its log. Export new ones the same way; replacing the file is enough, no restart needed.

## Sound quality

The bot sends YouTube's best free audio, Opus at about 130 kbps, without changing it. Spotify audio is always re-encoded, at 144 kbps. Discord accepts more than the channel's bitrate setting from bots, so it arrives as sent. For the best sound, listen in the Discord desktop app.

The EQ presets each server can pick:

| Preset | What |
| --- | --- |
| `bass` | +6 dB below 100 Hz |
| `treble` | +5 dB above 6 kHz |
| `vocals` | +4 dB around 2.5 kHz, -3 dB below 150 Hz |
| `soft` | -5 dB above 5 kHz, for harsh recordings |

Or set your own: five gains from -12 to 12 dB for bass (60 Hz), low (250 Hz), mid (1 kHz), high (4 kHz) and treble (12 kHz), like `eq 6 2 0 -1 3` or `/eq bands:6 2 0 -1 3`. Neighbouring bands overlap, so each is corrected for the others to land where asked. Only the differences matter, so `eq 4 4 4 4 4` is the same as off.

An EQ turns the rest down rather than the boost up, so it never clips, and sounds a little quieter. With an EQ or a volume other than 100%, the audio is decoded and re-encoded at 144 kbps. Changing the EQ restarts the track where it was, so there's a short gap.

## Disclaimer & Acknowledgements

musicart is **not affiliated with Discord, YouTube or Spotify**.

musicart uses [serenity](https://github.com/serenity-rs/serenity) for Discord, [songbird](https://github.com/serenity-rs/songbird) for voice (a patched copy in `vendor/songbird`), [Symphonia](https://github.com/pdeljanov/Symphonia) to decode audio from Spotify or when the volume or EQ changes it and [Opus](https://opus-codec.org/) to encode it again, [yt-dlp](https://github.com/yt-dlp/yt-dlp) with [Deno](https://deno.com/) for YouTube, [librespot](https://github.com/librespot-org/librespot) for Spotify audio, and [togematch](https://github.com/kurodaze/togematch) to find Spotify tracks on YouTube.

## License

Copyright (C) 2026 KuroDaze. Licensed under the GNU General Public License v3.0 or later, see [LICENSE](LICENSE).  
`vendor/songbird` is ISC, see its `LICENSE.md`.
