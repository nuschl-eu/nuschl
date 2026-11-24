// SPDX-License-Identifier: EUPL-1.2
use clap::Parser;
use nuschl_stream_sfu::config::Config;
use std::path::PathBuf;

/// The companion SFU for calls and screen sharing (docs/video-plan.md).
#[derive(Parser)]
#[command(version = nuschl_version::version())]
struct Arguments {
    /// Settings (nuschl-stream-sfu.toml).
    #[arg(long)]
    config: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nuschl_stream_sfu=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let config = Config::load(&arguments.config)?;
    config.check()?;
    // One thread: the core owns all media state.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        tokio::select! {
            result = nuschl_stream_sfu::serve(config) => result.map_err(Into::into),
            _ = tokio::signal::ctrl_c() => Ok(()),
        }
    })
}
