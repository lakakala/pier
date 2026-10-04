use super::*;
use tokio::net::TcpListener;

#[test]
fn strict_urls_redaction_and_three_way_patch() {
    for value in [
        "socks5://localhost:1080",
        "socks5://[::1]:1080",
        "socks5://u%40x:p%3Aa@host:1080",
    ] {
        let proxy = Proxy::try_from(value.to_owned()).unwrap();
        assert_eq!(format!("{proxy:?}"), "Proxy([redacted])");
        assert_eq!(
            serde_json::from_str::<Proxy>(&serde_json::to_string(&proxy).unwrap()).unwrap(),
            proxy
        );
    }
    for value in [
        "http://host:1080",
        "socks5h://host:1080",
        "socks5://host",
        "socks5://host:0",
        "socks5://host:1080/",
        "socks5://host:1080/a/..",
        "socks5://host:1080?q",
        "socks5://host:1080#f",
        "socks5://host:1080\n",
        "socks5://u@host:1080",
        "socks5://:p@host:1080",
        "socks5://u:@host:1080",
        "socks5://u:%gg@host:1080",
        "socks5://u:%ff@host:1080",
        "socks5://u:%00@host:1080",
        "socks5://u:p@host%2Fbad:1080",
        "socks5://u:p@other@host:1080",
    ] {
        let error = Proxy::try_from(value.to_owned()).unwrap_err().to_string();
        assert!(!error.contains(value));
    }
    assert!(Proxy::try_from(format!("socks5://u:{}@host:1080", "x".repeat(256))).is_err());
    #[derive(Deserialize)]
    struct Input {
        #[serde(default)]
        proxy: Patch,
    }
    let old = Some(Proxy::try_from("socks5://host:1080".to_owned()).unwrap());
    let parse = |json| serde_json::from_str::<Input>(json).unwrap().proxy;
    assert_eq!(parse("{}").apply(old.clone()), old);
    assert_eq!(parse(r#"{"proxy":null}"#).apply(old.clone()), None);
    assert_eq!(
        parse(r#"{"proxy":"socks5://next:1080"}"#).apply(old),
        Some(Proxy::try_from("socks5://next:1080".to_owned()).unwrap())
    );
}

type Step = (Vec<u8>, Vec<u8>);
async fn fixture(steps: Vec<Step>) -> (String, tokio::task::JoinHandle<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let task = tokio::spawn(async move {
        let (mut io, _) = listener.accept().await.unwrap();
        for (expected, reply) in steps {
            let mut data = vec![0; expected.len()];
            io.read_exact(&mut data).await.unwrap();
            assert_eq!(data, expected);
            // Deliberately fragment every negotiation field across writes.
            for byte in reply {
                io.write_all(&[byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
        }
        io
    });
    (address, task)
}

fn domain_request() -> Vec<u8> {
    let mut bytes = vec![5, 1, 0, 3, 18];
    bytes.extend_from_slice(b"only.at.proxy.test");
    bytes.extend_from_slice(&7444_u16.to_be_bytes());
    bytes
}

#[tokio::test]
async fn remote_dns_addresses_authentication_and_exact_fragmented_reply() {
    let mut ipv4 = vec![5, 1, 0, 1, 192, 0, 2, 1];
    ipv4.extend_from_slice(&7444_u16.to_be_bytes());
    let mut ipv6 = vec![5, 1, 0, 4];
    ipv6.extend_from_slice(
        &"2001:db8::1"
            .parse::<std::net::Ipv6Addr>()
            .unwrap()
            .octets(),
    );
    ipv6.extend_from_slice(&7444_u16.to_be_bytes());
    for (endpoint, request, bound) in [
        (
            "only.at.proxy.test:7444",
            domain_request(),
            vec![5, 0, 0, 1, 0, 0, 0, 0, 0, 0],
        ),
        ("192.0.2.1:7444", ipv4, vec![5, 0, 0, 3, 1, b'x', 0, 0]),
        (
            "[2001:db8::1]:7444",
            ipv6,
            [vec![5, 0, 0, 4], vec![0; 18]].concat(),
        ),
    ] {
        for auth in [false, true] {
            let method = if auth { 2 } else { 0 };
            let mut steps = vec![(vec![5, 1, method], vec![5, method])];
            if auth {
                steps.push((
                    [vec![1, 3], b"u@x".to_vec(), vec![6], b"secret".to_vec()].concat(),
                    vec![1, 0],
                ));
            }
            steps.push((request.clone(), [bound.clone(), b"NEXT".to_vec()].concat()));
            let (address, server) = fixture(steps).await;
            let proxy = Proxy::try_from(format!(
                "socks5://{}{address}",
                if auth { "u%40x:%73ecret@" } else { "" }
            ))
            .unwrap();
            let mut tunnel = tunnel(endpoint, &proxy).await.unwrap();
            let mut next = [0; 4];
            tunnel.read_exact(&mut next).await.unwrap();
            assert_eq!(&next, b"NEXT");
            drop(server.await.unwrap());
        }
    }
}

#[tokio::test]
async fn noise_authenticates_inside_tunnel_for_every_channel() {
    for purpose in [
        Purpose::Enrollment,
        Purpose::Control,
        Purpose::Artifact,
        Purpose::Upgrade,
        Purpose::Terminal,
    ] {
        let (address, server) = fixture(vec![
            (vec![5, 1, 0], vec![5, 0]),
            (domain_request(), vec![5, 0, 0, 1, 0, 0, 0, 0, 0, 0]),
        ])
        .await;
        let peer = tokio::spawn(async move {
            let mut socket = server.await.unwrap();
            let (prelude, raw) = secure::read_prelude(&mut socket).await.unwrap();
            assert_eq!(prelude.purpose, purpose);
            let mut io = secure::accept(socket, &raw, &secure::token_key("key"))
                .await
                .unwrap();
            io.write_all(b"encrypted").await.unwrap();
            io.flush().await.unwrap();
        });
        let proxy = Proxy::try_from(format!("socks5://{address}")).unwrap();
        let mut io = connect(
            "only.at.proxy.test:7444",
            Some(&proxy),
            purpose,
            "agent",
            &secure::token_key("key"),
        )
        .await
        .unwrap();
        let mut data = [0; 9];
        io.read_exact(&mut data).await.unwrap();
        assert_eq!(&data, b"encrypted");
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn refuses_bad_authentication_and_malformed_or_denied_replies() {
    let auth = [vec![1, 1], b"u".to_vec(), vec![1], b"p".to_vec()].concat();
    let cases = vec![
        (false, vec![(vec![5, 1, 0], vec![5, 255])]),
        (false, vec![(vec![5, 1, 0], vec![4, 0])]),
        (true, vec![(vec![5, 1, 2], vec![5, 0])]),
        (true, vec![(vec![5, 1, 2], vec![5, 2]), (auth, vec![1, 1])]),
    ];
    let mut cases = cases;
    for reply in [
        vec![5, 5, 0, 1],
        vec![4, 0, 0, 1],
        vec![5, 0, 1, 1],
        vec![5, 0, 0, 9],
        vec![5, 0, 0, 3, 0],
        vec![5, 0, 0, 1, 0],
    ] {
        cases.push((
            false,
            vec![(vec![5, 1, 0], vec![5, 0]), (domain_request(), reply)],
        ));
    }
    for (auth, steps) in cases {
        let (address, server) = fixture(steps).await;
        let closer = tokio::spawn(async move {
            drop(server.await.unwrap());
        });
        let proxy = Proxy::try_from(format!(
            "socks5://{}{address}",
            if auth { "u:p@" } else { "" }
        ))
        .unwrap();
        let error = connect(
            "only.at.proxy.test:7444",
            Some(&proxy),
            Purpose::Control,
            "agent",
            &[0; 32],
        )
        .await
        .err()
        .unwrap();
        assert!(error.is::<DialError>());
        assert!(!error.to_string().contains(&address));
        closer.await.unwrap();
    }
}

#[tokio::test]
async fn negotiation_and_noise_share_one_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = Proxy::try_from(format!("socks5://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        connect(
            "only.at.proxy.test:7444",
            Some(&proxy),
            Purpose::Control,
            "agent",
            &[0; 32],
        )
        .await
    });
    let (mut io, _) = listener.accept().await.unwrap();
    let mut greeting = [0; 3];
    io.read_exact(&mut greeting).await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(8)).await;
    // Resume while exchanging actual OS socket bytes so virtual time does not
    // automatically jump to the deadline while waiting for epoll.
    tokio::time::resume();
    io.write_all(&[5, 0]).await.unwrap();
    let mut request = vec![0; domain_request().len()];
    io.read_exact(&mut request).await.unwrap();
    io.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
    secure::read_prelude(&mut io).await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(
        task.await
            .unwrap()
            .err()
            .unwrap()
            .to_string()
            .contains("超时")
    );
    tokio::time::resume();
}

#[tokio::test]
async fn total_timeout_and_task_cancellation_close_the_proxy_socket() {
    for cancel in [false, true] {
        let (address, peer) = fixture(vec![(vec![5, 1, 0], vec![])]).await;
        let proxy = Proxy::try_from(format!("socks5://{address}")).unwrap();
        let task = tokio::spawn(async move {
            connect(
                "only.at.proxy.test:7444",
                Some(&proxy),
                Purpose::Control,
                "agent",
                &[0; 32],
            )
            .await
        });
        let mut io = peer.await.unwrap();
        if cancel {
            task.abort();
            assert!(task.await.err().unwrap().is_cancelled());
        } else {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(10)).await;
            assert!(
                task.await
                    .unwrap()
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("超时")
            );
            tokio::time::resume();
        }
        assert_eq!(io.read(&mut [0]).await.unwrap(), 0);
    }
}
