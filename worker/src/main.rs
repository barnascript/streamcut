mod audio;
mod captions;
mod config;
mod fetch;
mod media;
mod pipeline;
mod queue;
mod select;
mod signals;
mod store;
mod transcript;
mod worker;

use anyhow::{Result, bail};
use std::path::PathBuf;
use std::sync::Arc;

use config::Config;
use pipeline::{Progress, Settings};

fn usage() -> ! {
    eprintln!(
        "streamcut-worker            take jobs from the queue (default)\n\
         streamcut-worker local <video> [--out DIR] [--clips N] [--layout blur|crop] [--no-captions]\n\
                                     cut clips from one file, no Redis or database needed"
    );
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cfg = Config::from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("local") => local(&cfg, &args[1..]).await,
        Some("-h" | "--help") => usage(),
        Some(other) => bail!("unknown command {other}"),
        None => worker::serve(cfg).await,
    }
}

async fn local(cfg: &Config, args: &[String]) -> Result<()> {
    let Some(input) = args.first() else { usage() };
    let mut out = PathBuf::from("out");
    let mut settings = Settings::default();
    let mut i = 1;
    while i < args.len() {
        let next = || args.get(i + 1).cloned().unwrap_or_else(|| usage());
        match args[i].as_str() {
            "--out" => {
                out = PathBuf::from(next());
                i += 1;
            }
            "--clips" => {
                settings.clips = next().parse()?;
                i += 1;
            }
            "--layout" => {
                settings.layout = serde_json::from_str(&format!("\"{}\"", next()))?;
                i += 1;
            }
            "--no-captions" => settings.captions = false,
            _ => usage(),
        }
        i += 1;
    }
    let src = std::fs::canonicalize(input)?;
    std::fs::create_dir_all(&out)?;
    let out = std::fs::canonicalize(&out)?;
    let work = out.join("work");
    let progress: pipeline::OnProgress = Arc::new(|p: Progress| {
        eprintln!("[{:>3}%] {:<10} {}", p.pct, p.stage, p.message);
    });
    let started = std::time::Instant::now();
    let outcome = pipeline::run(
        &cfg.tools,
        &src,
        &work,
        &out,
        settings,
        cfg.render_slots,
        progress,
    )
    .await?;
    std::fs::write(
        out.join("summary.json"),
        serde_json::to_vec_pretty(&outcome)?,
    )?;
    println!(
        "\n{} clips from {:.0} s of video in {:.1} s",
        outcome.clips.len(),
        outcome.duration,
        started.elapsed().as_secs_f64()
    );
    for c in &outcome.clips {
        println!(
            "#{} {:>7.1}-{:<7.1} score {:.2}  {}",
            c.rank, c.start, c.end, c.score, c.title
        );
        for r in &c.reasons {
            println!("      - {r}");
        }
    }
    Ok(())
}
