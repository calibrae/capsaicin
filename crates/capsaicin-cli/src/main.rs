use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use capsaicin_client::{ClientEvent, DisplayEvent, SpiceClient};
use capsaicin_proto::enums::ChannelType;
use capsaicin_server::{Server, default_channels, serve_main_bootstrap};

mod viewer;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
capsaicin — SPICE test client / server / viewer

USAGE:
  capsaicin connect <host:port> [--password PW]
  capsaicin serve   <bind:port> [--password PW]
  capsaicin view    <host:port> [--password PW]

ENV:
  RUST_LOG   logging filter (default: info)
";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(true)
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    match rt.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(%e, "fatal");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let Some(subcommand) = args.next() else {
        print_usage_and_exit();
    };
    match subcommand.as_str() {
        "connect" => {
            let addr = args.next().unwrap_or_else(|| print_usage_and_exit());
            let password = parse_password_flag(args);
            connect_flow(&addr, &password).await
        }
        "serve" => {
            let addr = args.next().unwrap_or_else(|| print_usage_and_exit());
            let password = parse_password_flag(args);
            serve_flow(&addr, &password).await
        }
        "view" => {
            let addr = args.next().unwrap_or_else(|| print_usage_and_exit());
            let password = parse_password_flag(args);
            // The viewer creates its own tokio runtime in a sidecar
            // thread and owns the main thread for winit. We invoke it
            // synchronously here.
            viewer::run(&addr, &password)
        }
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => {
            eprintln!("unknown subcommand: {other}\n");
            print!("{USAGE}");
            Err("bad subcommand".into())
        }
    }
}

fn parse_password_flag<I: Iterator<Item = String>>(mut args: I) -> String {
    while let Some(a) = args.next() {
        if a == "--password" || a == "-p" {
            return args.next().unwrap_or_default();
        }
    }
    String::new()
}

fn print_usage_and_exit() -> ! {
    print!("{USAGE}");
    std::process::exit(2);
}

async fn connect_flow(addr: &str, password: &str) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(%addr, "connecting as SPICE client");
    let mut client = SpiceClient::connect(addr, password).await?;
    tracing::info!(session_id = client.session_id(), "SPICE client connected");

    let drain_secs: u64 = std::env::var("CAPSAICIN_DRAIN_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let until = tokio::time::Instant::now() + Duration::from_secs(drain_secs);
    loop {
        let remaining = until.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, client.next_event()).await {
            Ok(Some(ClientEvent::Display(evt))) => log_display_event(&evt),
            Ok(Some(ClientEvent::Closed(err))) => {
                tracing::warn!(?err, "client closed");
                break;
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    client.close().await;
    Ok(())
}

fn log_display_event(evt: &DisplayEvent) {
    match evt {
        DisplayEvent::SurfaceCreated {
            id,
            width,
            height,
            format,
            primary,
        } => tracing::info!(
            id, width, height, ?format, primary,
            "display: SurfaceCreated"
        ),
        DisplayEvent::SurfaceDestroyed { id } => {
            tracing::info!(id, "display: SurfaceDestroyed")
        }
        DisplayEvent::MonitorsConfig { heads, max_allowed } => {
            tracing::info!(heads = heads.len(), max_allowed, "display: MonitorsConfig");
            for h in heads {
                tracing::info!(
                    id = h.id,
                    surface = h.surface_id,
                    w = h.width,
                    h = h.height,
                    x = h.x,
                    y = h.y,
                    "  head"
                );
            }
        }
        DisplayEvent::Mode { width, height, bits } => {
            tracing::info!(width, height, bits, "display: Mode")
        }
        DisplayEvent::Mark => tracing::info!("display: Mark"),
        DisplayEvent::Reset => tracing::info!("display: Reset"),
        DisplayEvent::UnhandledDraw { msg_type, size } => {
            tracing::debug!(msg_type, size, "display: unhandled draw/stream")
        }
        DisplayEvent::Region {
            surface_id,
            rect,
            pixels,
            surface_format,
        } => {
            let w = rect.width();
            let h = rect.height();
            match pixels {
                capsaicin_client::RegionPixels::SolidColor(c) => tracing::info!(
                    surface_id, w, h, color = format!("{c:#010x}"), ?surface_format,
                    "display: Region (solid)"
                ),
                capsaicin_client::RegionPixels::Raw { data, stride } => tracing::info!(
                    surface_id, w, h, stride, bytes = data.len(), ?surface_format,
                    "display: Region (raw)"
                ),
            }
        }
        DisplayEvent::StreamCreated {
            stream_id,
            surface_id,
            codec,
            dest,
            src_width,
            src_height,
        } => tracing::info!(
            stream_id, surface_id, ?codec,
            w = dest.width(), h = dest.height(),
            src_width, src_height,
            "display: StreamCreated"
        ),
        DisplayEvent::StreamFrame {
            stream_id,
            multi_media_time,
            dest_rect,
            pixels,
        } => match pixels {
            capsaicin_client::RegionPixels::Raw { data, stride } => tracing::info!(
                stream_id, multi_media_time,
                w = dest_rect.width(), h = dest_rect.height(),
                stride, bytes = data.len(),
                "display: StreamFrame (raw)"
            ),
            capsaicin_client::RegionPixels::SolidColor(c) => tracing::info!(
                stream_id, color = format!("{c:#010x}"),
                "display: StreamFrame (solid?)"
            ),
        },
        DisplayEvent::StreamDestroyed { stream_id } => {
            tracing::info!(stream_id, "display: StreamDestroyed")
        }
        DisplayEvent::CopyRect {
            surface_id,
            src_x,
            src_y,
            dest_rect,
        } => tracing::debug!(
            surface_id, src_x, src_y,
            w = dest_rect.width(), h = dest_rect.height(),
            "display: CopyRect"
        ),
    }
}

async fn serve_flow(addr: &str, password: &str) -> Result<(), Box<dyn std::error::Error>> {
    let server = Arc::new(Server::new(password)?);
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "capsaicin serving SPICE");

    loop {
        let (stream, peer) = listener.accept().await?;
        let server = server.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_server_conn(server, stream, peer.to_string()).await {
                tracing::warn!(%peer, %e, "connection ended with error");
            }
        });
    }
}

async fn handle_server_conn(
    server: Arc<Server>,
    stream: tokio::net::TcpStream,
    peer: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let accepted = server.accept(stream).await?;
    tracing::info!(
        %peer,
        connection_id = accepted.connection_id,
        channel_type = ?accepted.channel_type,
        channel_id = accepted.channel_id,
        mini_header = accepted.channel.mini_header(),
        "linked"
    );

    match accepted.channel_type {
        ChannelType::Main => {
            let session_id = if accepted.connection_id == 0 {
                server.new_session_id()
            } else {
                accepted.connection_id
            };
            let mut ch = accepted.channel;
            serve_main_bootstrap(&mut ch, session_id, &default_channels(), None).await?;
            tracing::info!(%peer, session_id, "main channel attached; idling");
            while let Ok(m) = ch.read_message().await {
                tracing::debug!(%peer, msg_type = m.msg_type, "rx");
            }
        }
        _ => {
            let mut ch = accepted.channel;
            tracing::info!(%peer, channel_type = ?accepted.channel_type, "sub-channel opened; idling");
            while let Ok(m) = ch.read_message().await {
                tracing::debug!(%peer, msg_type = m.msg_type, "rx");
            }
        }
    }

    Ok(())
}
