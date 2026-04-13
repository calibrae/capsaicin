//! End-to-end integration tests: capsaicin-server vs capsaicin-net client
//! over a real loopback TCP socket.

use std::sync::Arc;
use std::time::Duration;

use capsaicin_net::{MainConnection, connect_sub_channel};
use capsaicin_proto::caps::{self, CapSet};
use capsaicin_proto::enums::{ChannelType, LinkError};
use capsaicin_proto::inputs::{
    MouseMotion, MousePosition, button_mask, client_msg as inputs_client,
};
use capsaicin_proto::types::{ChannelId, Writer};
use capsaicin_server::{Server, default_channels, serve_main_bootstrap};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::timeout;

/// Bind to 127.0.0.1:0, return both the listener and its concrete address.
async fn bind_loopback() -> (TcpListener, std::net::SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

/// Drive the server side of a single test fixture: accept main + sub
/// channels, send `CHANNELS_LIST`, then hold connections open until the
/// client closes them.
async fn run_test_server(listener: TcpListener, server: Arc<Server>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let server = server.clone();
        tokio::spawn(async move {
            let accepted = match server.accept(stream).await {
                Ok(a) => a,
                Err(_) => return,
            };
            match accepted.channel_type {
                ChannelType::Main => {
                    let session_id = if accepted.connection_id == 0 {
                        server.new_session_id()
                    } else {
                        accepted.connection_id
                    };
                    let mut ch = accepted.channel;
                    let _ = serve_main_bootstrap(&mut ch, session_id, &default_channels(), None)
                        .await;
                    // Keep open to simulate a real server.
                    loop {
                        if ch.read_message().await.is_err() {
                            break;
                        }
                    }
                }
                _ => {
                    let mut ch = accepted.channel;
                    loop {
                        if ch.read_message().await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
}

#[tokio::test]
async fn client_completes_main_bootstrap_and_discovers_channels() {
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("letmein").unwrap());
    let server_for_task = server.clone();
    let server_handle = tokio::spawn(async move {
        run_test_server(listener, server_for_task).await;
    });

    let addr_s = addr.to_string();
    let main = timeout(
        Duration::from_secs(5),
        MainConnection::connect(&addr_s, "letmein"),
    )
    .await
    .expect("client connect timed out")
    .expect("client bootstrap failed");

    assert!(main.session_id > 0, "server must allocate a session id");
    assert!(main.channel.mini_header());

    let advertised: Vec<u8> = main
        .available_channels
        .iter()
        .map(|c| c.channel_type)
        .collect();
    assert!(advertised.contains(&(ChannelType::Main as u8)));
    assert!(advertised.contains(&(ChannelType::Display as u8)));
    assert!(advertised.contains(&(ChannelType::Inputs as u8)));

    drop(main);
    server_handle.abort();
}

#[tokio::test]
async fn client_can_open_inputs_and_display_sub_channels() {
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());
    let sv = server.clone();
    let server_handle = tokio::spawn(async move { run_test_server(listener, sv).await });

    let addr_s = addr.to_string();
    let main = MainConnection::connect(&addr_s, "pw").await.unwrap();

    let inputs =
        connect_sub_channel(&addr_s, main.session_id, ChannelType::Inputs, 0, "pw", CapSet::new())
            .await
            .expect("inputs sub-channel");
    let display = connect_sub_channel(
        &addr_s,
        main.session_id,
        ChannelType::Display,
        0,
        "pw",
        CapSet::new(),
    )
    .await
    .expect("display sub-channel");

    assert!(inputs.mini_header());
    assert!(display.mini_header());

    server_handle.abort();
}

#[tokio::test]
async fn input_events_round_trip_over_inputs_channel() {
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("pw").unwrap());
    let sv = server.clone();

    // Hand the oneshot sender only to the task that will receive inputs.
    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));
    let server_handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let sv2 = sv.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let accepted = sv2.accept(stream).await.unwrap();
                if accepted.channel_type == ChannelType::Main {
                    let session_id = if accepted.connection_id == 0 {
                        sv2.new_session_id()
                    } else {
                        accepted.connection_id
                    };
                    let mut ch = accepted.channel;
                    serve_main_bootstrap(&mut ch, session_id, &default_channels(), None)
                        .await
                        .unwrap();
                    loop {
                        if ch.read_message().await.is_err() {
                            break;
                        }
                    }
                } else if accepted.channel_type == ChannelType::Inputs {
                    let mut ch = accepted.channel;
                    let mut collected = Vec::new();
                    while collected.len() < 2 {
                        let m = match ch.read_message().await {
                            Ok(m) => m,
                            Err(_) => break,
                        };
                        collected.push((m.msg_type, m.body.to_vec()));
                    }
                    if let Some(tx) = tx.lock().await.take() {
                        let _ = tx.send(collected);
                    }
                }
            });
        }
    });

    let addr_s = addr.to_string();
    let main = MainConnection::connect(&addr_s, "pw").await.unwrap();
    let mut inputs = connect_sub_channel(
        &addr_s,
        main.session_id,
        ChannelType::Inputs,
        0,
        "pw",
        CapSet::new(),
    )
    .await
    .unwrap();

    // Send a mouse motion and a mouse position.
    let mut w = Writer::new();
    MouseMotion {
        dx: 10,
        dy: -5,
        buttons_state: 0,
    }
    .encode(&mut w);
    inputs
        .write_message(inputs_client::INPUTS_MOUSE_MOTION, w.as_slice())
        .await
        .unwrap();

    let mut w = Writer::new();
    MousePosition {
        x: 640,
        y: 480,
        buttons_state: button_mask::LEFT,
        display_id: 0,
    }
    .encode(&mut w);
    inputs
        .write_message(inputs_client::INPUTS_MOUSE_POSITION, w.as_slice())
        .await
        .unwrap();

    let received = timeout(Duration::from_secs(5), rx).await.unwrap().unwrap();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].0, inputs_client::INPUTS_MOUSE_MOTION);
    let motion = MouseMotion::decode(&received[0].1).unwrap();
    assert_eq!(motion.dx, 10);
    assert_eq!(motion.dy, -5);

    assert_eq!(received[1].0, inputs_client::INPUTS_MOUSE_POSITION);
    let pos = MousePosition::decode(&received[1].1).unwrap();
    assert_eq!(pos.x, 640);
    assert_eq!(pos.y, 480);
    assert_eq!(pos.buttons_state, button_mask::LEFT);

    server_handle.abort();
}

#[tokio::test]
async fn server_rejects_wrong_password_over_tcp() {
    let (listener, addr) = bind_loopback().await;
    let server = Arc::new(Server::new("correct").unwrap());
    let sv = server.clone();
    let server_handle = tokio::spawn(async move { run_test_server(listener, sv).await });

    let addr_s = addr.to_string();
    let res = MainConnection::connect(&addr_s, "wrong").await;
    match res {
        Err(capsaicin_net::NetError::Link(LinkError::PermissionDenied)) => {}
        Err(other) => panic!("expected PermissionDenied, got {other:?}"),
        Ok(_) => panic!("wrong password should fail"),
    }

    server_handle.abort();
}

/// Sanity-check: ensure the common caps the client advertises by default
/// are the ones the proto crate documents, catching any drift.
#[test]
fn client_defaults_advertise_auth_spice_and_mini_header() {
    let opts = capsaicin_net::LinkOptions::new(ChannelType::Main);
    assert!(opts.common_caps.has(caps::common::AUTH_SPICE));
    assert!(opts.common_caps.has(caps::common::MINI_HEADER));
}

#[tokio::test]
async fn channels_list_includes_default_three() {
    let channels = default_channels();
    let types: Vec<u8> = channels.iter().map(|c: &ChannelId| c.channel_type).collect();
    assert_eq!(
        types,
        vec![
            ChannelType::Main as u8,
            ChannelType::Display as u8,
            ChannelType::Inputs as u8,
        ]
    );
}
