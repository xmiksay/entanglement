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
        // IPv4-translated ::ffff:0:0/96 and NAT64 local-use 64:ff9b:1::/48
        // (both the /48 split embedding and a /96 tail).
        "::ffff:0:7f00:1",
        "::ffff:0:a9fe:a9fe",
        "64:ff9b:1:7f00:0:100::",
        "64:ff9b:1:a9fe:a9:fe00::",
        "64:ff9b:1::7f00:1",
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
        "::ffff:0:808:808",
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
        "http://[::ffff:0:7f00:1]/",
        "http://[64:ff9b:1:7f00:0:100::]/",
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

#[test]
fn normalize_url_is_what_gets_fetched_and_graded() {
    for raw in [
        "HTTPS://PASTEBIN.COM/x",
        "https://pastebin.com./x",
        "https://pastebin.com:443/x",
        " https://pastebin.com/x",
        "https://paste\tbin.com/x",
    ] {
        assert_eq!(
            normalize_url(raw).as_deref(),
            Some("https://pastebin.com/x"),
            "{raw:?}"
        );
    }
    assert_eq!(normalize_url("not a url"), None);
}

#[tokio::test]
async fn check_hop_sees_each_normalized_redirect_target_and_can_stop_it() {
    let target = spawn_server(1, |_, _| {
        response("200 OK", "Content-Type: text/plain\r\n", b"ok")
    });
    let hop = spawn_server(2, move |_, _| {
        response(
            "302 Found",
            &format!("Location: HTTP://{target}/next\r\n"),
            b"",
        )
    });
    let seen = std::sync::Mutex::new(Vec::new());
    let out = loopback_fetcher()
        .fetch_checked(&format!("http://{hop}/"), 1024, |url| {
            seen.lock().unwrap().push(url);
            async { Ok(()) }
        })
        .await
        .unwrap();
    assert!(matches!(out.body, FetchBody::Text { ref text, .. } if text == "ok"));
    let seen = seen.into_inner().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].starts_with("http://127.0.0.1:") && seen[0].ends_with("/next"),
        "{seen:?}"
    );

    let err = loopback_fetcher()
        .fetch_checked(&format!("http://{hop}/"), 1024, |url| async move {
            anyhow::bail!("hop {url} refused")
        })
        .await
        .unwrap_err();
    assert!(format!("{err}").contains("refused"), "{err}");
}

/// [`spawn_server`], also handing each raw request (head + body) back.
fn spawn_recording(
    count: usize,
    respond: impl Fn(usize, &str) -> Vec<u8> + Send + 'static,
) -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr").to_string();
    let own = addr.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for n in 0..count {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(read) = sock.read(&mut buf) {
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..read]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let len = text[..head_end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if raw.len() >= head_end + 4 + len {
                        break;
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&raw).to_string());
            let _ = sock.write_all(&respond(n, &own));
            let _ = sock.flush();
        }
    });
    (addr, rx)
}

fn http_req(method: &str, url: String, body: Option<&str>) -> HttpRequest {
    HttpRequest {
        method: method.to_string(),
        url,
        headers: vec![("X-Trace".to_string(), "t1".to_string())],
        body: body.map(str::to_string),
        max_bytes: 1024,
    }
}

/// ADR-0227: credentials are asked for per hop, so a redirect to another
/// host can go out without them while the caller's own headers and a
/// 307's method + body survive.
#[tokio::test]
async fn request_sends_credentials_only_where_the_per_hop_seam_says() {
    let (target, target_rx) = spawn_recording(1, |_, _| {
        response("200 OK", "Content-Type: text/plain\r\n", b"landed")
    });
    let port = target.rsplit(':').next().unwrap().to_string();
    let (hop, hop_rx) = spawn_recording(1, move |_, _| {
        response(
            "307 Temporary Redirect",
            &format!("Location: http://fixture.test:{port}/end\r\n"),
            b"",
        )
    });
    let out = loopback_fetcher()
        .request(
            http_req("post", format!("http://{hop}/start"), Some("{\"q\":1}")),
            |_, _| async { Ok(()) },
            |url| {
                if url.starts_with("http://127.0.0.1:") {
                    vec![("Authorization".into(), "Bearer s3cret".into())]
                } else {
                    Vec::new()
                }
            },
        )
        .await
        .unwrap();
    assert_eq!(out.status, 200);
    let first = hop_rx.recv().unwrap();
    assert!(first.starts_with("POST /start"), "{first}");
    assert!(
        first
            .to_ascii_lowercase()
            .contains("authorization: bearer s3cret"),
        "{first}"
    );
    assert!(first.ends_with("{\"q\":1}"), "{first}");
    let second = target_rx.recv().unwrap();
    assert!(
        second.starts_with("POST /end"),
        "a 307 keeps the method: {second}"
    );
    assert!(
        second.ends_with("{\"q\":1}"),
        "a 307 keeps the body: {second}"
    );
    assert!(
        second.to_ascii_lowercase().contains("x-trace: t1"),
        "{second}"
    );
    assert!(
        !second.contains("s3cret"),
        "cross-host hop must not carry credentials: {second}"
    );
}

#[tokio::test]
async fn a_303_becomes_a_bodyless_get_and_check_hop_sees_the_new_method() {
    let (target, target_rx) = spawn_recording(1, |_, _| {
        response("200 OK", "Content-Type: text/plain\r\n", b"ok")
    });
    let hop = spawn_server(1, move |_, _| {
        response(
            "303 See Other",
            &format!("Location: http://{target}/done\r\n"),
            b"",
        )
    });
    let seen = std::sync::Mutex::new(Vec::new());
    loopback_fetcher()
        .request(
            http_req("POST", format!("http://{hop}/"), Some("payload")),
            |method, url| {
                seen.lock().unwrap().push(format!("{method} {url}"));
                async { Ok(()) }
            },
            |_| Vec::new(),
        )
        .await
        .unwrap();
    let got = target_rx.recv().unwrap();
    assert!(got.starts_with("GET /done"), "{got}");
    assert!(!got.contains("payload"), "{got}");
    let seen = seen.into_inner().unwrap();
    assert!(seen[0].starts_with("GET http://127.0.0.1:"), "{seen:?}");
}

#[test]
fn methods_parse_case_insensitively_from_a_closed_set() {
    assert_eq!(parse_method("patch").unwrap(), "PATCH");
    assert!(parse_method("TRACE").is_err());
    assert!(has_userinfo("https://u:p@a.io/"));
    assert!(
        has_userinfo("https:u:p@a.io/"),
        "the scheme-relative spelling too"
    );
    assert!(has_userinfo("https://u@a.io/"));
    assert!(!has_userinfo("https://a.io/x?u=me@x"));
    assert!(!has_userinfo("not a url"));
    assert_eq!(
        scheme_and_host("HTTPS://API.GitHub.com./x?y=1"),
        Some(("https".to_string(), "api.github.com".to_string()))
    );
}
