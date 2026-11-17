//! Picks the clips: the best windows that do not overlap, with edges moved
//! to the start and end of a sentence so nobody gets cut off mid-word.

use crate::signals::Signals;
use crate::transcript::{Segment, Word};
use serde::Serialize;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub count: usize,
    pub min_len: usize,
    pub max_len: usize,
    /// Seconds kept free between two clips.
    pub gap: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pick {
    pub start: f64,
    pub end: f64,
    pub score: f32,
    /// The single best second inside the clip. Used for the thumbnail.
    pub peak: f64,
}

/// Seconds of build-up kept before the busy part, and a beat after it.
/// A moment with no setup makes no sense to someone scrolling past.
const LEAD_IN: usize = 6;
const TAIL: usize = 3;

fn moving_avg(v: &[f32], half: usize) -> Vec<f32> {
    let n = v.len();
    (0..n)
        .map(|i| {
            let (a, b) = (i.saturating_sub(half), (i + half + 1).min(n));
            v[a..b].iter().sum::<f32>() / (b - a) as f32
        })
        .collect()
}

/// Average of the clip plus a bonus for its best few seconds, so a short
/// sharp moment can beat a long, mildly busy one.
fn clip_score(score: &[f32], s: usize, e: usize) -> f32 {
    let w = &score[s..e];
    let mean = w.iter().sum::<f32>() / w.len() as f32;
    let mut sorted = w.to_vec();
    sorted.sort_by(|a, b| b.total_cmp(a));
    let top = sorted.iter().take(3).sum::<f32>() / sorted.len().min(3) as f32;
    mean + 0.35 * top
}

/// Grows or trims [s, e) to fit the length limits while keeping the peak in.
/// When trimming, I keep more before the peak than after it.
fn fit(
    mut s: usize,
    mut e: usize,
    peak: usize,
    lo: usize,
    hi: usize,
    min_len: usize,
    max_len: usize,
) -> (usize, usize) {
    if e - s > max_len {
        let before = (max_len * 6 / 10).min(peak - s);
        s = peak - before;
        e = s + max_len;
    }
    while e - s < min_len && (s > lo || e < hi) {
        if s > lo && (peak - s <= e - peak || e >= hi) {
            s -= 1;
        } else if e < hi {
            e += 1;
        }
    }
    (s.max(lo), e.min(hi))
}

/// The editor's way: find the busiest second, grow out while the action
/// stays above 40% of that peak, add the build-up, fit to length, repeat.
pub fn pick_windows(score: &[f32], opts: Options) -> Vec<Pick> {
    let n = score.len();
    if n == 0 || opts.count == 0 {
        return vec![];
    }
    let min_len = opts.min_len.max(1).min(n);
    let max_len = opts.max_len.max(min_len).min(n);
    let sm = moving_avg(score, 2);
    let mut blocked = vec![false; n];
    let mut out: Vec<Pick> = Vec::new();

    while out.len() < opts.count {
        let Some(peak) = (0..n)
            .filter(|i| !blocked[*i])
            .max_by(|a, b| sm[*a].total_cmp(&sm[*b]).then(b.cmp(a)))
        else {
            break;
        };
        let top = sm[peak];
        // Only keep weak moments when nothing better exists at all.
        if top < 0.15 && !out.is_empty() {
            break;
        }
        // The free stretch around the peak (other clips + their gap are off limits).
        let lo = (0..peak)
            .rev()
            .find(|i| blocked[*i])
            .map(|i| i + 1)
            .unwrap_or(0);
        let hi = (peak..n).find(|i| blocked[*i]).unwrap_or(n);
        if hi - lo < min_len {
            blocked[peak] = true;
            continue;
        }
        let (mut s, mut e) = (peak, peak + 1);
        if top > 0.0 {
            let floor = 0.4 * top;
            while s > lo && sm[s - 1] >= floor && e - s < max_len {
                s -= 1;
            }
            while e < hi && sm[e] >= floor && e - s < max_len {
                e += 1;
            }
        }
        s = s.saturating_sub(LEAD_IN).max(lo);
        e = (e + TAIL).min(hi);
        let (s, e) = fit(s, e, peak, lo, hi, min_len, max_len);
        for b in blocked[s.saturating_sub(opts.gap)..(e + opts.gap).min(n)].iter_mut() {
            *b = true;
        }
        out.push(Pick {
            start: s as f64,
            end: e as f64,
            score: clip_score(score, s, e),
            peak: peak as f64 + 0.5,
        });
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

/// The edge nearest to `target`. Edges on the `lean` side count as closer:
/// starting a little early keeps the setup, ending a little late keeps the
/// reaction.
fn closest(
    target: f64,
    options: impl Iterator<Item = f64>,
    lo: f64,
    hi: f64,
    lean: f64,
) -> Option<f64> {
    let cost = |t: f64| {
        let d = (t - target).abs();
        if (t - target) * lean > 0.0 {
            d * 0.6
        } else {
            d
        }
    };
    options
        .filter(|t| *t >= lo && *t <= hi)
        .min_by(|a, b| cost(*a).total_cmp(&cost(*b)))
}

/// Moves clip edges to sentence edges (or to quiet gaps when there is no
/// transcript). If no good edge is close, the edge stays where it was.
pub fn snap(
    pick: &Pick,
    segments: &[Segment],
    silences: &[(f64, f64)],
    opts: Options,
    duration: f64,
) -> (f64, f64) {
    let (mut start, mut end) = (pick.start, pick.end);
    if !segments.is_empty() {
        if let Some(s) = closest(
            start,
            segments.iter().map(|s| s.start),
            start - 6.0,
            start + 4.0,
            -1.0,
        ) {
            start = s;
        }
        if let Some(e) = closest(
            end,
            segments.iter().map(|s| s.end),
            end - 4.0,
            end + 6.0,
            1.0,
        ) {
            end = e;
        }
    } else if !silences.is_empty() {
        let mids = || silences.iter().map(|(a, b)| (a + b) / 2.0);
        if let Some(s) = closest(start, mids(), start - 3.0, start + 3.0, -1.0) {
            start = s;
        }
        if let Some(e) = closest(end, mids(), end - 3.0, end + 3.0, 1.0) {
            end = e;
        }
    }
    // Never lose the peak, and stay close to the length the user asked for.
    let len = end - start;
    let ok = len >= opts.min_len as f64 - 3.0
        && len <= opts.max_len as f64 + 6.0
        && start <= pick.peak
        && end >= pick.peak;
    if !ok {
        start = pick.start;
        end = pick.end;
    }
    // A little air on both sides feels less abrupt.
    ((start - 0.25).max(0.0), (end + 0.4).min(duration))
}

fn clock(t: f64) -> String {
    let t = t.max(0.0) as u64;
    if t >= 3600 {
        format!("{}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
    } else {
        format!("{}:{:02}", t / 60, t % 60)
    }
}

/// Plain-English reasons, shown next to each clip.
pub fn reasons(start: f64, end: f64, sig: &Signals) -> Vec<String> {
    let n = sig.score.len();
    let (a, b) = (start.floor() as usize, (end.ceil() as usize).min(n));
    if a >= b {
        return vec![];
    }
    let mut out = Vec::new();

    let jump = (a..b)
        .map(|i| sig.loud_db[i] - sig.baseline_db[i])
        .fold(f32::MIN, f32::max);
    if jump >= 4.0 {
        out.push(format!(
            "Sound jumps {jump:.0} dB above the minute around it"
        ));
    }

    let talk = sig.words_per_s[a..b].iter().sum::<f32>() / (b - a) as f32;
    if sig.median_wps > 0.0 && talk > sig.median_wps * 1.15 {
        out.push(format!(
            "Faster talking: {talk:.1} words a second (usual {:.1})",
            sig.median_wps
        ));
    }

    let mut said: Vec<String> = Vec::new();
    for h in sig
        .hits
        .iter()
        .filter(|h| h.at >= start && h.at <= end && h.phrase != "!")
    {
        let p = if h.phrase.starts_with('[') {
            format!("hears {}", h.phrase.trim_matches(|c| c == '[' || c == ']'))
        } else {
            format!("\u{201c}{}\u{201d}", h.phrase)
        };
        if !said.contains(&p) {
            said.push(p);
        }
    }
    if !said.is_empty() {
        let shown: Vec<String> = said.into_iter().take(3).collect();
        out.push(format!("Key words: {}", shown.join(", ")));
    }

    let cuts = (a..b).filter(|i| sig.cuts[*i] > 1.0).count();
    if cuts >= 5 {
        out.push("Lots of camera changes".to_string());
    }

    if out.is_empty() {
        out.push(format!(
            "Best-scoring stretch near {}",
            clock((start + end) / 2.0)
        ));
    }
    out
}

/// A short title from the sentence in the clip with the most going on.
pub fn title(start: f64, end: f64, segments: &[Segment], sig: &Signals) -> String {
    let n = sig.score.len();
    let best = segments
        .iter()
        .filter(|s| s.end > start && s.start < end && s.text.split_whitespace().count() >= 3)
        .max_by(|x, y| {
            let sc = |s: &Segment| {
                let (a, b) = (s.start.max(0.0) as usize, (s.end.ceil() as usize).min(n));
                if a >= b {
                    return f32::MIN;
                }
                sig.score[a..b].iter().copied().fold(f32::MIN, f32::max)
            };
            sc(x).total_cmp(&sc(y))
        });
    match best {
        Some(seg) => tidy_title(&seg.text),
        None => format!("Moment at {}", clock(start)),
    }
}

pub fn tidy_title(text: &str) -> String {
    let t = text.replace(">>", " ");
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = t.trim_matches(|c: char| !c.is_alphanumeric() && c != '!' && c != '?');
    let mut out = String::new();
    for word in t.split(' ') {
        if out.chars().count() + word.chars().count() + 1 > 70 {
            out.push('\u{2026}');
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    let mut c = out.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => out,
    }
}

/// The words spoken inside a clip, for search and for the clip card.
pub fn clip_text(words: &[Word], start: f64, end: f64) -> String {
    words
        .iter()
        .filter(|w| w.start >= start && w.end <= end + 0.3 && w.is_spoken())
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(count: usize) -> Options {
        Options {
            count,
            min_len: 10,
            max_len: 30,
            gap: 5,
        }
    }

    #[test]
    fn picks_the_peak_first() {
        let mut score = vec![0f32; 300];
        for x in score[100..112].iter_mut() {
            *x = 3.0;
        }
        for x in score[200..206].iter_mut() {
            *x = 1.5;
        }
        let picks = pick_windows(&score, opts(2));
        assert_eq!(picks.len(), 2);
        assert!(
            picks[0].start <= 100.0 && picks[0].end >= 112.0,
            "{:?}",
            picks[0]
        );
        assert!(
            picks[1].start <= 200.0 && picks[1].end >= 206.0,
            "{:?}",
            picks[1]
        );
        assert!(picks[0].score > picks[1].score);
        assert!((100.0..112.0).contains(&picks[0].peak));
    }

    #[test]
    fn clips_never_overlap_and_respect_the_gap() {
        let score: Vec<f32> = (0..400).map(|i| ((i as f32) / 7.0).sin() + 1.0).collect();
        let picks = pick_windows(&score, opts(8));
        for (i, a) in picks.iter().enumerate() {
            assert!(a.end - a.start >= 10.0 && a.end - a.start <= 30.0);
            for b in picks.iter().skip(i + 1) {
                assert!(
                    a.end + 5.0 <= b.start || b.end + 5.0 <= a.start,
                    "{a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn a_flat_boring_video_still_gives_one_clip() {
        let picks = pick_windows(&vec![-1.0; 120], opts(5));
        assert_eq!(picks.len(), 1);
    }

    #[test]
    fn short_video_does_not_panic() {
        let picks = pick_windows(&[1.0, 2.0, 1.0], opts(3));
        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0].start, 0.0);
        assert_eq!(picks[0].end, 3.0);
    }

    fn seg(start: f64, end: f64) -> Segment {
        Segment {
            start,
            end,
            text: "a b c".into(),
            words: vec![],
        }
    }

    #[test]
    fn snaps_to_sentence_edges() {
        let p = Pick {
            start: 50.0,
            end: 75.0,
            score: 1.0,
            peak: 60.0,
        };
        let segs = vec![
            seg(40.0, 47.5),
            seg(47.5, 52.0),
            seg(52.0, 76.8),
            seg(76.8, 90.0),
        ];
        let (s, e) = snap(&p, &segs, &[], opts(1), 1000.0);
        assert!((s - 47.25).abs() < 1e-9, "start {s}");
        assert!((e - 77.2).abs() < 1e-9, "end {e}");
    }

    #[test]
    fn keeps_edges_when_snapping_would_lose_the_peak() {
        let p = Pick {
            start: 50.0,
            end: 60.0,
            score: 1.0,
            peak: 59.5,
        };
        let segs = vec![seg(46.0, 56.0)];
        let (s, e) = snap(&p, &segs, &[], opts(1), 1000.0);
        assert!(
            (s - 49.75).abs() < 1e-9 && (e - 60.4).abs() < 1e-9,
            "{s} {e}"
        );
    }

    #[test]
    fn snaps_to_silence_without_transcript() {
        let p = Pick {
            start: 20.0,
            end: 40.0,
            score: 1.0,
            peak: 30.0,
        };
        let (s, e) = snap(&p, &[], &[(18.0, 19.0), (41.0, 42.0)], opts(1), 1000.0);
        assert!(
            (s - 18.25).abs() < 1e-9 && (e - 41.9).abs() < 1e-9,
            "{s} {e}"
        );
    }

    #[test]
    fn titles_are_short_and_capitalised() {
        assert_eq!(
            tidy_title(" >> the launch is a no-go for today."),
            "The launch is a no-go for today"
        );
        let long = "word ".repeat(40);
        let t = tidy_title(&long);
        assert!(t.chars().count() <= 71 && t.ends_with('\u{2026}'));
    }
}
