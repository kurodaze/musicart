# songbird 0.6.1, vendored

This is [songbird](https://github.com/serenity-rs/songbird) 0.6.1 from crates.io (ISC license, see LICENSE.md) with one feature added and two bugs fixed for musicart. Drop this copy once songbird has all three.

## `Config::stun_server`

Some networks filter Discord voice: they drop nearly all of a voice flow that starts with Discord's 74-byte IP discovery packet. With `stun_server` set, songbird asks that STUN server for the public address and port from the same UDP socket instead, and never sends the discovery packet. Measured on a home connection behind such a filter: 44 of 1,000 audio packets arrived with IP discovery, 1,000 of 1,000 with STUN.

Changed files:

- `src/config.rs`: the `stun_server` field, its default and builder
- `src/driver/connection/mod.rs`: IP discovery moved into `discover()`, plus `stun_mapped()`
- `src/driver/scheduler/idle.rs`: `#[allow(unused_imports)]` on one import, to keep builds free of a warning the release already has

## Seeking a track that's still loading

songbird queues a seek sent while a track's input is still being made, but 0.6.1 throws the queued seek away on the next tick if the input isn't ready yet, so the track plays from the start. musicart seeks right after starting a track to resume it where its stream failed. The queued seek is now kept until the input is ready. Measured: a seek to 0:56 sent right after starting a track landed at 0:56.001 with the fix, and was dropped without it.

Changed files:

- `src/driver/tasks/mixer/track.rs`: `get_or_ready_input` keeps the queued seek while the input isn't ready

## Seeking HTTP streams

songbird 0.6.1 never seeks an HTTP stream. Seeking back reopens it from the start and reads forward, and a short seek forward fails with "stream can only be seeked forward" when the target is already buffered, which fails the track. A server that takes byte ranges is now seeked by reconnecting at the target, as songbird already does after a dropped connection. Measured on YouTube: a seek 4 seconds ahead, 10 back, and both with the stream decoded for an EQ all landed where asked.

Changed files:

- `src/input/sources/http.rs`: `HttpStream` is seekable when the server takes ranges, reports the whole file's length after reconnecting, and `poll_complete` no longer panics, since tokio calls it before every seek
- `src/input/adapters/async_adapter.rs`: the sink seeks by reconnecting and drops bytes read before the seek; the reader turns relative seeks into absolute ones, since it's behind the sink
