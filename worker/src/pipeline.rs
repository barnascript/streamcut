//! One video in, ranked vertical clips out. No Redis or database in here,
//! so the same code runs from the queue and from the `local` command.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::media::{self, Layout, RenderJob, Tools};
use crate::{audio, captions, select, signals, transcript};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub clips: usize,
    pub min_len: usize,
    pub max_len: usize,
    pub layout: Layout,
    pub captions: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            clips: 5,
            min_len: 20,
            max_len: 60,
            layout: Layout::Blur,
            captions: true,
        }
    }
}

impl Settings {
    /// The API checks these too. I check again here so a bad message on the
    /// queue cannot make the worker render 500 clips.
    pub fn clamped(mut self) -> Self {
        self.clips = self.clips.clamp(1, 10);
        self.min_len = self.min_len.clamp(5, 90);
        self.max_len = self.max_len.clamp(self.min_len, 180);
        self
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Progress {
    pub stage: &'static str,
    pub pct: u8,
    pub message: String,
}

pub type OnProgress = Arc<dyn Fn(Progress) + Send + Sync>;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipOut {
    pub rank: usize,
    pub start: f64,
    pub end: f64,
    pub score: f32,
    pub title: String,
    pub reasons: Vec<String>,
    pub text: String,
    pub video: PathBuf,
    pub thumb: PathBuf,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Chart {
    pub step: f64,
    pub loudness: Vec<f32>,
    pub speech: Vec<f32>,
    pub hype: Vec<f32>,
    pub score: Vec<f32>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub duration: f64,
    pub has_transcript: bool,
    pub words: usize,
    pub clips: Vec<ClipOut>,
    pub chart: Chart,
    pub timings_ms: Vec<(String, u128)>,
}

fn say(p: &OnProgress, stage: &'static str, pct: u8, message: impl Into<String>) {
    p(Progress {
        stage,
        pct,
        message: message.into(),
    });
}

pub async fn run(
    tools: &Tools,
    src: &Path,
    work: &Path,
    out_dir: &Path,
    settings: Settings,
    render_slots: usize,
    progress: OnProgress,
) -> Result<Outcome> {
    let settings = settings.clamped();
    tokio::fs::create_dir_all(work).await?;
    tokio::fs::create_dir_all(out_dir).await?;
    let mut timings: Vec<(String, u128)> = Vec::new();
    let clock = std::time::Instant::now();
    let lap = |name: &str, timings: &mut Vec<(String, u128)>| {
        timings.push((name.to_string(), clock.elapsed().as_millis()))
    };

    say(&progress, "probe", 2, "Reading the video");
    let probe = media::probe(tools, src).await?;
    lap("probe", &mut timings);

    // Sound and words.
    let wav = work.join("audio16k.wav");
    let mut loud_db: Vec<f32> = vec![-30.0; probe.duration.ceil() as usize];
    let mut fine_levels: Vec<f32> = Vec::new();
    if probe.has_audio {
        say(&progress, "audio", 5, "Pulling out the sound");
        media::extract_audio(tools, src, &wav, probe.duration).await?;
        let bytes = tokio::fs::read(&wav).await?;
        let pcm = audio::read_wav(&bytes).context("reading extracted audio")?;
        loud_db = audio::levels(&pcm, 1.0);
        fine_levels = audio::levels(&pcm, 0.05);
        lap("audio", &mut timings);
    }

    say(
        &progress,
        "transcribe",
        8,
        "Writing down what is said and finding camera changes",
    );
    let p2 = progress.clone();
    let words_job = async {
        if !probe.has_audio {
            return Ok::<_, anyhow::Error>(None);
        }
        let base = work.join("transcript");
        let json = media::transcribe(tools, &wav, &base, probe.duration, move |pct| {
            // whisper is most of the wait: map its 0-100 to 8-55.
            let overall = 8 + (pct as u32 * 47 / 100) as u8;
            p2(Progress {
                stage: "transcribe",
                pct: overall,
                message: format!("Writing down what is said ({pct}%)"),
            });
        })
        .await?;
        match json {
            Some(path) => {
                let text = tokio::fs::read_to_string(&path).await?;
                Ok(Some(transcript::parse_whisper_json(&text)?))
            }
            None => Ok(None),
        }
    };
    let (segments, cuts) = tokio::join!(words_job, media::scene_cuts(tools, src, probe.duration));
    let segments = segments?.unwrap_or_default();
    let cuts = cuts?;
    let words = transcript::all_words(&segments);
    lap("transcribe+scenes", &mut timings);

    say(&progress, "analyse", 58, "Scoring every second");
    let sig = signals::build(
        &loud_db,
        &words,
        &cuts,
        signals::Weights::for_input(!words.is_empty()),
    );
    let silences = audio::silences(&fine_levels, 0.05, 0.3);
    let opts = select::Options {
        count: settings.clips,
        min_len: settings.min_len,
        max_len: settings.max_len,
        gap: 5,
    };
    let picks = select::pick_windows(&sig.score, opts);

    struct Planned {
        rank: usize,
        start: f64,
        end: f64,
        score: f32,
        peak: f64,
        title: String,
        reasons: Vec<String>,
        text: String,
    }
    let planned: Vec<Planned> = picks
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (start, end) = select::snap(p, &segments, &silences, opts, probe.duration);
            Planned {
                rank: i + 1,
                start,
                end,
                score: p.score,
                peak: p.peak,
                title: select::title(start, end, &segments, &sig),
                reasons: select::reasons(start, end, &sig),
                text: select::clip_text(&words, start, end),
            }
        })
        .collect();
    lap("analyse", &mut timings);

    // Render. ffmpeg already uses several cores, so I only run a couple at once.
    let total = planned.len().max(1);
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let slots = Arc::new(Semaphore::new(render_slots.max(1)));
    say(
        &progress,
        "render",
        60,
        format!("Cutting {} clips", planned.len()),
    );
    let mut tasks = Vec::new();
    for p in planned {
        let slots = slots.clone();
        let done = done.clone();
        let progress = progress.clone();
        let tools = tools.clone();
        let src = src.to_path_buf();
        let work = work.to_path_buf();
        let out_dir = out_dir.to_path_buf();
        let words = if settings.captions {
            words.clone()
        } else {
            vec![]
        };
        let layout = settings.layout;
        let has_audio = probe.has_audio;
        tasks.push(tokio::spawn(async move {
            let _slot = slots.acquire_owned().await?;
            let ass_path = work.join(format!("clip{}.ass", p.rank));
            let ass = if words.is_empty() {
                None
            } else {
                let a = captions::ass(
                    &words,
                    p.start,
                    p.end,
                    &tools.font_name,
                    layout.caption_margin(),
                );
                tokio::fs::write(&ass_path, a).await?;
                Some(ass_path.as_path())
            };
            let video = out_dir.join(format!("{}.mp4", p.rank));
            let thumb = out_dir.join(format!("{}.jpg", p.rank));
            media::render_clip(
                &tools,
                RenderJob {
                    src: &src,
                    start: p.start,
                    end: p.end,
                    layout,
                    ass,
                    has_audio,
                    out: &video,
                },
            )
            .await?;
            media::thumbnail(
                &tools,
                &video,
                (p.peak - p.start).clamp(0.0, p.end - p.start - 0.1),
                &thumb,
            )
            .await?;
            let bytes = tokio::fs::metadata(&video).await?.len();
            let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let pct = 60 + (n * 38 / total) as u8;
            progress(Progress {
                stage: "render",
                pct,
                message: format!("Cut clip {n} of {total}"),
            });
            Ok::<_, anyhow::Error>(ClipOut {
                rank: p.rank,
                start: p.start,
                end: p.end,
                score: p.score,
                title: p.title,
                reasons: p.reasons,
                text: p.text,
                video,
                thumb,
                width: captions::WIDTH,
                height: captions::HEIGHT,
                bytes,
            })
        }));
    }
    let mut clips = Vec::new();
    for t in tasks {
        clips.push(t.await??);
    }
    clips.sort_by_key(|c| c.rank);
    lap("render", &mut timings);

    let max_points = 1500;
    let (loudness, step) = signals::downsample(&sig.loud, max_points);
    let chart = Chart {
        step,
        loudness,
        speech: signals::downsample(&sig.speech, max_points).0,
        hype: signals::downsample(&sig.hype, max_points).0,
        score: signals::downsample(&sig.score, max_points).0,
    };
    say(
        &progress,
        "done",
        100,
        format!("{} clips ready", clips.len()),
    );
    Ok(Outcome {
        duration: probe.duration,
        has_transcript: !words.is_empty(),
        words: words.len(),
        clips,
        chart,
        timings_ms: timings,
    })
}
