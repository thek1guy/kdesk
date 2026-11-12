mod capture;
mod config;
mod eis;
mod portal;
mod server;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let Some(cfg) = config::parse_args(std::env::args().skip(1))? else {
        print!("{}", config::HELP);
        return Ok(());
    };

    if cfg.forget {
        if config::forget_restore_token()? {
            println!("Saved portal permission deleted. You'll be asked again next time.");
        } else {
            println!("Nothing saved.");
        }
        return Ok(());
    }

    server::run(cfg).await
}
