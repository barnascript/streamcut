//! Everything that runs ffmpeg, ffprobe or whisper.cpp.
//! Every call has a time limit. A stuck child process must never hang a worker.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::captions;

#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: String,
    pub ffprobe: String,
    pub whisper_bin: String,
    pub whisper_model: Option<PathBuf>,
    pub whisper_threads: usize,
    pub fonts_dir: PathBuf,
    pub font_name: String,
    pub encoder: String,
}

/// The input itself is broken (not a video, no sound, too long...).
/// Trying again will not help, so the job fails straight away.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BadInput(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// The whole picture in the middle, a blurred copy filling the back.
    Blur,
    /// A 9:16 slice from the centre of the picture.
    Crop,
}

impl Layout {
    pub fn caption_margin(self) -> u32 {
        match self {
            Layout::Blur => 470,
            Layout::Crop => 380,
        }
    }
}

async fn run(mut cmd: Command, limit: Duration, what: &str) -> Result<std::process::Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().with_context(|| format!("starting {what}"))?;
    let out = tokio::time::timeout(limit, child.wait_with_output())
        .await
        .map_err(|_| {
            anyhow!(
                "{what} took longer than {}s and was stopped",
                limit.as_secs()
            )
        })??;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(6).collect();
        bail!(
            "{what} failed: {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        );
    }
    Ok(out)
}

/// Generous limits: a few times real time, with a floor for short files.
fn limit_for(duration_s: f64, factor: f64) -> Duration {
    Duration::from_secs_f64((duration_s * factor).max(120.0))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Probe {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
}

#[derive(Deserialize)]
struct FfStream {
    codec_type: String,
    width: Option<u32>,
    height: Option<u32>,
}
#[derive(Deserialize)]
struct FfFormat {
    duration: Option<String>,
}
#[derive(Deserialize)]
struct FfProbe {
    streams: Vec<FfStream>,
    format: FfFormat,
}

pub async fn probe(t: &Tools, src: &Path) -> Result<Probe> {
    let mut cmd = Command::new(&t.ffprobe);
    cmd.args([
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ])
    .arg(src);
    let out = match run(cmd, Duration::from_secs(60), "ffprobe").await {
        Ok(o) => o,
        Err(e) => return Err(BadInput(format!("could not read the file as a video ({e})")).into()),
    };
    let p: FfProbe = serde_json::from_slice(&out.stdout).context("reading ffprobe output")?;
    let video = p.streams.iter().find(|s| s.codec_type == "video");
    let has_audio = p.streams.iter().any(|s| s.codec_type == "audio");
    let Some(v) = video else {
        return Err(BadInput("the file has no video track".into()).into());
    };
    let duration: f64 = p
        .format
        .duration
        .as_deref()
        .and_then(|d| d.parse().ok())
        .unwrap_or(0.0);
    if duration < 3.0 {
        return Err(BadInput("the video is shorter than 3 seconds".into()).into());
    }
    Ok(Probe {
        duration,
        width: v.width.unwrap_or(0),
        height: v.height.unwrap_or(0),
        has_audio,
    })
}

/// 16 kHz mono 16-bit WAV: what whisper.cpp wants and what audio.rs reads.
pub async fn extract_audio(t: &Tools, src: &Path, out: &Path, duration: f64) -> Result<()> {
    let mut cmd = Command::new(&t.ffmpeg);
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le"])
        .arg(out);
    run(cmd, limit_for(duration, 0.5), "ffmpeg (audio)").await?;
    Ok(())
}

/// Times (in seconds) where the picture changes a lot.
pub async fn scene_cuts(t: &Tools, src: &Path, duration: f64) -> Result<Vec<f64>> {
    let mut cmd = Command::new(&t.ffmpeg);
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(src)
        .args([
            "-an",
            "-vf",
            "fps=4,scale=320:-2,select='gt(scene,0.35)',metadata=print:file=-",
            "-f",
            "null",
            "-",
        ]);
    let out = run(cmd, limit_for(duration, 1.0), "ffmpeg (scenes)").await?;
    Ok(parse_scene_times(&String::from_utf8_lossy(&out.stdout)))
}

pub fn parse_scene_times(text: &str) -> Vec<f64> {
    text.lines()
        .filter_map(|l| l.split("pts_time:").nth(1))
        .filter_map(|v| v.split_whitespace().next()?.parse().ok())
        .collect()
}

/// Runs whisper.cpp and returns the path of its JSON file.
/// `on_progress` gets 0..=100 as whisper works through the audio.
pub async fn transcribe(
    t: &Tools,
    wav: &Path,
    out_base: &Path,
    duration: f64,
    on_progress: impl Fn(u8),
) -> Result<Option<PathBuf>> {
    let Some(model) = &t.whisper_model else {
        return Ok(None);
    };
    let mut cmd = Command::new(&t.whisper_bin);
    cmd.arg("-m")
        .arg(model)
        .arg("-f")
        .arg(wav)
        .args([
            "-l",
            "en",
            "-ojf",
            "-pp",
            "-t",
            &t.whisper_threads.to_string(),
            "-of",
        ])
        .arg(out_base)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().context("starting whisper-cli")?;
    let stderr = child.stderr.take().expect("piped stderr");
    let limit = limit_for(duration, 1.5);
    let mut tail: Vec<String> = Vec::new();
    let work = async {
        let mut lines = BufReader::new(stderr).lines();
        while let Some(line) = lines.next_line().await? {
            if let Some(p) = crate::transcript::progress_from_line(&line) {
                on_progress(p);
            } else {
                tail.push(line);
                if tail.len() > 6 {
                    tail.remove(0);
                }
            }
        }
        child.wait().await.map_err(anyhow::Error::from)
    };
    let status = tokio::time::timeout(limit, work).await.map_err(|_| {
        anyhow!(
            "whisper took longer than {}s and was stopped",
            limit.as_secs()
        )
    })??;
    if !status.success() {
        bail!("whisper failed: {}", tail.join(" | "));
    }
    let json = out_base.with_extension("json");
    if !json.exists() {
        bail!("whisper did not write {}", json.display());
    }
    Ok(Some(json))
}

/// ffmpeg filter arguments need some characters escaped. My work paths are
/// uuids, but I check anyway rather than build a broken command.
fn filter_path(p: &Path) -> Result<String> {
    let s = p.to_string_lossy().to_string();
    if s.chars()
        .any(|c| matches!(c, '\'' | ':' | '\\' | ',' | ';' | '[' | ']'))
    {
        bail!("path not safe for an ffmpeg filter: {s}");
    }
    Ok(s)
}

pub fn video_filter(layout: Layout, ass: Option<&Path>, fonts_dir: &Path) -> Result<String> {
    let (w, h) = (captions::WIDTH, captions::HEIGHT);
    let base = match layout {
        // The blurred back is made at quarter size and scaled up: same look,
        // a lot less work for the CPU.
        Layout::Blur => format!(
            "split=2[bg0][fg0];\
             [bg0]scale=270:480:force_original_aspect_ratio=increase,crop=270:480,gblur=sigma=10,eq=brightness=-0.12,scale={w}:{h}[bg];\
             [fg0]scale={w}:-2[fg];\
             [bg][fg]overlay=x=(W-w)/2:y=(H-h)/2-200"
        ),
        Layout::Crop => format!("scale=-2:{h},crop={w}:{h}"),
    };
    let mut f = format!("[0:v]{base},setsar=1,format=yuv420p");
    if let Some(ass) = ass {
        f.push_str(&format!(
            ",subtitles=filename='{}':fontsdir='{}'",
            filter_path(ass)?,
            filter_path(fonts_dir)?
        ));
    }
    f.push_str("[v]");
    Ok(f)
}

pub fn encoder_args(encoder: &str) -> Vec<String> {
    let a: &[&str] = match encoder {
        "h264_videotoolbox" => &[
            "-c:v",
            "h264_videotoolbox",
            "-b:v",
            "7M",
            "-maxrate",
            "9M",
            "-bufsize",
            "14M",
            "-profile:v",
            "high",
        ],
        _ => &[
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "21",
            "-profile:v",
            "high",
        ],
    };
    a.iter().map(|s| s.to_string()).collect()
}

pub struct RenderJob<'a> {
    pub src: &'a Path,
    pub start: f64,
    pub end: f64,
    pub layout: Layout,
    pub ass: Option<&'a Path>,
    pub has_audio: bool,
    pub out: &'a Path,
}

pub async fn render_clip(t: &Tools, j: RenderJob<'_>) -> Result<()> {
    let dur = (j.end - j.start).max(0.5);
    let mut cmd = Command::new(&t.ffmpeg);
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-ss",
            &format!("{:.3}", j.start),
            "-t",
            &format!("{dur:.3}"),
            "-i",
        ])
        .arg(j.src)
        .args([
            "-filter_complex",
            &video_filter(j.layout, j.ass, &t.fonts_dir)?,
            "-map",
            "[v]",
        ]);
    if j.has_audio {
        let fade_out = (dur - 0.4).max(0.0);
        cmd.args([
            "-map",
            "0:a:0",
            "-af",
            &format!(
                "afade=t=in:d=0.25,afade=t=out:st={fade_out:.3}:d=0.4,loudnorm=I=-14:TP=-1.5:LRA=11"
            ),
            "-ar",
            "48000",
            "-c:a",
            "aac",
            "-b:a",
            "160k",
        ]);
    }
    cmd.args(encoder_args(&t.encoder))
        .args(["-pix_fmt", "yuv420p", "-g", "60", "-movflags", "+faststart"])
        .arg(j.out);
    run(cmd, limit_for(dur, 8.0), "ffmpeg (render)").await?;
    Ok(())
}

pub async fn thumbnail(t: &Tools, clip: &Path, at: f64, out: &Path) -> Result<()> {
    let mut cmd = Command::new(&t.ffmpeg);
    cmd.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-ss",
        &format!("{:.3}", at.max(0.0)),
        "-i",
    ])
    .arg(clip)
    .args(["-frames:v", "1", "-vf", "scale=360:-2", "-q:v", "4"])
    .arg(out);
    run(cmd, Duration::from_secs(60), "ffmpeg (thumbnail)").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scene_times() {
        let out = "frame:0    pts:49      pts_time:12.25\nlavfi.scene_score=0.5\nframe:1    pts:80      pts_time:20\n";
        assert_eq!(parse_scene_times(out), vec![12.25, 20.0]);
    }

    #[test]
    fn builds_blur_filter_with_captions() {
        let f = video_filter(
            Layout::Blur,
            Some(Path::new("/w/abc/c1.ass")),
            Path::new("/app/fonts"),
        )
        .unwrap();
        assert!(f.starts_with("[0:v]split=2"));
        assert!(f.contains("subtitles=filename='/w/abc/c1.ass':fontsdir='/app/fonts'"));
        assert!(f.ends_with("[v]"));
    }

    #[test]
    fn refuses_unsafe_paths() {
        assert!(
            video_filter(
                Layout::Crop,
                Some(Path::new("/tmp/a:b.ass")),
                Path::new("/f")
            )
            .is_err()
        );
    }

    #[test]
    fn crop_filter_without_captions() {
        let f = video_filter(Layout::Crop, None, Path::new("/f")).unwrap();
        assert_eq!(
            f,
            "[0:v]scale=-2:1920,crop=1080:1920,setsar=1,format=yuv420p[v]"
        );
    }
}
