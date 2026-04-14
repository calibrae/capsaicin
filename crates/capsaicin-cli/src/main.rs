use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use capsaicin_client::{ClientEvent, DisplayEvent, SpiceClient};
use capsaicin_net::{TlsConfig, parse_fingerprint};
use capsaicin_proto::enums::ChannelType;
use capsaicin_server::{Server, default_channels, serve_main_bootstrap};

mod viewer;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
capsaicin — SPICE test client / server / viewer

USAGE:
  capsaicin connect <host:port> [--password PW] [TLS-FLAGS]
  capsaicin serve   <bind:port> [--password PW]
  capsaicin view    <host:port> [--password PW] [TLS-FLAGS]

TLS-FLAGS (client):
  --tls                  force TLS (no plain fallback)
  --no-tls               force plain TCP (skip TLS probe)
  --ca-file PATH         PEM CA bundle to verify server cert
  --fingerprint HEX      pin SHA256 of server leaf cert (aa:bb:.. or 64 hex)
  --insecure             skip certificate verification (loud warning)

  Default: try TLS first, fall back to plain TCP on handshake failure.
  --ca-file / --fingerprint / --insecure imply --tls.

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
            let rest: Vec<String> = args.collect();
            let password = parse_password_flag(rest.iter().cloned()).unwrap_or_default();
            let policy = parse_tls_policy(&rest)?;
            connect_flow(&addr, &password, policy).await
        }
        "serve" => {
            let addr = args.next().unwrap_or_else(|| print_usage_and_exit());
            // Refuse to start without an explicit password decision: the
            // server has no auth other than the password, and silently
            // launching with no auth is a footgun.
            let password = match parse_password_flag(args) {
                Some(p) => {
                    if p.is_empty() {
                        tracing::warn!(
                            "serve: started with empty password — server accepts any client"
                        );
                    }
                    p
                }
                None => {
                    eprintln!(
                        "error: 'serve' requires --password VALUE (use --password '' to \
                         explicitly opt in to no authentication)"
                    );
                    std::process::exit(2);
                }
            };
            serve_flow(&addr, &password).await
        }
        "view" => {
            let addr = args.next().unwrap_or_else(|| print_usage_and_exit());
            let rest: Vec<String> = args.collect();
            let password = parse_password_flag(rest.iter().cloned()).unwrap_or_default();
            let policy = parse_tls_policy(&rest)?;
            // The viewer creates its own tokio runtime in a sidecar
            // thread and owns the main thread for winit. We invoke it
            // synchronously here.
            viewer::run(&addr, &password, policy)
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

/// Returns `Some(pw)` if `--password VAL` was passed (VAL may be the
/// empty string explicitly with `--password ''`), `None` otherwise.
/// Errors out if `--password` was given without a value.
fn parse_password_flag<I: Iterator<Item = String>>(mut args: I) -> Option<String> {
    while let Some(a) = args.next() {
        if a == "--password" || a == "-p" {
            match args.next() {
                Some(v) => return Some(v),
                None => {
                    eprintln!("error: {a} requires a value (use '' for empty)");
                    std::process::exit(2);
                }
            }
        }
    }
    None
}

fn print_usage_and_exit() -> ! {
    print!("{USAGE}");
    std::process::exit(2);
}

/// What transport the user asked for.
#[derive(Debug, Clone)]
pub(crate) enum TlsPolicy {
    /// Try TLS first, fall back to plain TCP on handshake failure. Default.
    Auto(TlsConfig),
    /// Force TLS, no fallback.
    Tls(TlsConfig),
    /// Force plain TCP, skip TLS probe.
    Plain,
}

/// Parse `--tls / --no-tls / --ca-file / --fingerprint / --insecure`.
pub(crate) fn parse_tls_policy(args: &[String]) -> Result<TlsPolicy, Box<dyn std::error::Error>> {
    let mut force_tls = false;
    let mut force_plain = false;
    let mut cfg: Option<TlsConfig> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tls" => force_tls = true,
            "--no-tls" => force_plain = true,
            "--insecure" => cfg = Some(TlsConfig::Insecure),
            "--ca-file" => {
                let v = args
                    .get(i + 1)
                    .ok_or("--ca-file requires a path")?
                    .clone();
                cfg = Some(TlsConfig::CaFile(v));
                i += 1;
            }
            "--fingerprint" => {
                let v = args.get(i + 1).ok_or("--fingerprint requires a value")?;
                cfg = Some(TlsConfig::Fingerprint(parse_fingerprint(v)?));
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    if force_plain && (force_tls || cfg.is_some()) {
        return Err("--no-tls is incompatible with TLS flags".into());
    }
    if force_plain {
        return Ok(TlsPolicy::Plain);
    }
    let cfg = cfg.unwrap_or(TlsConfig::SystemRoots);
    if force_tls || matches!(cfg, TlsConfig::Insecure | TlsConfig::CaFile(_) | TlsConfig::Fingerprint(_)) {
        Ok(TlsPolicy::Tls(cfg))
    } else {
        Ok(TlsPolicy::Auto(cfg))
    }
}

/// Connect honouring the TLS policy. For `Auto`, try TLS first; if the
/// TLS handshake fails (server doesn't speak TLS), reconnect plain.
pub(crate) async fn connect_with_policy(
    addr: &str,
    password: &str,
    policy: TlsPolicy,
) -> Result<SpiceClient, capsaicin_client::ClientError> {
    match policy {
        TlsPolicy::Plain => {
            tracing::info!(%addr, "connecting plain TCP");
            SpiceClient::connect(addr, password).await
        }
        TlsPolicy::Tls(cfg) => {
            tracing::info!(%addr, "connecting TLS");
            SpiceClient::connect_tls(addr, password, cfg).await
        }
        TlsPolicy::Auto(cfg) => {
            tracing::info!(%addr, "auto-detect: trying TLS first");
            match SpiceClient::connect_tls(addr, password, cfg).await {
                Ok(c) => Ok(c),
                Err(e) if looks_like_tls_handshake_failure(&e) => {
                    tracing::info!(%e, "TLS probe failed, falling back to plain TCP");
                    SpiceClient::connect(addr, password).await
                }
                Err(e) => Err(e),
            }
        }
    }
}

/// Heuristic: distinguish "server doesn't speak TLS" from "auth /
/// protocol error after a successful TLS handshake". We only fall back
/// for the former.
fn looks_like_tls_handshake_failure(err: &capsaicin_client::ClientError) -> bool {
    use capsaicin_client::ClientError;
    let s = err.to_string().to_lowercase();
    matches!(err, ClientError::Net(_))
        && (s.contains("tls handshake")
            || s.contains("invalid sni")
            || s.contains("unexpected eof")
            || s.contains("connection reset")
            || s.contains("decrypt"))
}

async fn connect_flow(
    addr: &str,
    password: &str,
    policy: TlsPolicy,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(%addr, "connecting as SPICE client");
    let mut client = connect_with_policy(addr, password, policy).await?;
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
            Ok(Some(ClientEvent::Cursor(evt))) => {
                tracing::debug!(?evt, "cursor event")
            }
            Ok(Some(ClientEvent::MouseMode(mode))) => {
                tracing::info!(?mode, "mouse mode")
            }
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

/// Cap on simultaneously-handled client connections. Without this an
/// attacker can open thousands of TCP sockets, each costing a 4 KiB
/// link-payload allocation + a tokio task, until the host runs out of
/// FDs or memory.
const MAX_CONCURRENT_CONNS: usize = 64;

async fn serve_flow(addr: &str, password: &str) -> Result<(), Box<dyn std::error::Error>> {
    let server = Arc::new(Server::new(password)?);
    let listener = TcpListener::bind(addr).await?;
    let limit = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CONNS));
    tracing::info!(%addr, "capsaicin serving SPICE");

    loop {
        let (stream, peer) = listener.accept().await?;
        let permit = match limit.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(%peer, "rejecting connection: at capacity");
                drop(stream);
                continue;
            }
        };
        let server = server.clone();
        tokio::spawn(async move {
            let _permit = permit; // drop releases the slot
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
    let accepted = match server.accept(stream).await {
        Ok(a) => a,
        Err(capsaicin_server::NetError::Link(
            capsaicin_proto::enums::LinkError::PermissionDenied,
        )) => {
            // Loud, dedicated log for failed auth so brute-force shows
            // up clearly in metrics-style log scraping.
            tracing::warn!(%peer, "auth: password rejected");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
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
            // Always allocate a fresh random session id rather than
            // honouring whatever the client claimed — a sequential or
            // attacker-supplied id could be used to hijack another
            // user's sub-channels.
            let session_id = server.new_session_id();
            let mut ch = accepted.channel;
            serve_main_bootstrap(&mut ch, session_id, &default_channels(), None).await?;
            tracing::info!(%peer, session_id, "main channel attached; idling");
            while let Ok(m) = ch.read_message().await {
                tracing::debug!(%peer, msg_type = m.msg_type, "rx");
            }
            server.end_session(session_id);
        }
        _ => {
            // Reject sub-channel attaches for sessions we don't know
            // about. Without this any peer with the password could
            // open Display/Inputs sub-channels for a session id they
            // guessed (sequential ids made guessing trivial; even with
            // random ids we still want defence-in-depth).
            if !server.is_live_session(accepted.connection_id) {
                tracing::warn!(
                    %peer,
                    connection_id = accepted.connection_id,
                    channel_type = ?accepted.channel_type,
                    "sub-channel attach refused: session id not live"
                );
                return Ok(());
            }
            let mut ch = accepted.channel;
            tracing::info!(%peer, channel_type = ?accepted.channel_type, "sub-channel opened; idling");
            while let Ok(m) = ch.read_message().await {
                tracing::debug!(%peer, msg_type = m.msg_type, "rx");
            }
        }
    }

    Ok(())
}
