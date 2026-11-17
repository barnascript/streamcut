//! Reads the 16 kHz mono WAV that ffmpeg writes for us and turns it into
//! loudness numbers. I do this in Rust instead of parsing ffmpeg's text output:
//! it takes about a second for a 4-hour stream and it is easy to test.

use anyhow::{Result, bail};

pub struct Pcm {
    pub rate: u32,
    pub samples: Vec<i16>,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Parses a 16-bit PCM mono WAV. It walks the chunks, so a LIST chunk before
/// the data (ffmpeg adds one) is fine.
pub fn read_wav(bytes: &[u8]) -> Result<Pcm> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("not a WAV file");
    }
    let mut pos = 12;
    let mut rate = 0u32;
    let mut fmt_ok = false;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32_at(bytes, pos + 4) as usize;
        let start = pos + 8;
        let end = start.saturating_add(len).min(bytes.len());
        match id {
            b"fmt " => {
                let b = &bytes[start..end];
                if b.len() < 16 {
                    bail!("WAV fmt chunk is too short");
                }
                let format = u16_at(b, 0);
                let channels = u16_at(b, 2);
                rate = u32_at(b, 4);
                let bits = u16_at(b, 14);
                if format != 1 || channels != 1 || bits != 16 {
                    bail!(
                        "expected 16-bit mono PCM, got format {format}, {channels} channels, {bits} bits"
                    );
                }
                fmt_ok = true;
            }
            b"data" => {
                if !fmt_ok {
                    bail!("WAV data chunk came before fmt chunk");
                }
                let samples = bytes[start..end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                    .collect();
                return Ok(Pcm { rate, samples });
            }
            _ => {}
        }
        pos = start.saturating_add(len).saturating_add(len & 1);
    }
    bail!("WAV has no data chunk")
}

pub const FLOOR_DB: f32 = -90.0;

/// Loudness (RMS in dBFS) for each slice of `frame_s` seconds.
pub fn levels(pcm: &Pcm, frame_s: f64) -> Vec<f32> {
    let n = ((pcm.rate as f64) * frame_s).round().max(1.0) as usize;
    pcm.samples
        .chunks(n)
        .map(|chunk| {
            let sum: f64 = chunk.iter().map(|&s| (s as f64) * (s as f64)).sum();
            let mean = sum / chunk.len() as f64;
            if mean <= 0.0 {
                return FLOOR_DB;
            }
            let db = 10.0 * (mean / (32768.0 * 32768.0)).log10();
            (db as f32).max(FLOOR_DB)
        })
        .collect()
}

pub fn percentile(values: &[f32], p: f64) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f32> = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let idx = ((v.len() - 1) as f64 * p.clamp(0.0, 1.0)).round() as usize;
    v[idx]
}

/// Quiet gaps, as (start, end) in seconds. The threshold follows the noise
/// floor of the file, because a stream with music under it never drops to
/// true silence.
pub fn silences(levels: &[f32], frame_s: f64, min_len_s: f64) -> Vec<(f64, f64)> {
    let threshold = (percentile(levels, 0.10) + 8.0).clamp(-60.0, -28.0);
    let mut out = Vec::new();
    let mut run_start: Option<usize> = None;
    for (i, &db) in levels.iter().chain(std::iter::once(&0.0)).enumerate() {
        let quiet = i < levels.len() && db < threshold;
        match (quiet, run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(s)) => {
                let (a, b) = (s as f64 * frame_s, i as f64 * frame_s);
                if b - a >= min_len_s {
                    out.push((a, b));
                }
                run_start = None;
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(samples: &[i16], rate: u32) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + 12 + data.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVE");
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        // An odd-sized chunk ffmpeg-style metadata, to test padding.
        b.extend_from_slice(b"LIST");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(b"abc\0");
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&data);
        b
    }

    #[test]
    fn reads_wav_with_extra_chunks() {
        let pcm = read_wav(&wav(&[1, -2, 3], 16000)).unwrap();
        assert_eq!(pcm.rate, 16000);
        assert_eq!(pcm.samples, vec![1, -2, 3]);
    }

    #[test]
    fn rejects_stereo() {
        let mut b = wav(&[0, 0], 16000);
        b[22] = 2; // channels
        assert!(read_wav(&b).is_err());
    }

    #[test]
    fn full_scale_square_wave_is_zero_db() {
        let samples: Vec<i16> = (0..1600)
            .map(|i| if i % 2 == 0 { 32767 } else { -32767 })
            .collect();
        let pcm = Pcm {
            rate: 1600,
            samples,
        };
        let l = levels(&pcm, 1.0);
        assert_eq!(l.len(), 1);
        assert!(l[0] > -0.1 && l[0] <= 0.0, "{}", l[0]);
    }

    #[test]
    fn digital_silence_hits_the_floor() {
        let pcm = Pcm {
            rate: 100,
            samples: vec![0; 300],
        };
        assert_eq!(levels(&pcm, 1.0), vec![FLOOR_DB; 3]);
    }

    #[test]
    fn finds_quiet_gaps() {
        // loud 1 s, quiet 0.5 s, loud 1 s, quiet 0.1 s (too short), loud.
        let mut l = vec![-20.0; 10];
        l.extend(vec![-70.0; 5]);
        l.extend(vec![-20.0; 10]);
        l.push(-70.0);
        l.extend(vec![-20.0; 10]);
        let s = silences(&l, 0.1, 0.3);
        assert_eq!(s.len(), 1);
        assert!((s[0].0 - 1.0).abs() < 1e-9 && (s[0].1 - 1.5).abs() < 1e-9);
    }
}
