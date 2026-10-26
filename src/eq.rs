//! Equalizer presets. A preset decodes the stream, runs it through biquad filters (the RBJ audio
//! EQ cookbook) and hands songbird raw PCM, which it encodes again like it does for volume.

use songbird::input::codecs::{get_codec_registry, get_probe};
use songbird::input::{AudioStream, Input, RawAdapter};
use std::f64::consts::PI;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::{Time, TimeBase};

/// The custom EQ's bands and where they are, in Hz. Bass and treble are shelves that reach
/// their full gain there, from 120 Hz down and 6.5 kHz up.
pub const BANDS: [(&str, f64); 5] = [
    ("bass", 60.0),
    ("low", 250.0),
    ("mid", 1000.0),
    ("high", 4000.0),
    ("treble", 12000.0),
];
const SHELVES: (f64, f64) = (120.0, 6500.0);
/// Wide enough that neighbouring bands, two octaves apart, meet without a dip.
const BAND_Q: f64 = 0.6;
/// How far a custom band goes either way, in dB.
pub const BAND_LIMIT: i8 = 12;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Preset {
    Off,
    Bass,
    Treble,
    Vocals,
    Soft,
    /// a gain in dB per band
    Custom([i8; 5]),
}

impl Preset {
    pub const ALL: [Preset; 5] = [
        Preset::Off,
        Preset::Bass,
        Preset::Treble,
        Preset::Vocals,
        Preset::Soft,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Preset::Off => "off",
            Preset::Bass => "bass",
            Preset::Treble => "treble",
            Preset::Vocals => "vocals",
            Preset::Soft => "soft",
            Preset::Custom(_) => "custom",
        }
    }

    /// A preset's name, or five gains in dB for the custom bands, like "6 2 0 -1 3".
    pub fn parse(text: &str) -> Option<Preset> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("none") || text.eq_ignore_ascii_case("reset") {
            return Some(Preset::Off);
        }
        if let Some(preset) = Preset::ALL
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(text))
        {
            return Some(preset);
        }
        let gains: Vec<i8> = text
            .split_whitespace()
            .map(|g| {
                g.parse()
                    .ok()
                    .filter(|g| (-BAND_LIMIT..=BAND_LIMIT).contains(g))
            })
            .collect::<Option<_>>()?;
        let gains: [i8; 5] = gains.try_into().ok()?;
        // only the differences matter, since the result is turned down to its loudest part anyway
        Some(match gains.iter().all(|&g| g == gains[0]) {
            true => Preset::Off,
            false => Preset::Custom(gains),
        })
    }

    /// The name, or for a custom EQ its bands, like "custom: bass +6, low +2, mid 0, ...".
    pub fn describe(self) -> String {
        let Preset::Custom(gains) = self else {
            return self.name().into();
        };
        let bands: Vec<String> = BANDS
            .iter()
            .zip(gains)
            .map(|((band, _), gain)| match gain {
                0 => format!("{band} 0"),
                gain => format!("{band} {gain:+}"),
            })
            .collect();
        format!("custom: {}", bands.join(", "))
    }

    fn design(self, rate: f64) -> Vec<Biquad> {
        match self {
            Preset::Off => vec![],
            Preset::Bass => vec![Biquad::low_shelf(rate, 100.0, 6.0)],
            Preset::Treble => vec![Biquad::high_shelf(rate, 6000.0, 5.0)],
            Preset::Vocals => vec![
                Biquad::low_shelf(rate, 150.0, -3.0),
                Biquad::peak(rate, 2500.0, 4.0, 1.0),
            ],
            Preset::Soft => vec![Biquad::high_shelf(rate, 5000.0, -5.0)],
            Preset::Custom(gains) => custom(gains, rate),
        }
    }
}

/// Neighbouring bands overlap and add up, so each band's own filter is corrected until the whole
/// curve has the asked-for gain at every band. Gains are taken relative to the highest, so the
/// filters only cut.
fn custom(gains: [i8; 5], rate: f64) -> Vec<Biquad> {
    let top = gains.iter().copied().max().unwrap_or(0);
    let want = gains.map(|g| f64::from(g - top));
    let filters = |set: [f64; 5]| -> Vec<Biquad> {
        (0..5)
            .map(|i| match i {
                0 => Biquad::low_shelf(rate, SHELVES.0, set[0]),
                4 => Biquad::high_shelf(rate, SHELVES.1, set[4]),
                _ => Biquad::peak(rate, BANDS[i].1, set[i], BAND_Q),
            })
            .collect()
    };
    let mut set = want;
    for _ in 0..20 {
        let now = filters(set);
        for (i, (_, freq)) in BANDS.iter().enumerate() {
            let got: f64 = now.iter().map(|f| f.gain_db(*freq, rate)).sum();
            set[i] += want[i] - got;
        }
    }
    filters(set)
}

/// The most the filters together raise any frequency, in dB.
fn peak_db(filters: &[Biquad], rate: f64) -> f64 {
    let top = 20_000f64.min(rate * 0.45);
    (0..=400)
        .map(|i| 20.0 * (top / 20.0).powf(f64::from(i) / 400.0))
        .map(|freq| filters.iter().map(|f| f.gain_db(freq, rate)).sum::<f64>())
        .fold(0.0, f64::max)
}

/// One second-order filter, normalized so a0 = 1.
#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Biquad {
    fn new(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Biquad {
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// Shelves have a slope of 1, the steepest without a bump.
    fn low_shelf(rate: f64, freq: f64, gain_db: f64) -> Biquad {
        let (a, cos, alpha) = Biquad::terms(rate, freq, gain_db, 1.0 / 2f64.sqrt());
        let s = 2.0 * a.sqrt() * alpha;
        Biquad::new(
            a * ((a + 1.0) - (a - 1.0) * cos + s),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
            a * ((a + 1.0) - (a - 1.0) * cos - s),
            (a + 1.0) + (a - 1.0) * cos + s,
            -2.0 * ((a - 1.0) + (a + 1.0) * cos),
            (a + 1.0) + (a - 1.0) * cos - s,
        )
    }

    fn high_shelf(rate: f64, freq: f64, gain_db: f64) -> Biquad {
        let (a, cos, alpha) = Biquad::terms(rate, freq, gain_db, 1.0 / 2f64.sqrt());
        let s = 2.0 * a.sqrt() * alpha;
        Biquad::new(
            a * ((a + 1.0) + (a - 1.0) * cos + s),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
            a * ((a + 1.0) + (a - 1.0) * cos - s),
            (a + 1.0) - (a - 1.0) * cos + s,
            2.0 * ((a - 1.0) - (a + 1.0) * cos),
            (a + 1.0) - (a - 1.0) * cos - s,
        )
    }

    fn peak(rate: f64, freq: f64, gain_db: f64, q: f64) -> Biquad {
        let (a, cos, alpha) = Biquad::terms(rate, freq, gain_db, q);
        Biquad::new(
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        )
    }

    /// How much this filter raises or lowers `freq`, in dB.
    fn gain_db(&self, freq: f64, rate: f64) -> f64 {
        let w = 2.0 * PI * freq / rate;
        let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
        let num =
            (self.b0 + self.b1 * c1 + self.b2 * c2).powi(2) + (self.b1 * s1 + self.b2 * s2).powi(2);
        let den =
            (1.0 + self.a1 * c1 + self.a2 * c2).powi(2) + (self.a1 * s1 + self.a2 * s2).powi(2);
        10.0 * (num / den).log10()
    }

    fn terms(rate: f64, freq: f64, gain_db: f64, q: f64) -> (f64, f64, f64) {
        let w = 2.0 * PI * freq / rate;
        (10f64.powf(gain_db / 40.0), w.cos(), w.sin() / (2.0 * q))
    }
}

/// A preset's filters running over interleaved samples, with the history each channel needs.
struct Chain {
    gain: f64,
    filters: Vec<Biquad>,
    /// x1, x2, y1, y2 per filter and channel
    state: Vec<[f64; 4]>,
    channels: usize,
}

impl Chain {
    /// Turns everything down by as much as the filters raise their loudest part, so it can't clip.
    fn new(preset: Preset, rate: u32, channels: usize) -> Chain {
        let filters = preset.design(f64::from(rate));
        let state = vec![[0.0; 4]; filters.len() * channels];
        Chain {
            gain: 10f64.powf(-peak_db(&filters, f64::from(rate)) / 20.0),
            filters,
            state,
            channels,
        }
    }

    fn run(&mut self, samples: &mut [f32]) {
        for frame in samples.chunks_mut(self.channels) {
            for (c, sample) in frame.iter_mut().enumerate() {
                let mut x = f64::from(*sample) * self.gain;
                for (f, s) in self
                    .filters
                    .iter()
                    .zip(self.state.iter_mut().skip(c).step_by(self.channels))
                {
                    let y = f.b0 * x + f.b1 * s[0] + f.b2 * s[1] - f.a1 * s[2] - f.a2 * s[3];
                    *s = [x, s[0], y, s[2]];
                    x = y;
                }
                *sample = x as f32;
            }
        }
    }

    fn reset(&mut self) {
        self.state.fill([0.0; 4]);
    }
}

/// A stream's audio with an EQ preset on it. Opening it reads the stream, so run it off the
/// async runtime.
pub fn open(stream: AudioStream<Box<dyn MediaSource>>, preset: Preset) -> Result<Input, String> {
    let source = Equalized::new(stream.input, preset)?;
    let (rate, channels) = (source.rate, source.channels as u32);
    Ok(RawAdapter::new(source, rate, channels).into())
}

impl Equalized {
    fn new(input: Box<dyn MediaSource>, preset: Preset) -> Result<Equalized, String> {
        let source = MediaSourceStream::new(input, Default::default());
        let format = get_probe()
            .format(
                &Hint::new(),
                source,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .map_err(|e| format!("couldn't read the stream: {e}"))?
            .format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or("the stream has no audio")?;
        let params = track.codec_params.clone();
        let decoder = get_codec_registry()
            .make(&params, &DecoderOptions::default())
            .map_err(|e| format!("couldn't decode the stream: {e}"))?;
        let rate = params.sample_rate.unwrap_or(48_000);
        let channels = params.channels.map_or(2, |c| c.count());
        Ok(Equalized {
            track: track.id,
            format,
            decoder,
            time_base: params.time_base,
            rate,
            channels,
            chain: Chain::new(preset, rate, channels),
            samples: None,
            out: Vec::new(),
            at: 0,
            skip: 0,
        })
    }
}

/// Decoded, filtered audio as interleaved little-endian f32 PCM, the format RawAdapter takes.
struct Equalized {
    track: u32,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    time_base: Option<TimeBase>,
    rate: u32,
    channels: usize,
    chain: Chain,
    samples: Option<SampleBuffer<f32>>,
    out: Vec<u8>,
    at: usize,
    /// frames still to drop after a seek, which lands at the packet before the target
    skip: u64,
}

impl Equalized {
    /// Decodes and filters the next packet into `out`. False at the end of the stream.
    fn fill(&mut self) -> io::Result<bool> {
        self.out.clear();
        self.at = 0;
        let packet = match self.format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(e)) if e.kind() == ErrorKind::UnexpectedEof => {
                return Ok(false);
            }
            Err(e) => return Err(io::Error::other(e)),
        };
        if packet.track_id() != self.track {
            return Ok(true);
        }
        let decoded = match self.decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // one bad packet is a moment of silence, not the end
            Err(SymphoniaError::DecodeError(_)) => return Ok(true),
            Err(e) => return Err(io::Error::other(e)),
        };
        let spec = *decoded.spec();
        if spec.channels.count() != self.channels {
            return Err(io::Error::other("the stream changed its channel count"));
        }
        let fits = self
            .samples
            .as_ref()
            .is_some_and(|s| s.capacity() >= decoded.capacity() * self.channels);
        if !fits {
            self.samples = Some(SampleBuffer::new(decoded.capacity() as u64, spec));
        }
        let samples = self.samples.as_mut().expect("just made");
        samples.copy_interleaved_ref(decoded);
        let skip = (self.skip as usize).min(samples.len() / self.channels);
        self.skip -= skip as u64;
        let pcm = &mut samples.samples_mut()[skip * self.channels..];
        self.chain.run(pcm);
        self.out.extend(pcm.iter().flat_map(|s| s.to_le_bytes()));
        Ok(true)
    }
}

impl Read for Equalized {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.out.len() {
            if !self.fill()? {
                return Ok(0);
            }
        }
        let n = buf.len().min(self.out.len() - self.at);
        buf[..n].copy_from_slice(&self.out[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl Seek for Equalized {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let SeekFrom::Start(pos) = pos else {
            return Err(ErrorKind::Unsupported.into());
        };
        // RawAdapter passes positions that still count its 16-byte header
        let frame_bytes = 4 * self.channels as u64;
        let frame = pos.saturating_sub(16) / frame_bytes;
        let time = Time::from(frame as f64 / f64::from(self.rate));
        let to = SeekTo::Time {
            time,
            track_id: Some(self.track),
        };
        let seeked = self
            .format
            .seek(SeekMode::Accurate, to)
            .map_err(io::Error::other)?;
        self.decoder.reset();
        self.chain.reset();
        self.out.clear();
        self.at = 0;
        self.skip = self.time_base.map_or(0, |tb| {
            let early = tb.calc_time(seeked.required_ts.saturating_sub(seeked.actual_ts));
            ((early.seconds as f64 + early.frac) * f64::from(self.rate)) as u64
        });
        Ok(frame * frame_bytes)
    }
}

impl MediaSource for Equalized {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How much louder a sine comes out than it went in, in dB.
    fn gain_db(preset: Preset, freq: f64) -> f64 {
        let rate = 48_000;
        let mut chain = Chain::new(preset, rate, 2);
        let mut samples: Vec<f32> = (0..rate as usize * 2)
            .flat_map(|i| {
                let s = (2.0 * PI * freq * i as f64 / f64::from(rate)).sin() as f32 * 0.5;
                [s, s]
            })
            .collect();
        let before: f64 = samples.iter().map(|&s| f64::from(s).powi(2)).sum();
        chain.run(&mut samples);
        // the second half, after the filters settle
        let half = samples.len() / 2;
        let after: f64 = samples[half..].iter().map(|&s| f64::from(s).powi(2)).sum();
        10.0 * (after / (before / 2.0)).log10()
    }

    #[test]
    fn presets_shape_the_sound() {
        let near = |got: f64, want: f64| (got - want).abs() < 0.5;
        assert!(near(gain_db(Preset::Off, 1000.0), 0.0));
        // bass keeps the lows as they are and turns the rest down, so a boost can't clip
        assert!(near(gain_db(Preset::Bass, 30.0), 0.0));
        assert!(near(gain_db(Preset::Bass, 5000.0), -6.0));
        assert!(near(gain_db(Preset::Treble, 15000.0), 0.0));
        assert!(near(gain_db(Preset::Treble, 100.0), -5.0));
        assert!(near(gain_db(Preset::Vocals, 2500.0), 0.0));
        assert!(near(gain_db(Preset::Soft, 15000.0), -5.0));
        assert!(near(gain_db(Preset::Soft, 200.0), 0.0));
        let custom = [
            Preset::Custom([6, 0, 0, 0, 0]),
            Preset::Custom([0, 12, 12, 0, 0]),
            Preset::Custom([12, 12, 12, 12, 12]),
            Preset::Custom([-12, 6, -12, 6, -12]),
        ];
        for preset in Preset::ALL.into_iter().chain(custom) {
            for freq in [
                20.0, 60.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 4000.0, 10000.0, 20000.0,
            ] {
                assert!(gain_db(preset, freq) < 0.3, "{preset:?} boosts {freq} Hz");
            }
        }
        // a custom curve has the asked-for gains at its bands, relative to each other
        for preset in custom {
            let Preset::Custom(gains) = preset else {
                unreachable!()
            };
            let got: Vec<f64> = BANDS
                .iter()
                .map(|(_, freq)| gain_db(preset, *freq))
                .collect();
            for (i, (band, _)) in BANDS.iter().enumerate() {
                let (want, have) = (f64::from(gains[i] - gains[0]), got[i] - got[0]);
                assert!(
                    (have - want).abs() < 0.7,
                    "{preset:?}: {band} {have:.1} dB from bass, not {want}"
                );
            }
        }
    }

    /// A real stream: `MUSICART_WEBM=track.webm cargo test -- --ignored`
    #[test]
    #[ignore]
    fn seeks_land_on_the_sample() {
        let path = std::env::var("MUSICART_WEBM").expect("MUSICART_WEBM");
        let open = || {
            let file = std::fs::File::open(&path).unwrap();
            Equalized::new(Box::new(file), Preset::Off).unwrap()
        };
        let pcm = |source: &mut Equalized, frames: usize| {
            let mut bytes = vec![0; frames * 4 * source.channels];
            source.read_exact(&mut bytes).unwrap();
            bytes
                .chunks(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                .collect::<Vec<_>>()
        };
        let mut whole = open();
        let (rate, channels) = (whole.rate as usize, whole.channels);
        let expected = pcm(&mut whole, 57 * rate);
        let mut seeked = open();
        // as RawAdapter asks: 16 header bytes, then 56 seconds of frames
        let at = seeked
            .seek(SeekFrom::Start(16 + (56 * rate * 4 * channels) as u64))
            .unwrap();
        assert_eq!(at, (56 * rate * 4 * channels) as u64);
        let got = pcm(&mut seeked, rate);
        // the decoder needs a moment to settle after a seek, so compare the last half second
        let tail = rate / 2 * channels;
        let diff = expected[expected.len() - tail..]
            .iter()
            .zip(&got[got.len() - tail..])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(diff < 0.01, "after seeking, samples differ by up to {diff}");
        let mut rest = Vec::new();
        whole.read_to_end(&mut rest).unwrap();
        let seconds = (57 * rate * 4 * channels + rest.len()) as f64 / (rate * 4 * channels) as f64;
        println!(
            "{seconds:.2}s of audio at {rate} Hz, {channels} channels, max diff after seek {diff}"
        );
    }
}
