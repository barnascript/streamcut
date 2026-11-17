//! One number per second for "how much is happening here".
//!
//! I score each second from four things I can measure:
//!   loud   - the sound jumps above the minute around it (crowd, shouting)
//!   speech - people talk faster than usual
//!   hype   - someone says a phrase that usually marks a moment ("no way",
//!            "we have liftoff") or whisper hears [APPLAUSE] / [LAUGHTER]
//!   cuts   - the picture changes a lot (replays, camera switches)
//! Each one is turned into "how unusual is this for this video", so a quiet
//! podcast and a loud esports stream are both judged against themselves.

use crate::transcript::Word;
use serde::Serialize;

#[derive(Clone, Copy, Debug)]
pub struct Weights {
    pub loud: f32,
    pub speech: f32,
    pub hype: f32,
    pub cuts: f32,
}

impl Weights {
    pub fn for_input(has_transcript: bool) -> Self {
        if has_transcript {
            Weights {
                loud: 0.40,
                speech: 0.20,
                hype: 0.30,
                cuts: 0.10,
            }
        } else {
            Weights {
                loud: 0.75,
                speech: 0.0,
                hype: 0.0,
                cuts: 0.25,
            }
        }
    }
}

/// A phrase that someone said, found at a point in time.
#[derive(Clone, Debug, Serialize)]
pub struct Hit {
    pub at: f64,
    pub phrase: String,
    pub weight: f32,
}

#[derive(Debug, Default, Serialize)]
pub struct Signals {
    pub loud_db: Vec<f32>,
    pub baseline_db: Vec<f32>,
    pub words_per_s: Vec<f32>,
    pub loud: Vec<f32>,
    pub speech: Vec<f32>,
    pub hype: Vec<f32>,
    pub cuts: Vec<f32>,
    pub score: Vec<f32>,
    #[serde(skip)]
    pub hits: Vec<Hit>,
    pub median_wps: f32,
}

/// Phrases that tend to mark a moment worth clipping, with a weight.
/// Kept short on purpose: every entry here was checked against real streams.
pub const PHRASES: &[(&str, f32)] = &[
    ("oh my god", 3.0),
    ("oh my gosh", 2.5),
    ("no way", 2.5),
    ("let's go", 2.5),
    ("here we go", 2.0),
    ("look at that", 2.0),
    ("there you see it", 1.5),
    ("did you see that", 2.5),
    ("what just happened", 3.0),
    ("are you kidding", 2.5),
    ("we have liftoff", 3.5),
    ("liftoff", 3.0),
    ("lift off", 3.0),
    ("go for launch", 2.5),
    ("no go", 2.0),
    ("no-go", 2.0),
    ("scrub", 2.5),
    ("scrubbed", 2.5),
    ("abort", 2.5),
    ("touchdown", 2.5),
    ("goal", 2.0),
    ("clutch", 2.0),
    ("unbelievable", 2.5),
    ("incredible", 2.0),
    ("insane", 2.0),
    ("crazy", 1.5),
    ("wow", 2.0),
    ("whoa", 2.0),
    ("holy", 2.0),
    ("amazing", 1.5),
    ("haha", 2.0),
    ("hahaha", 2.5),
    ("first time ever", 2.0),
    ("breaking news", 1.5),
];

/// Sounds whisper writes in brackets.
const SOUND_TAGS: &[(&str, f32)] = &[
    ("applause", 2.5),
    ("cheering", 3.0),
    ("cheers", 2.5),
    ("laughter", 2.5),
    ("laughing", 2.5),
    ("laughs", 2.5),
    ("crowd", 2.0),
    ("screaming", 2.5),
    ("gasps", 2.0),
];

pub fn rolling_median(v: &[f32], half: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(v.len());
    let mut buf: Vec<f32> = Vec::with_capacity(half * 2 + 1);
    for i in 0..v.len() {
        let a = i.saturating_sub(half);
        let b = (i + half + 1).min(v.len());
        buf.clear();
        buf.extend(v[a..b].iter().copied().filter(|x| *x > -70.0));
        if buf.is_empty() {
            out.push(v[i]);
            continue;
        }
        buf.sort_by(|x, y| x.total_cmp(y));
        out.push(buf[buf.len() / 2]);
    }
    out
}

fn median(v: &mut [f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

/// Robust spread (median absolute deviation scaled to match a std dev).
fn mad(values: &[f32], center: f32) -> f32 {
    let mut d: Vec<f32> = values.iter().map(|x| (x - center).abs()).collect();
    1.4826 * median(&mut d)
}

/// Finds the phrases in PHRASES and SOUND_TAGS in a list of words.
pub fn find_hits(words: &[Word]) -> Vec<Hit> {
    let norms: Vec<String> = words.iter().map(|w| w.norm()).collect();
    let mut hits = Vec::new();
    for (i, w) in words.iter().enumerate() {
        if w.is_sound_tag() {
            let n = norms[i].as_str();
            if let Some((tag, weight)) = SOUND_TAGS.iter().find(|(t, _)| n.contains(t)) {
                hits.push(Hit {
                    at: w.start,
                    phrase: format!("[{tag}]"),
                    weight: *weight,
                });
            }
            continue;
        }
        let mut best: Option<(&str, f32)> = None;
        for (phrase, weight) in PHRASES {
            let parts: Vec<&str> = phrase.split(' ').collect();
            if i + parts.len() > norms.len() {
                continue;
            }
            // Letters and digits only on both sides: "Let's" = "lets", "no-go" = "nogo".
            let matched = parts.iter().enumerate().all(|(k, p)| {
                let n: String = norms[i + k]
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect();
                let p: String = p
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .flat_map(|c| c.to_lowercase())
                    .collect();
                n == p
            });
            if matched && best.map(|(_, bw)| *weight > bw).unwrap_or(true) {
                best = Some((phrase, *weight));
            }
        }
        if let Some((phrase, weight)) = best {
            hits.push(Hit {
                at: w.start,
                phrase: phrase.to_string(),
                weight,
            });
        } else if w.text.ends_with('!') {
            hits.push(Hit {
                at: w.start,
                phrase: "!".into(),
                weight: 0.6,
            });
        }
    }
    hits
}

pub fn build(loud_db: &[f32], words: &[Word], cut_times: &[f64], weights: Weights) -> Signals {
    let n = loud_db.len();
    if n == 0 {
        return Signals::default();
    }

    // Loud: compare each second with the minute around it.
    let baseline = rolling_median(loud_db, 30);
    let diffs: Vec<f32> = loud_db
        .iter()
        .zip(&baseline)
        .filter(|(d, _)| **d > -70.0)
        .map(|(d, b)| d - b)
        .collect();
    let spread = mad(&diffs, 0.0).max(1.5);
    let loud: Vec<f32> = loud_db
        .iter()
        .zip(&baseline)
        .map(|(d, b)| {
            if *d <= -70.0 {
                -2.0
            } else {
                ((d - b) / spread).clamp(-3.0, 5.0)
            }
        })
        .collect();

    // Speech: words a second over a 5 s window.
    let mut starts = vec![0f32; n];
    for w in words.iter().filter(|w| w.is_spoken()) {
        let i = w.start.floor() as usize;
        if i < n {
            starts[i] += 1.0;
        }
    }
    let words_per_s: Vec<f32> = (0..n)
        .map(|i| {
            let a = i.saturating_sub(2);
            let b = (i + 3).min(n);
            starts[a..b].iter().sum::<f32>() / 5.0
        })
        .collect();
    let mut talking: Vec<f32> = words_per_s.iter().copied().filter(|x| *x > 0.4).collect();
    let median_wps = median(&mut talking);
    let wps_spread = mad(&talking, median_wps).max(0.35);
    let speech: Vec<f32> = words_per_s
        .iter()
        .map(|w| {
            if *w <= 0.0 || words.is_empty() {
                0.0
            } else {
                ((w - median_wps) / wps_spread).clamp(-2.0, 4.0)
            }
        })
        .collect();

    // Hype: spread each phrase over a few seconds so a window around it scores.
    let hits = find_hits(words);
    let mut hype = vec![0f32; n];
    for h in &hits {
        let c = h.at.floor() as i64;
        for d in -3i64..=4 {
            let i = c + d;
            if i >= 0 && (i as usize) < n {
                let fall = 1.0 - (d.abs() as f32) / 5.0;
                hype[i as usize] += h.weight * fall;
            }
        }
    }
    for x in hype.iter_mut() {
        *x = x.min(4.0);
    }

    // Cuts: scene changes in the 10 s around each second.
    let mut cut_count = vec![0f32; n];
    for &t in cut_times {
        let i = t.floor() as usize;
        if i < n {
            cut_count[i] += 1.0;
        }
    }
    let density: Vec<f32> = (0..n)
        .map(|i| cut_count[i.saturating_sub(5)..(i + 5).min(n)].iter().sum())
        .collect();
    let mut nonzero: Vec<f32> = density.clone();
    let med_cuts = median(&mut nonzero);
    let cuts: Vec<f32> = density
        .iter()
        .map(|d| ((d - med_cuts) / 1.5).clamp(0.0, 3.0))
        .collect();

    let score: Vec<f32> = (0..n)
        .map(|i| {
            weights.loud * loud[i]
                + weights.speech * speech[i]
                + weights.hype * hype[i]
                + weights.cuts * cuts[i]
        })
        .collect();

    Signals {
        loud_db: loud_db.to_vec(),
        baseline_db: baseline,
        words_per_s,
        loud,
        speech,
        hype,
        cuts,
        score,
        hits,
        median_wps,
    }
}

/// Shrinks a series to at most `max` points (by averaging) for the chart.
pub fn downsample(v: &[f32], max: usize) -> (Vec<f32>, f64) {
    if v.len() <= max || max == 0 {
        return (v.to_vec(), 1.0);
    }
    let step = (v.len() as f64 / max as f64).ceil() as usize;
    let out = v
        .chunks(step)
        .map(|c| c.iter().sum::<f32>() / c.len() as f32)
        .collect();
    (out, step as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(start: f64, text: &str) -> Word {
        Word {
            start,
            end: start + 0.3,
            text: text.into(),
        }
    }

    #[test]
    fn finds_multi_word_phrases() {
        let words = vec![
            w(1.0, "Oh"),
            w(1.2, "my"),
            w(1.4, "God,"),
            w(2.0, "look"),
            w(2.2, "at"),
            w(2.4, "that."),
        ];
        let hits = find_hits(&words);
        let p: Vec<&str> = hits.iter().map(|h| h.phrase.as_str()).collect();
        assert_eq!(p, vec!["oh my god", "look at that"]);
        assert_eq!(hits[0].at, 1.0);
    }

    #[test]
    fn matches_with_or_without_apostrophe() {
        let hits = find_hits(&[w(0.0, "Lets"), w(0.2, "go!")]);
        assert_eq!(hits[0].phrase, "let's go");
        let hits = find_hits(&[w(0.0, "Let's"), w(0.2, "go")]);
        assert_eq!(hits[0].phrase, "let's go");
    }

    #[test]
    fn finds_sound_tags() {
        let hits = find_hits(&[w(5.0, "[APPLAUSE]")]);
        assert_eq!(hits[0].phrase, "[applause]");
    }

    #[test]
    fn hyphenated_no_go_counts() {
        let hits = find_hits(&[w(0.0, "a"), w(0.5, "no-go")]);
        assert_eq!(hits[0].phrase, "no-go");
        let hits = find_hits(&[w(0.0, "no"), w(0.5, "go")]);
        assert_eq!(hits[0].phrase, "no go");
    }

    #[test]
    fn a_loud_burst_scores_above_its_surroundings() {
        let mut db = vec![-30.0f32; 120];
        for x in db[60..65].iter_mut() {
            *x = -12.0;
        }
        let s = build(&db, &[], &[], Weights::for_input(false));
        let best = s
            .score
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert!((60..65).contains(&best), "best second was {best}");
        assert!(s.loud[62] > 3.0);
        assert!(s.loud[10].abs() < 0.5);
    }

    #[test]
    fn silence_is_penalised_not_ignored() {
        let mut db = vec![-30.0f32; 100];
        for x in db[40..50].iter_mut() {
            *x = -90.0;
        }
        let s = build(&db, &[], &[], Weights::for_input(false));
        assert!(s.score[45] < s.score[10]);
    }

    #[test]
    fn fast_talking_raises_speech() {
        let db = vec![-25.0f32; 60];
        let mut words = Vec::new();
        // 2 words/s normally, 6 words/s between 30 and 36 s.
        for i in 0..60 {
            let per = if (30..36).contains(&i) { 6 } else { 2 };
            for k in 0..per {
                words.push(w(i as f64 + k as f64 / per as f64, "word"));
            }
        }
        let s = build(&db, &words, &[], Weights::for_input(true));
        assert!(s.speech[33] > 2.0, "{}", s.speech[33]);
        assert!(s.speech[10].abs() < 0.5);
    }

    #[test]
    fn downsample_keeps_short_series() {
        let (v, step) = downsample(&[1.0, 2.0, 3.0], 10);
        assert_eq!(v, vec![1.0, 2.0, 3.0]);
        assert_eq!(step, 1.0);
        let (v, step) = downsample(&[1.0, 3.0, 5.0, 7.0], 2);
        assert_eq!(v, vec![2.0, 6.0]);
        assert_eq!(step, 2.0);
    }
}
