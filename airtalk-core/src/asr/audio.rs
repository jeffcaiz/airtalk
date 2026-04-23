//! Audio encoding for ASR upload.
//!
//! Two formats are supported:
//!
//! * `Wav` — raw PCM16 LE 16 kHz mono wrapped in a 44-byte RIFF/WAVE
//!   header. Zero encode CPU, zero quality loss, ~32 KB/s on the wire.
//! * `Opus { bitrate_bps }` — libopus (via the `audiopus` crate) at
//!   the requested bitrate, encapsulated in an Ogg container. ~10×
//!   smaller than WAV for speech at 24 kbps with no measurable ASR
//!   accuracy impact.
//!
//! # CLI grammar
//!
//! The `--asr-audio-format` flag accepts:
//!
//! * `wav`             — WAV
//! * `opus`            — Opus @ 24 kbps (default)
//! * `opus:<bitrate>` — Opus at a specific bitrate. Bitrate may be
//!   `16k` / `24k` / `32k` (suffix = *1000) or a literal integer like
//!   `24000` (bits per second).
//!
//! # Opus / Ogg layout
//!
//! A valid Ogg Opus stream is three kinds of pages:
//!
//! 1. Page 0 (BOS): a single `OpusHead` packet (19 bytes) — codec
//!    parameters + `pre_skip` for the decoder.
//! 2. Page 1: a single `OpusTags` packet — vendor string + empty
//!    user comment list.
//! 3. Pages 2..n: audio packets (one 20 ms frame each). We pack up to
//!    50 packets (~1 s) per page to amortize the 27-byte page header.
//!    The last page is flagged EOS.
//!
//! Granule positions are in 48 kHz samples regardless of input rate
//! (Opus's internal rate). `pre_skip` is the encoder's lookahead
//! reported in input-rate samples, scaled by 3 (16 kHz → 48 kHz).

use std::str::FromStr;

use anyhow::{bail, Context, Result};
use audiopus::coder::Encoder;
use audiopus::{Application, Bitrate, Channels, SampleRate};

/// Selected output audio container + codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Wav,
    Opus { bitrate_bps: i32 },
}

impl AudioFormat {
    /// MIME type for the `data:` URI wrapper in ASR requests.
    pub fn mime(self) -> &'static str {
        match self {
            AudioFormat::Wav => "audio/wav",
            AudioFormat::Opus { .. } => "audio/ogg",
        }
    }

    /// Encode PCM16 LE 16 kHz mono input into the chosen format.
    pub fn encode(self, pcm: &[u8]) -> anyhow::Result<Vec<u8>> {
        match self {
            AudioFormat::Wav => Ok(pcm16_to_wav_16k_mono(pcm)),
            AudioFormat::Opus { bitrate_bps } => pcm16_to_opus_ogg_16k_mono(pcm, bitrate_bps),
        }
    }
}

impl FromStr for AudioFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("wav") {
            return Ok(AudioFormat::Wav);
        }
        if s.eq_ignore_ascii_case("opus") {
            return Ok(AudioFormat::Opus {
                bitrate_bps: 24_000,
            });
        }
        if let Some(rest) = s.strip_prefix("opus:").or_else(|| s.strip_prefix("OPUS:")) {
            let bitrate_bps = parse_bitrate(rest)?;
            return Ok(AudioFormat::Opus { bitrate_bps });
        }
        bail!("unknown audio format {s:?}; expected `wav`, `opus`, or `opus:<bitrate>` (e.g. `opus:24k`)");
    }
}

fn parse_bitrate(s: &str) -> anyhow::Result<i32> {
    let s = s.trim();
    let (num_str, multiplier) = match s.as_bytes().last() {
        Some(b'k') | Some(b'K') => (&s[..s.len() - 1], 1000),
        _ => (s, 1),
    };
    let n: i32 = num_str
        .parse()
        .with_context(|| format!("parsing bitrate {s:?}"))?;
    if n <= 0 {
        bail!("bitrate must be positive, got {n}");
    }
    Ok(n * multiplier)
}

// ─── WAV ───────────────────────────────────────────────────────────────

/// Wrap PCM16 LE 16 kHz mono bytes in a 44-byte RIFF/WAVE header.
///
/// Layout:
///
/// ```text
/// 0  "RIFF"              4 bytes
/// 4  file_size - 8       u32 LE
/// 8  "WAVE"              4 bytes
/// 12 "fmt "              4 bytes
/// 16 subchunk1_size = 16 u32 LE
/// 20 audio_format   = 1  u16 LE   (PCM)
/// 22 num_channels   = 1  u16 LE
/// 24 sample_rate = 16000 u32 LE
/// 28 byte_rate   = 32000 u32 LE
/// 32 block_align     = 2 u16 LE
/// 34 bits_per_sample= 16 u16 LE
/// 36 "data"              4 bytes
/// 40 data_size           u32 LE
/// 44 PCM payload
/// ```
fn pcm16_to_wav_16k_mono(pcm: &[u8]) -> Vec<u8> {
    const SAMPLE_RATE: u32 = 16_000;
    const CHANNELS: u16 = 1;
    const BITS_PER_SAMPLE: u16 = 16;
    const BYTE_RATE: u32 = SAMPLE_RATE * (CHANNELS as u32) * (BITS_PER_SAMPLE as u32) / 8;
    const BLOCK_ALIGN: u16 = CHANNELS * BITS_PER_SAMPLE / 8;

    let data_size = pcm.len() as u32;
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32.saturating_add(data_size)).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&CHANNELS.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&BYTE_RATE.to_le_bytes());
    out.extend_from_slice(&BLOCK_ALIGN.to_le_bytes());
    out.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_size.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

// ─── Opus / Ogg ────────────────────────────────────────────────────────

/// Samples per Opus frame at 16 kHz input for 20 ms framing.
const OPUS_FRAME_SAMPLES_16K: usize = 320;
/// Samples per 20 ms at 48 kHz (Opus's internal rate, used for granule
/// positions).
const OPUS_FRAME_SAMPLES_48K: u64 = 960;
/// Per-page cap on audio packets. At 24 kbps × 20 ms ≈ 60 B/packet,
/// 50 packets is ~3 KB/page — well under the 65 kB page max and keeps
/// the lacing table (≤ 255 entries) safe even at higher bitrates.
const PACKETS_PER_PAGE: usize = 50;

/// Encode PCM16 LE 16 kHz mono into an Ogg Opus byte stream.
fn pcm16_to_opus_ogg_16k_mono(pcm: &[u8], bitrate_bps: i32) -> anyhow::Result<Vec<u8>> {
    // PCM bytes → i16 samples (little-endian). A dangling odd byte is
    // dropped; upstream always hands us whole samples so this is a
    // defensive truncation rather than an expected case.
    let samples: Vec<i16> = pcm
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();

    let mut encoder = Encoder::new(SampleRate::Hz16000, Channels::Mono, Application::Voip)
        .context("creating Opus encoder")?;
    encoder
        .set_bitrate(Bitrate::BitsPerSecond(bitrate_bps))
        .with_context(|| format!("setting Opus bitrate to {bitrate_bps} bps"))?;

    // Lookahead comes back in input-rate samples. OpusHead's pre_skip
    // is always expressed at 48 kHz, so scale by 3 (16 kHz → 48 kHz).
    let lookahead_16k = encoder
        .lookahead()
        .context("querying Opus encoder lookahead")?;
    let pre_skip_48k: u16 = lookahead_16k.saturating_mul(3).min(u16::MAX as u32) as u16;

    // Encode in 20 ms frames. The final frame is zero-padded if the
    // PCM length isn't a multiple of 320 samples.
    let mut encoded_packets: Vec<Vec<u8>> = Vec::new();
    let mut out_buf = vec![0u8; 4000];
    for frame in samples.chunks(OPUS_FRAME_SAMPLES_16K) {
        let n = if frame.len() == OPUS_FRAME_SAMPLES_16K {
            encoder
                .encode(frame, &mut out_buf)
                .context("Opus encode failed")?
        } else {
            let mut padded = [0i16; OPUS_FRAME_SAMPLES_16K];
            padded[..frame.len()].copy_from_slice(frame);
            encoder
                .encode(&padded, &mut out_buf)
                .context("Opus encode failed (tail frame)")?
        };
        encoded_packets.push(out_buf[..n].to_vec());
    }

    // Build the Ogg stream.
    let serial = derive_serial();
    let mut ogg = Vec::new();
    let mut seq: u32 = 0;

    // Page 0: OpusHead (BOS).
    let head = build_opus_head(pre_skip_48k);
    write_ogg_page(&mut ogg, OggHeader::Bos, 0, serial, seq, &[&head])?;
    seq += 1;

    // Page 1: OpusTags.
    let tags = build_opus_tags();
    write_ogg_page(&mut ogg, OggHeader::Normal, 0, serial, seq, &[&tags])?;
    seq += 1;

    // Pages 2..: audio packets.
    if encoded_packets.is_empty() {
        // No audio — still need an EOS page to terminate the stream.
        write_ogg_page(&mut ogg, OggHeader::Eos, 0, serial, seq, &[])?;
    } else {
        let total_chunks = encoded_packets.chunks(PACKETS_PER_PAGE).count();
        let mut granule_48k: u64 = 0;
        for (idx, packets) in encoded_packets.chunks(PACKETS_PER_PAGE).enumerate() {
            granule_48k += (packets.len() as u64) * OPUS_FRAME_SAMPLES_48K;
            let header = if idx + 1 == total_chunks {
                OggHeader::Eos
            } else {
                OggHeader::Normal
            };
            let refs: Vec<&[u8]> = packets.iter().map(|p| p.as_slice()).collect();
            write_ogg_page(&mut ogg, header, granule_48k, serial, seq, &refs)?;
            seq += 1;
        }
    }

    Ok(ogg)
}

#[derive(Clone, Copy)]
enum OggHeader {
    Normal = 0x00,
    Bos = 0x02,
    Eos = 0x04,
}

fn build_opus_head(pre_skip_48k: u16) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1); // version
    head.push(1); // channel count
    head.extend_from_slice(&pre_skip_48k.to_le_bytes());
    head.extend_from_slice(&16_000u32.to_le_bytes()); // input sample rate (informational)
    head.extend_from_slice(&0u16.to_le_bytes()); // output gain (Q7.8 fixed-point)
    head.push(0); // channel mapping family 0 (mono/stereo)
    head
}

fn build_opus_tags() -> Vec<u8> {
    const VENDOR: &str = "airtalk";
    let mut tags = Vec::with_capacity(8 + 4 + VENDOR.len() + 4);
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(VENDOR.len() as u32).to_le_bytes());
    tags.extend_from_slice(VENDOR.as_bytes());
    tags.extend_from_slice(&0u32.to_le_bytes()); // user comment list length
    tags
}

/// Append one Ogg page containing the given packets.
///
/// The segment table uses standard Ogg lacing: each packet contributes
/// `size / 255` full-255-byte segments followed by one terminator
/// segment of `size % 255` bytes (possibly 0). CRC is computed over
/// the fully-built page with the CRC field treated as zero during
/// computation.
fn write_ogg_page(
    out: &mut Vec<u8>,
    header: OggHeader,
    granule_48k: u64,
    serial: u32,
    seq: u32,
    packets: &[&[u8]],
) -> Result<()> {
    let mut segments: Vec<u8> = Vec::new();
    for packet in packets {
        let size = packet.len();
        let full = size / 255;
        let remainder = size % 255;
        segments.extend(std::iter::repeat_n(255, full));
        // Always emit the remainder byte (0 if packet size is a
        // multiple of 255) — Ogg requires an explicit terminator.
        segments.push(remainder as u8);
    }
    anyhow::ensure!(
        segments.len() <= 255,
        "ogg page has >255 segments ({}); caller must split packets across pages",
        segments.len()
    );

    let page_start = out.len();
    out.extend_from_slice(b"OggS");
    out.push(0); // stream structure version
    out.push(header as u8);
    out.extend_from_slice(&granule_48k.to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    let crc_offset = out.len();
    out.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
    out.push(segments.len() as u8);
    out.extend_from_slice(&segments);
    for packet in packets {
        out.extend_from_slice(packet);
    }

    let crc = OGG_CRC.checksum(&out[page_start..]);
    out[crc_offset..crc_offset + 4].copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

/// Derive a u32 stream serial number. Only needs to be unique within
/// the enclosing bytestream, which is a single-stream file for us —
/// so monotonically-nonzero is good enough.
fn derive_serial() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| {
            // Mix seconds and nanos so two back-to-back calls in the
            // same millisecond still differ.
            (d.as_secs() as u32)
                .wrapping_mul(31)
                .wrapping_add(d.subsec_nanos())
        })
        .unwrap_or(1)
}

/// Ogg uses CRC-32 with polynomial 0x04C11DB7, initial value 0, no
/// input/output reflection, no final XOR.
static OGG_CRC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::Algorithm {
    width: 32,
    poly: 0x04C11DB7,
    init: 0,
    refin: false,
    refout: false,
    xorout: 0,
    check: 0x89a1897f,
    residue: 0,
});

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wav() {
        assert_eq!(AudioFormat::from_str("wav").unwrap(), AudioFormat::Wav);
        assert_eq!(AudioFormat::from_str("WAV").unwrap(), AudioFormat::Wav);
    }

    #[test]
    fn parse_opus_default() {
        assert_eq!(
            AudioFormat::from_str("opus").unwrap(),
            AudioFormat::Opus {
                bitrate_bps: 24_000
            }
        );
    }

    #[test]
    fn parse_opus_with_bitrate() {
        assert_eq!(
            AudioFormat::from_str("opus:16k").unwrap(),
            AudioFormat::Opus {
                bitrate_bps: 16_000
            }
        );
        assert_eq!(
            AudioFormat::from_str("opus:24k").unwrap(),
            AudioFormat::Opus {
                bitrate_bps: 24_000
            }
        );
        assert_eq!(
            AudioFormat::from_str("opus:32000").unwrap(),
            AudioFormat::Opus {
                bitrate_bps: 32_000
            }
        );
    }

    #[test]
    fn parse_unknown_format_errors() {
        assert!(AudioFormat::from_str("mp3").is_err());
        assert!(AudioFormat::from_str("opus:").is_err());
        assert!(AudioFormat::from_str("opus:abc").is_err());
        assert!(AudioFormat::from_str("opus:-1").is_err());
    }

    #[test]
    fn wav_header_is_correct() {
        let pcm: Vec<u8> = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        let wav = AudioFormat::Wav.encode(&pcm).unwrap();
        assert_eq!(wav.len(), 44 + pcm.len());
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        let sr = u32::from_le_bytes(wav[24..28].try_into().unwrap());
        assert_eq!(sr, 16_000);
    }

    #[test]
    fn opus_output_starts_with_ogg_and_opushead() {
        // 1 second of silence (16000 samples * 2 bytes).
        let pcm = vec![0u8; 32_000];
        let opus = AudioFormat::Opus {
            bitrate_bps: 24_000,
        }
        .encode(&pcm)
        .unwrap();

        // First page: OggS + OpusHead inside.
        assert_eq!(&opus[0..4], b"OggS", "first page magic");
        // header_type at byte 5 should be 0x02 (BOS)
        assert_eq!(opus[5], 0x02, "first page header_type BOS");
        // OpusHead magic should appear shortly after the page header
        // (page header = 27 + n_segments bytes). Find it.
        let head_pos = opus
            .windows(8)
            .position(|w| w == b"OpusHead")
            .expect("OpusHead not found");
        assert!(head_pos < 60, "OpusHead should be in the first page");
    }

    #[test]
    fn opus_stream_has_tags_and_eos() {
        let pcm = vec![0u8; 32_000];
        let opus = AudioFormat::Opus {
            bitrate_bps: 16_000,
        }
        .encode(&pcm)
        .unwrap();

        // OpusTags header must appear as the second page's packet.
        assert!(
            opus.windows(8).any(|w| w == b"OpusTags"),
            "OpusTags not found in Opus stream"
        );

        // The stream must contain at least one EOS page (header_type 0x04).
        let mut found_eos = false;
        let mut i = 0;
        while i + 27 <= opus.len() {
            if &opus[i..i + 4] == b"OggS" {
                let header_type = opus[i + 5];
                if header_type & 0x04 != 0 {
                    found_eos = true;
                }
                let n_segments = opus[i + 26] as usize;
                if i + 27 + n_segments > opus.len() {
                    break;
                }
                let payload: usize = opus[i + 27..i + 27 + n_segments]
                    .iter()
                    .map(|&b| b as usize)
                    .sum();
                i += 27 + n_segments + payload;
            } else {
                i += 1;
            }
        }
        assert!(found_eos, "no EOS page in Opus stream");
    }

    #[test]
    fn opus_roundtrip_via_decoder() {
        // Non-silent input so the encoder produces non-trivial packets.
        // A 440 Hz sine wave at 16 kHz for 0.5 s.
        let samples: Vec<i16> = (0..8_000)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                (f32::sin(2.0 * std::f32::consts::PI * 440.0 * t) * 8_000.0) as i16
            })
            .collect();
        let pcm: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();

        let opus = AudioFormat::Opus {
            bitrate_bps: 24_000,
        }
        .encode(&pcm)
        .unwrap();

        // Ballpark: 0.5 s at 24 kbps ≈ 1.5 KB raw opus, + Ogg headers
        // + ~25 pages × 28 B (we pack 50 packets/page so only 1 audio page).
        // Hard guarantee: output is smaller than the equivalent WAV.
        let wav_size = 44 + pcm.len();
        assert!(
            opus.len() < wav_size / 3,
            "Opus output ({} B) should be much smaller than WAV ({} B)",
            opus.len(),
            wav_size
        );

        // Can we decode the audio packets back? Skip the header pages
        // and find the first audio page (header_type 0x00 or 0x04) with
        // a non-zero granule position.
        use audiopus::coder::Decoder;
        let mut decoder = Decoder::new(SampleRate::Hz16000, Channels::Mono).expect("Opus decoder");

        // Find any 'OggS' page past the first two and feed its first
        // packet to the decoder — a successful decode proves the pipe
        // produced valid Opus bytes.
        let mut audio_packet: Option<Vec<u8>> = None;
        let mut i = 0;
        let mut page_idx = 0;
        while i + 27 <= opus.len() {
            if &opus[i..i + 4] == b"OggS" {
                let n_segments = opus[i + 26] as usize;
                let lacing = &opus[i + 27..i + 27 + n_segments];
                let payload_start = i + 27 + n_segments;
                if page_idx >= 2 && !lacing.is_empty() {
                    // First packet spans lacing segments until one < 255.
                    let mut pkt_len = 0usize;
                    for &seg in lacing {
                        pkt_len += seg as usize;
                        if seg < 255 {
                            break;
                        }
                    }
                    audio_packet = Some(opus[payload_start..payload_start + pkt_len].to_vec());
                    break;
                }
                let payload_total: usize = lacing.iter().map(|&b| b as usize).sum();
                i = payload_start + payload_total;
                page_idx += 1;
            } else {
                i += 1;
            }
        }

        let packet_bytes = audio_packet.expect("no audio packet found in Opus stream");
        let mut decoded = vec![0i16; 5_760]; // 120 ms max at 48 kHz
        let packet = audiopus::packet::Packet::try_from(packet_bytes.as_slice())
            .expect("wrapping decoded bytes as Packet");
        let signals = audiopus::MutSignals::try_from(decoded.as_mut_slice())
            .expect("wrapping output buf as MutSignals");
        let decoded_samples = decoder
            .decode(Some(packet), signals, false)
            .expect("decoding the first audio packet");
        assert!(decoded_samples > 0, "decoder produced no samples");
    }

    #[test]
    fn mime_types() {
        assert_eq!(AudioFormat::Wav.mime(), "audio/wav");
        assert_eq!(
            AudioFormat::Opus {
                bitrate_bps: 24_000
            }
            .mime(),
            "audio/ogg"
        );
    }
}
