//! Word-by-word captions as an ASS subtitle file that ffmpeg burns in.
//! Short lines (up to 3 words) with the word being said shown in yellow,
//! the way people expect captions on short videos to look.

use crate::transcript::Word;

pub const WIDTH: u32 = 1080;
pub const HEIGHT: u32 = 1920;

const MAX_WORDS: usize = 3;
const MAX_CHARS: usize = 18;

fn ts(t: f64) -> String {
    let cs = (t.max(0.0) * 100.0).round() as u64;
    format!(
        "{}:{:02}:{:02}.{:02}",
        cs / 360_000,
        (cs / 6000) % 60,
        (cs / 100) % 60,
        cs % 100
    )
}

fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '{' | '}' | '\\'))
        .collect::<String>()
        .to_uppercase()
}

/// Groups words into short caption lines.
pub fn lines(words: &[Word]) -> Vec<Vec<Word>> {
    let mut out: Vec<Vec<Word>> = Vec::new();
    let mut cur: Vec<Word> = Vec::new();
    for w in words {
        let chars: usize = cur
            .iter()
            .map(|x| x.text.chars().count() + 1)
            .sum::<usize>()
            + w.text.chars().count();
        let pause = cur.last().map(|l| w.start - l.end > 0.6).unwrap_or(false);
        if !cur.is_empty() && (cur.len() >= MAX_WORDS || chars > MAX_CHARS || pause) {
            out.push(std::mem::take(&mut cur));
        }
        let ends_sentence = w.text.ends_with(['.', '!', '?']);
        cur.push(w.clone());
        if ends_sentence {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Builds the .ass file for one clip. Times in the file start at 0 because
/// ffmpeg seeks to the clip start before it draws the captions.
pub fn ass(words: &[Word], clip_start: f64, clip_end: f64, font: &str, margin_v: u32) -> String {
    let inside: Vec<Word> = words
        .iter()
        .filter(|w| w.is_spoken() && w.start >= clip_start - 0.05 && w.start < clip_end)
        .map(|w| Word {
            start: w.start - clip_start,
            end: (w.end.min(clip_end) - clip_start).max(w.start - clip_start + 0.05),
            text: w
                .text
                .trim_matches(|c: char| c == ',' || c == ';')
                .to_string(),
        })
        .collect();

    let mut s = format!(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: {WIDTH}\nPlayResY: {HEIGHT}\nWrapStyle: 2\nScaledBorderAndShadow: yes\n\n\
[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
Style: Cap,{font},86,&H00FFFFFF,&H00FFFFFF,&H00101010,&H90000000,0,0,0,0,100,100,1,0,1,7,3,2,60,60,{margin_v},1\n\n\
[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n"
    );

    let all_lines = lines(&inside);
    for (li, line) in all_lines.iter().enumerate() {
        let next_line_start = all_lines
            .get(li + 1)
            .and_then(|l| l.first())
            .map(|w| w.start);
        for (k, w) in line.iter().enumerate() {
            let from = if k == 0 {
                w.start
            } else {
                w.start.max(line[k - 1].start)
            };
            let mut to = match line.get(k + 1) {
                Some(next) => next.start,
                None => w.end + 0.25,
            };
            if let Some(n) = next_line_start {
                to = to.min(n);
            }
            to = to.min(clip_end - clip_start);
            if to <= from {
                continue;
            }
            let text: Vec<String> = line
                .iter()
                .enumerate()
                .map(|(j, x)| {
                    let t = clean(&x.text);
                    if j == k {
                        format!("{{\\c&H00D7FF&}}{t}{{\\c&HFFFFFF&}}")
                    } else {
                        t
                    }
                })
                .collect();
            s.push_str(&format!(
                "Dialogue: 0,{},{},Cap,,0,0,0,,{}\n",
                ts(from),
                ts(to),
                text.join(" ")
            ));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(start: f64, end: f64, text: &str) -> Word {
        Word {
            start,
            end,
            text: text.into(),
        }
    }

    #[test]
    fn formats_ass_times() {
        assert_eq!(ts(0.0), "0:00:00.00");
        assert_eq!(ts(61.234), "0:01:01.23");
        assert_eq!(ts(3725.5), "1:02:05.50");
    }

    #[test]
    fn breaks_lines_on_count_pause_and_sentence_end() {
        let words = vec![
            w(0.0, 0.2, "one"),
            w(0.2, 0.4, "two"),
            w(0.4, 0.6, "three"),
            w(0.6, 0.8, "four."),
            w(0.8, 1.0, "five"),
            w(2.0, 2.2, "six"),
        ];
        let l: Vec<Vec<String>> = lines(&words)
            .iter()
            .map(|l| l.iter().map(|w| w.text.clone()).collect())
            .collect();
        assert_eq!(
            l,
            vec![
                vec!["one", "two", "three"],
                vec!["four."],
                vec!["five"],
                vec!["six"]
            ]
        );
    }

    #[test]
    fn highlights_one_word_at_a_time_with_clip_relative_times() {
        let words = vec![
            w(10.0, 10.3, "we"),
            w(10.3, 10.6, "have"),
            w(10.6, 11.2, "liftoff!"),
        ];
        let a = ass(&words, 10.0, 20.0, "Archivo Black", 500);
        let events: Vec<&str> = a.lines().filter(|l| l.starts_with("Dialogue")).collect();
        assert_eq!(events.len(), 3);
        assert!(events[0].contains("0:00:00.00,0:00:00.30"), "{}", events[0]);
        assert!(events[0].contains("{\\c&H00D7FF&}WE{\\c&HFFFFFF&} HAVE LIFTOFF!"));
        assert!(events[2].contains("HAVE {\\c&H00D7FF&}LIFTOFF!"));
        assert!(a.contains("Style: Cap,Archivo Black,86"));
    }

    #[test]
    fn skips_words_outside_the_clip_and_sound_tags() {
        let words = vec![
            w(1.0, 1.5, "before"),
            w(5.0, 5.5, "[APPLAUSE]"),
            w(6.0, 6.5, "inside"),
            w(30.0, 30.5, "after"),
        ];
        let a = ass(&words, 4.0, 20.0, "X", 500);
        assert!(a.contains("INSIDE"));
        assert!(!a.contains("BEFORE") && !a.contains("AFTER") && !a.contains("APPLAUSE"));
    }

    #[test]
    fn strips_ass_control_characters() {
        let a = ass(&[w(0.0, 0.5, "{\\b1}hi")], 0.0, 5.0, "X", 500);
        assert!(a.contains("B1HI"));
    }
}
