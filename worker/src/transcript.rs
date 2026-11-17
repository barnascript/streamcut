//! Turns whisper.cpp's JSON output into sentences and words with times.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Word {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

impl Word {
    /// Lower case, letters and digits only. "No-go!" becomes "nogo".
    pub fn norm(&self) -> String {
        self.text
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '\'')
            .flat_map(|c| c.to_lowercase())
            .collect()
    }

    /// Whisper writes sounds it hears as [APPLAUSE], (laughs) and so on.
    pub fn is_sound_tag(&self) -> bool {
        let t = self.text.trim();
        (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('(') && t.ends_with(')'))
    }

    pub fn is_spoken(&self) -> bool {
        !self.is_sound_tag() && self.text.chars().any(|c| c.is_alphanumeric())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub words: Vec<Word>,
}

#[derive(Deserialize)]
struct Offsets {
    from: i64,
    to: i64,
}

#[derive(Deserialize)]
struct Token {
    text: String,
    offsets: Offsets,
}

#[derive(Deserialize)]
struct RawSegment {
    offsets: Offsets,
    text: String,
    #[serde(default)]
    tokens: Vec<Token>,
}

#[derive(Deserialize)]
struct Raw {
    transcription: Vec<RawSegment>,
}

/// whisper.cpp splits words into pieces (" refurb", "ished"). A piece that
/// starts with a space starts a new word; anything else joins the last one.
pub fn parse_whisper_json(json: &str) -> Result<Vec<Segment>> {
    let raw: Raw = serde_json::from_str(json).context("reading whisper JSON")?;
    let mut out = Vec::with_capacity(raw.transcription.len());
    for seg in raw.transcription {
        let mut words: Vec<Word> = Vec::new();
        for tok in seg.tokens {
            if tok.text.starts_with("[_") {
                continue; // whisper's own markers, like [_BEG_]
            }
            let (from, to) = (
                tok.offsets.from as f64 / 1000.0,
                tok.offsets.to as f64 / 1000.0,
            );
            let starts_word = tok.text.starts_with(' ') || words.is_empty();
            let piece = tok.text.trim();
            if piece.is_empty() {
                continue;
            }
            if starts_word {
                words.push(Word {
                    start: from,
                    end: to,
                    text: piece.to_string(),
                });
            } else if let Some(last) = words.last_mut() {
                last.text.push_str(piece);
                last.end = last.end.max(to);
            }
        }
        // ">>" is how whisper marks a new speaker. It is not a word.
        words.retain(|w| w.is_spoken() || w.is_sound_tag());
        let text = seg
            .text
            .replace(">>", " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() && words.is_empty() {
            continue;
        }
        out.push(Segment {
            start: seg.offsets.from as f64 / 1000.0,
            end: seg.offsets.to as f64 / 1000.0,
            text,
            words,
        });
    }
    Ok(out)
}

pub fn all_words(segments: &[Segment]) -> Vec<Word> {
    segments
        .iter()
        .flat_map(|s| s.words.iter().cloned())
        .collect()
}

/// Reads the "progress = 40%" lines whisper.cpp prints with --print-progress.
pub fn progress_from_line(line: &str) -> Option<u8> {
    let rest = line.split("progress =").nth(1)?;
    rest.trim().trim_end_matches('%').trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"transcription":[
      {"offsets":{"from":0,"to":3000},"text":" >> And we are refurbished!",
       "tokens":[
         {"text":"[_BEG_]","offsets":{"from":0,"to":0}},
         {"text":" >>","offsets":{"from":0,"to":500}},
         {"text":" And","offsets":{"from":500,"to":800}},
         {"text":" we","offsets":{"from":800,"to":1000}},
         {"text":" are","offsets":{"from":1000,"to":1200}},
         {"text":" refurb","offsets":{"from":1200,"to":1700}},
         {"text":"ished","offsets":{"from":1700,"to":2100}},
         {"text":"!","offsets":{"from":2100,"to":2200}},
         {"text":"[_TT_150]","offsets":{"from":3000,"to":3000}}
       ]},
      {"offsets":{"from":3000,"to":5000},"text":" [APPLAUSE]",
       "tokens":[
         {"text":" [","offsets":{"from":3000,"to":3100}},
         {"text":"APPL","offsets":{"from":3100,"to":3500}},
         {"text":"AUSE","offsets":{"from":3500,"to":4000}},
         {"text":"]","offsets":{"from":4000,"to":5000}}
       ]}
    ]}"#;

    #[test]
    fn joins_word_pieces_and_drops_markers() {
        let segs = parse_whisper_json(SAMPLE).unwrap();
        assert_eq!(segs.len(), 2);
        let w: Vec<&str> = segs[0].words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(w, vec!["And", "we", "are", "refurbished!"]);
        assert_eq!(segs[0].words[3].start, 1.2);
        assert_eq!(segs[0].words[3].end, 2.2);
        assert_eq!(segs[0].text, "And we are refurbished!");
    }

    #[test]
    fn keeps_sound_tags_as_one_word() {
        let segs = parse_whisper_json(SAMPLE).unwrap();
        assert_eq!(segs[1].words.len(), 1);
        assert!(segs[1].words[0].is_sound_tag());
        assert!(!segs[1].words[0].is_spoken());
    }

    #[test]
    fn normalises_words() {
        let w = Word {
            start: 0.0,
            end: 1.0,
            text: "No-go!".into(),
        };
        assert_eq!(w.norm(), "nogo");
    }

    #[test]
    fn reads_progress_lines() {
        assert_eq!(
            progress_from_line("whisper_print_progress_callback: progress =  45%"),
            Some(45)
        );
        assert_eq!(progress_from_line("something else"), None);
    }
}
