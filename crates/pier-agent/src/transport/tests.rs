use super::*;

fn setup() -> (
    tempfile::TempDir,
    Arc<Transport>,
    mpsc::UnboundedReceiver<Message>,
) {
    let root = tempfile::tempdir().unwrap();
    let address = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let token_file = root.path().join("token");
    fs::write(&token_file, "test-token").unwrap();
    let config = Config {
        agent_id: "agent".into(),
        token_file,
        state_dir: root.path().join("state"),
        connection_mode: ConnectionMode::ControllerToAgent,
        listen: Some(address),
        controller_tcp: String::new(),
        heartbeat_seconds: 15,
        runtime: Default::default(),
    };
    let (events, receiver) = mpsc::unbounded_channel();
    (root, Transport::new(config, events), receiver)
}

async fn attach(transport: &Transport, session: &str, id: &str, purpose: Purpose) -> Wire {
    let io = secure::connect(
        &transport.config.listen.unwrap().to_string(),
        purpose,
        "agent",
        &secure::token_key("test-token"),
    )
    .await
    .unwrap();
    let mut wire = pier_protocol::framed(io);
    pier_protocol::send(
        &mut wire,
        &Message::Channel {
            session: session.into(),
            id: id.into(),
            purpose,
        },
    )
    .await
    .unwrap();
    wire
}

#[tokio::test]
async fn passive_channels_require_current_session_and_are_cancelled_on_disconnect() {
    let (_root, transport, mut events) = setup();
    let _controls = transport.bind().await.unwrap();
    assert!(transport.open(Purpose::Artifact).await.is_err());
    transport
        .begin("session".into(), CancellationToken::new())
        .unwrap();
    let worker = transport.clone();
    let request = tokio::spawn(async move { worker.open(Purpose::Artifact).await });
    let Message::OpenChannel {
        id,
        session,
        purpose,
    } = events.recv().await.unwrap()
    else {
        panic!("channel request expected");
    };
    let mut wrong = attach(&transport, "old-session", &id, purpose).await;
    assert!(
        timeout(Duration::from_secs(2), pier_protocol::receive(&mut wrong))
            .await
            .unwrap()
            .is_err()
    );
    let mut wrong = attach(&transport, &session, &id, Purpose::Upgrade).await;
    assert!(
        timeout(Duration::from_secs(2), pier_protocol::receive(&mut wrong))
            .await
            .unwrap()
            .is_err()
    );
    let mut controller = attach(&transport, &session, &id, purpose).await;
    let mut agent = request.await.unwrap().unwrap();
    pier_protocol::send(&mut controller, &Message::Ping)
        .await
        .unwrap();
    assert!(matches!(
        pier_protocol::receive(&mut agent).await.unwrap(),
        Message::Ping
    ));
    let mut replay = attach(&transport, &session, &id, purpose).await;
    assert!(
        timeout(Duration::from_secs(2), pier_protocol::receive(&mut replay))
            .await
            .unwrap()
            .is_err()
    );
    transport.disconnect();
    assert!(pier_protocol::receive(&mut agent).await.is_err());
    assert!(!transport.current("session"));
    transport.stop();
}

#[tokio::test]
async fn passive_listener_rejects_bad_keys_identity_and_duplicate_control() {
    let (_root, transport, _events) = setup();
    let mut controls = transport.bind().await.unwrap().unwrap();
    let endpoint = transport.config.listen.unwrap().to_string();
    let key = secure::token_key("test-token");
    assert!(
        secure::connect(&endpoint, Purpose::Control, "agent", &[9; 32])
            .await
            .is_err()
    );
    assert!(
        secure::connect(&endpoint, Purpose::Control, "wrong", &key)
            .await
            .is_err()
    );
    let _first = secure::connect(&endpoint, Purpose::Control, "agent", &key)
        .await
        .unwrap();
    let _accepted = controls.recv().await.unwrap();
    let mut second = pier_protocol::framed(
        secure::connect(&endpoint, Purpose::Control, "agent", &key)
            .await
            .unwrap(),
    );
    assert!(
        timeout(Duration::from_secs(2), pier_protocol::receive(&mut second))
            .await
            .unwrap()
            .is_err()
    );
    transport.stop();
}

#[tokio::test(start_paused = true)]
async fn pending_requests_expire_and_disconnect_releases_waiters() {
    let (_root, transport, mut events) = setup();
    transport
        .begin("session".into(), CancellationToken::new())
        .unwrap();
    let worker = transport.clone();
    let request = tokio::spawn(async move { worker.open(Purpose::Upgrade).await });
    events.recv().await.unwrap();
    tokio::time::advance(Duration::from_secs(11)).await;
    assert!(request.await.unwrap().is_err());
    assert!(
        transport
            .session
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .pending
            .is_empty()
    );
    let worker = transport.clone();
    let request = tokio::spawn(async move { worker.open(Purpose::Upgrade).await });
    events.recv().await.unwrap();
    transport.disconnect();
    assert!(request.await.unwrap().is_err());
}
