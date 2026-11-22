//! `orca-server` – engine ORCA RadioAIR bez okna (headless). Hodí se pro Mac mini bez monitoru,
//! servery a vývoj UI. Desktopová aplikace spouští totéž uvnitř okna.

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "orca-server", version, about = "ORCA RadioAIR – odbavovací engine pro rádio")]
struct Args {
    /// Datová složka (konfigurace orca.toml, databáze, exporty).
    #[arg(long, env = "ORCA_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Přepíše adresu API z konfigurace (např. 0.0.0.0:8750 pro přístup ze sítě).
    #[arg(long)]
    bind: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,symphonia=warn,symphonia_bundle_mp3=error")).init();
    let args = Args::parse();
    let data_dir = args.data_dir.unwrap_or_else(orca_core::default_data_dir);
    let orca = tokio::task::spawn_blocking(move || orca_core::Orca::start(&data_dir)).await??;
    let bind = args.bind.unwrap_or_else(|| orca.cfg.read().api.bind.clone());
    let app = orca_server::router(orca.clone());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    log::info!("API a UI: http://{bind}");
    axum::serve(listener, app).with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; }).await?;
    orca.encoders.stop_all();
    Ok(())
}
