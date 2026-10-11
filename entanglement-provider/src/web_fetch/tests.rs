use std::io::{Read, Write};
use std::net::IpAddr;

use super::ssrf::{validate_url, Guard, GuardedResolver};
use super::*;
use reqwest::dns::Resolve;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// Serve `count` connections on a fresh loopback listener, answering the
/// n-th with `respond(n, own_addr)`. Raw sockets, like `endpoint.rs`'s
/// fixture — this crate has no HTTP server dependency.
fn spawn_server(count: usize, respond: impl Fn(usize, &str) -> Vec<u8> + Send + 'static) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr").to_string();
    let own = addr.clone();
    std::thread::spawn(move || {
        for n in 0..count {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(&respond(n, &own));
            let _ = sock.flush();
        }
    });
    addr
}

fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn loopback_fetcher() -> WebFetcher {
    WebFetcher::with_guard(Guard {
        allow_loopback: true,
        fake_dns: vec![("fixture.test".into(), ip("127.0.0.1"))],
    })
    .unwrap()
}

#[test]
fn blocks_every_non_public_ipv4_range() {
    for addr in [
        "0.0.0.0",
        "0.1.2.3",
        "127.0.0.1",
        "127.255.255.254",
        "10.0.0.1",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1",
        "169.254.169.254",
        "169.254.0.1",
        "100.64.0.1",
        "100.127.255.255",
        "224.0.0.1",
        "239.255.255.250",
        "255.255.255.255",
        "240.0.0.1",
        "192.0.0.8",
        "192.0.2.1",
        "198.18.0.1",
        "198.51.100.1",
        "203.0.113.1",
    ] {
        assert!(is_blocked(ip(addr)), "{addr} must be blocked");
    }
}

#[test]
fn blocks_every_non_public_ipv6_range_and_embedded_ipv4() {
    for addr in [
        "::",
        "::1",
        "fc00::1",
        "fd12:3456::1",
        "fe80::1",
        "fec0::1",
        "ff02::1",
        "2001:db8::1",
        "2001::1",
        // IPv4-mapped / -compatible / NAT64 / 6to4 spellings of blocked v4.
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::ffff:169.254.169.254",
        "::ffff:192.168.0.1",
        "::ffff:100.64.0.1",
        "::ffff:0.0.0.0",
        "::127.0.0.1",
        "64:ff9b::a9fe:a9fe",
        "2002:7f00:1::",
    ] {
        assert!(is_blocked(ip(addr)), "{addr} must be blocked");
    }
}

#[test]
fn public_addresses_pass() {
    for addr in [
        "1.1.1.1",
        "93.184.216.34",
        "100.128.0.1",
        "172.32.0.1",
        "2606:4700::1111",
        "::ffff:8.8.8.8",
    ] {
        assert!(!is_blocked(ip(addr)), "{addr} must be allowed");
    }
}

fn check(url: &str) -> Result<()> {
    validate_url(&Url::parse(url).unwrap(), &Guard::default())
}

#[test]
fn url_validation_refuses_blocked_literals_in_every_encoding() {
    for url in [
        "http://127.0.0.1/",
        "http://[::1]/",
        "http://[::ffff:127.0.0.1]/",
        "http://[::ffff:7f00:1]/",
        "http://2130706433/",
        "http://0x7f000001/",
        "http://0177.0.0.1/",
        "http://0x7f.1/",
        "http://127.1/",
        "http://169.254.169.254/latest/meta-data/",
        "http://[fe80::1]/",
        "http://[fd00::1]:8080/",
        "http://0/",
        "http://10.1/",
    ] {
        let err = check(url).expect_err(url);
        assert!(format!("{err}").contains("refusing"), "{url}: {err}");
    }
}

#[test]
fn url_validation_refuses_local_names_schemes_and_credentials() {
    for url in [
        "http://localhost/",
        "http://LOCALHOST./",
        "http://api.localhost/",
        "file:///etc/passwd",
        "ftp://example.com/",
        "gopher://example.com/",
        "https://user:pw@example.com/",
    ] {
        assert!(check(url).is_err(), "{url} must be refused");
    }
    assert!(check("https://example.com/path?q=1").is_ok());
    assert!(check("http://8.8.8.8/").is_ok());
}

#[tokio::test]
async fn resolver_refuses_a_name_whose_dns_answer_is_private() {
    // Real DNS: `localhost` resolves to loopback. The resolver (the layer
    // that guards every hostname the connector dials) must refuse it even
    // though name validation is a separate layer.
    let resolver = GuardedResolver {
        guard: Guard::default(),
    };
    let err = match resolver.resolve("localhost".parse().unwrap()).await {
        Ok(_) => panic!("localhost must be refused"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("blocked address"), "{err}");
}

#[tokio::test]
async fn fetch_refuses_a_public_looking_name_that_resolves_private() {
    let fetcher = WebFetcher::with_guard(Guard {
        allow_loopback: false,
        fake_dns: vec![("rebind.test".into(), ip("127.0.0.1"))],
    })
    .unwrap();
    let err = fetcher
        .fetch("http://rebind.test:9/", 1024)
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("blocked address"), "{err}");
}

#[tokio::test]
async fn fetch_refuses_a_blocked_literal_without_connecting() {
    let fetcher = WebFetcher::new().unwrap();
    let err = fetcher.fetch("http://[::1]:9/", 1024).await.unwrap_err();
    assert!(format!("{err}").contains("refusing"), "{err}");
}

#[tokio::test]
async fn fetch_reads_a_text_body_through_the_guarded_resolver() {
    let addr = spawn_server(1, |_, _| {
        response(
            "200 OK",
            "Content-Type: text/plain; charset=utf-8\r\n",
            b"hello",
        )
    });
    let port = addr.rsplit(':').next().unwrap();
    let out = loopback_fetcher()
        .fetch(&format!("http://fixture.test:{port}/x"), 1024)
        .await
        .unwrap();
    assert_eq!(out.status, 200);
    assert_eq!(out.mime.as_deref(), Some("text/plain"));
    match out.body {
        FetchBody::Text { text, truncated } => {
            assert_eq!(text, "hello");
            assert!(!truncated);
        }
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_caps_the_body_at_max_bytes() {
    let addr = spawn_server(1, |_, _| {
        response("200 OK", "Content-Type: text/plain\r\n", &[b'a'; 5000])
    });
    let out = loopback_fetcher()
        .fetch(&format!("http://{addr}/"), 100)
        .await
        .unwrap();
    match out.body {
        FetchBody::Text { text, truncated } => {
            assert_eq!(text.len(), 100);
            assert!(truncated);
        }
        other => panic!("expected text, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_reports_binary_content_without_reading_it() {
    let addr = spawn_server(1, |_, _| {
        response(
            "200 OK",
            "Content-Type: image/png\r\n",
            &[0x89, b'P', b'N', b'G', 0, 0],
        )
    });
    let out = loopback_fetcher()
        .fetch(&format!("http://{addr}/logo.png"), 1024)
        .await
        .unwrap();
    assert_eq!(out.mime.as_deref(), Some("image/png"));
    assert!(
        matches!(out.body, FetchBody::Binary { size: Some(6) }),
        "{out:?}"
    );
}

#[tokio::test]
async fn fetch_sniffs_untyped_binary_by_nul_bytes() {
    let addr = spawn_server(1, |_, _| response("200 OK", "", b"\x7fELF\0\0\0"));
    let out = loopback_fetcher()
        .fetch(&format!("http://{addr}/"), 1024)
        .await
        .unwrap();
    assert!(matches!(out.body, FetchBody::Binary { .. }), "{out:?}");
}

#[tokio::test]
async fn redirect_to_a_private_address_is_refused() {
    let addr = spawn_server(1, |_, _| {
        response(
            "302 Found",
            "Location: http://169.254.169.254/latest/meta-data/\r\n",
            b"",
        )
    });
    let err = loopback_fetcher()
        .fetch(&format!("http://{addr}/"), 1024)
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("169.254.169.254"), "{err}");
}

#[tokio::test]
async fn redirects_are_followed_and_capped() {
    let target = spawn_server(1, |_, _| {
        response("200 OK", "Content-Type: text/plain\r\n", b"landed")
    });
    let hop = spawn_server(1, move |_, _| {
        response(
            "301 Moved",
            &format!("Location: http://{target}/end\r\n"),
            b"",
        )
    });
    let out = loopback_fetcher()
        .fetch(&format!("http://{hop}/start"), 1024)
        .await
        .unwrap();
    assert!(out.final_url.ends_with("/end"), "{}", out.final_url);
    assert!(matches!(out.body, FetchBody::Text { ref text, .. } if text == "landed"));

    let looping = spawn_server(MAX_REDIRECTS + 1, |n, own| {
        response("302 Found", &format!("Location: http://{own}/{n}\r\n"), b"")
    });
    let err = loopback_fetcher()
        .fetch(&format!("http://{looping}/"), 1024)
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("too many redirects"), "{err}");
}

#[test]
fn textual_mime_classification() {
    for m in [
        "text/html",
        "text/plain",
        "application/json",
        "application/ld+json",
        "image/svg+xml",
    ] {
        assert!(is_textual(m), "{m}");
    }
    for m in [
        "image/png",
        "application/pdf",
        "application/octet-stream",
        "video/mp4",
    ] {
        assert!(!is_textual(m), "{m}");
    }
}
