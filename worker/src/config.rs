use std::path::PathBuf;

use crate::media::Tools;

fn env(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

#[derive(Clone, Debug)]
pub struct Config {
    pub redis_url: String,
    pub scylla_nodes: Vec<String>,
    pub keyspace: String,
    pub data_dir: PathBuf,
    pub worker_id: String,
    pub render_slots: usize,
    /// A job nobody has touched for this long is taken over by another worker.
    pub claim_idle_ms: u64,
    pub max_attempts: u64,
    pub max_download_mb: u64,
    /// Only for local tests that serve a file from this machine.
    pub allow_private_urls: bool,
    pub tools: Tools,
}

impl Config {
    pub fn from_env() -> Self {
        let repo_fonts = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fonts");
        let default_model = dirs_home().map(|h| h.join(".cache/streamcut/ggml-base.en.bin"));
        let whisper_model = std::env::var("WHISPER_MODEL")
            .ok()
            .map(PathBuf::from)
            .or(default_model)
            .filter(|p| p.exists());
        let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "local".into());
        Config {
            redis_url: env("REDIS_URL", "redis://127.0.0.1:6379"),
            scylla_nodes: env("SCYLLA_NODES", "127.0.0.1:9042")
                .split(',')
                .map(|s| s.trim().to_string())
                .collect(),
            keyspace: env("SCYLLA_KEYSPACE", "streamcut"),
            data_dir: PathBuf::from(env("DATA_DIR", "../data")),
            worker_id: env("WORKER_ID", &format!("{host}-{}", std::process::id())),
            render_slots: env_num("RENDER_SLOTS", 2),
            claim_idle_ms: env_num("CLAIM_IDLE_MS", 90_000),
            max_attempts: env_num("MAX_ATTEMPTS", 3),
            max_download_mb: env_num("MAX_DOWNLOAD_MB", 2048),
            allow_private_urls: env("ALLOW_PRIVATE_URLS", "0") == "1",
            tools: Tools {
                ffmpeg: env("FFMPEG", "ffmpeg"),
                ffprobe: env("FFPROBE", "ffprobe"),
                whisper_bin: env("WHISPER_BIN", "whisper-cli"),
                whisper_model,
                whisper_threads: env_num("WHISPER_THREADS", 6),
                fonts_dir: PathBuf::from(env("FONTS_DIR", &repo_fonts.to_string_lossy())),
                font_name: env("CAPTION_FONT", "Archivo Black"),
                encoder: env("ENCODER", "libx264"),
            },
        }
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
