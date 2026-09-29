//! `gns-server` — the Genius Bot API with the web console built in.
//!
//! ```text
//! gns-server --root ./.gns-data                     # http://127.0.0.1:8787
//! gns-server --mock                                 # offline echo model
//! OPENAI_BASE_URL=https://api.example.com/v1 OPENAI_API_KEY=... gns-server --model qwen-plus
//! ```
//!
//! Model, endpoint and key can also be set from the console (Settings);
//! they are saved to `<root>/gns-server.json`.

use anyhow::{Context, Result, bail};
use clap::Parser;
use gns_runtime::{AgentHost, AgentHostConfig};
use gns_server::model::ModelSettingsPatch;
use gns_server::{AppState, EventBus, ModelHub};
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "gns-server", version, about = "Genius Bot multi-agent API with an embedded debug console")]
struct Args {
    /// Root directory for agents and memory.
    #[arg(long, default_value = ".gns-data")]
    root: std::path::PathBuf,
    /// Address to listen on.
    #[arg(long, env = "GNS_SERVER_HOST", default_value = "127.0.0.1")]
    host: std::net::IpAddr,
    #[arg(long, env = "GNS_SERVER_PORT", default_value_t = 8787)]
    port: u16,
    /// Require `Authorization: Bearer <token>` on /api. Mandatory off loopback: agents run shell commands.
    #[arg(long, env = "GNS_SERVER_TOKEN")]
    token: Option<String>,
    /// Model name (overrides the saved settings). Same resolution as gns-cli.
    #[arg(long)]
    model: Option<String>,
    /// openai | kimi | moonshot | auto | mock (overrides the saved settings).
    #[arg(long, env = "GNS_PROVIDER")]
    provider: Option<String>,
    /// OpenAI-compatible base URL, version path included (overrides the saved settings).
    #[arg(long)]
    base_url: Option<String>,
    /// API key (overrides the saved settings). Prefer the environment or the console.
    #[arg(long)]
    api_key: Option<String>,
    /// Environment variable holding the API key.
    #[arg(long)]
    api_key_env: Option<String>,
    /// Disable streaming.
    #[arg(long)]
    no_stream: bool,
    /// Use the offline mock model (same as --provider mock).
    #[arg(long)]
    mock: bool,
    /// IANA time zone, e.g. Asia/Shanghai.
    #[arg(long, env = "GNS_TZ")]
    tz: Option<String>,
    /// Your display name.
    #[arg(long, env = "GNS_USER_NAME")]
    user_name: Option<String>,
    /// Disable the default Shell guard policy (dangerous commands need approval).
    #[arg(long)]
    no_guard: bool,
    /// Model calls kept in memory for the console.
    #[arg(long, default_value_t = 300)]
    llm_log_capacity: usize,
    /// Also append every model call (full request and response) to this JSON Lines file.
    #[arg(long)]
    llm_log_file: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    if !args.host.is_loopback() && args.token.is_none() {
        bail!("listening on {} exposes shell execution through agents; pass --token (or GNS_SERVER_TOKEN)", args.host);
    }

    std::fs::create_dir_all(&args.root).with_context(|| format!("creating {}", args.root.display()))?;
    let settings_path = args.root.join("gns-server.json");
    let mut settings = ModelHub::load_settings(&settings_path).context("reading the saved model settings")?.unwrap_or_default();
    ModelSettingsPatch {
        provider: if args.mock { Some("mock".into()) } else { args.provider.clone() },
        model: args.model.clone(),
        base_url: args.base_url.clone(),
        api_key: args.api_key.clone(),
        api_key_env: args.api_key_env.clone(),
        stream: args.no_stream.then_some(false),
        ..Default::default()
    }
    .apply(&mut settings);

    let bus = EventBus::new(1000);
    let hub = Arc::new(ModelHub::new(settings, settings_path, args.llm_log_capacity, bus.clone(), args.llm_log_file.clone()));
    let status = hub.status();
    match &status.error {
        None => tracing::info!(model = %status.model, provider = %status.provider, base_url = ?status.base_url, "model ready"),
        Some(e) => tracing::warn!("model not configured yet ({e}); set it in the console"),
    }

    let mut config = AgentHostConfig::new(&args.root);
    config.user_name = args.user_name.clone();
    config.time_zone = args.tz.clone();
    let host = AgentHost::open(config, hub.clone()).await.context("opening the host")?;
    let forwarder = bus.forward(host.subscribe());
    if !args.no_guard {
        host.add_policy(Arc::new(gns_tools::ShellGuardPolicy::default()));
    }

    let state = AppState { host: host.clone(), hub, bus, token: args.token.clone() };
    let addr = SocketAddr::new(args.host, args.port);
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    eprintln!("gns-server listening on http://{addr}  (root {})", args.root.display());
    axum::serve(listener, gns_server::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    host.shutdown().await;
    forwarder.abort();
    Ok(())
}
