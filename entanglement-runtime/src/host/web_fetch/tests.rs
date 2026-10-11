use super::*;

fn text_outcome(mime: &str, body: &str) -> FetchOutcome {
    FetchOutcome {
        final_url: "https://example.com/page".into(),
        status: 200,
        mime: Some(mime.into()),
        body: FetchBody::Text {
            text: body.into(),
            truncated: false,
        },
    }
}

#[test]
fn capability_is_exec_so_modes_grade_it_like_curl() {
    let tool = WebFetchTool::new().unwrap();
    assert_eq!(tool.capabilities(), &[Capability::Exec]);
}

#[tokio::test]
async fn html_becomes_markdown_without_scripts_or_styles() {
    let html = "<html><head><title>t</title><style>p{}</style></head><body>\
                <h1>Title</h1><script>alert(1)</script>\
                <p>Hello <a href=\"https://x.test/a\">link</a></p></body></html>";
    let out = render(text_outcome("text/html", html), Format::Markdown, 0)
        .await
        .unwrap();
    assert!(out.contains("# Title"), "{out}");
    assert!(out.contains("[link](https://x.test/a)"), "{out}");
    assert!(!out.contains("alert"), "{out}");
    assert!(!out.contains("p{}"), "{out}");
    assert!(
        out.starts_with("URL: https://example.com/page\nStatus: 200\n"),
        "{out}"
    );
}

#[tokio::test]
async fn text_format_drops_link_and_image_targets() {
    let html = "<p>See <a href=\"https://x.test/a\">docs</a> <img src=\"i.png\" alt=\"pic\"></p>";
    let out = render(text_outcome("text/html", html), Format::Text, 0)
        .await
        .unwrap();
    assert!(out.contains("See docs pic"), "{out}");
    assert!(!out.contains("x.test"), "{out}");
}

#[tokio::test]
async fn raw_format_and_non_html_text_pass_through() {
    let html = "<p>x</p>";
    let raw = render(text_outcome("text/html", html), Format::Raw, 0)
        .await
        .unwrap();
    assert!(raw.ends_with("<p>x</p>"), "{raw}");
    let json = render(
        text_outcome("application/json", "{\"a\":1}"),
        Format::Markdown,
        0,
    )
    .await
    .unwrap();
    assert!(json.ends_with("{\"a\":1}"), "{json}");
}

#[tokio::test]
async fn binary_body_is_a_placeholder_never_inlined() {
    let outcome = FetchOutcome {
        final_url: "https://example.com/logo.png".into(),
        status: 200,
        mime: Some("image/png".into()),
        body: FetchBody::Binary { size: Some(1234) },
    };
    let out = render(outcome, Format::Markdown, 0).await.unwrap();
    assert_eq!(
        out,
        "[binary omitted: image/png, 1234 bytes, https://example.com/logo.png] (HTTP 200)"
    );
}

#[tokio::test]
async fn download_truncation_is_flagged_in_the_header() {
    let mut outcome = text_outcome("text/plain", "abc");
    outcome.body = FetchBody::Text {
        text: "abc".into(),
        truncated: true,
    };
    let out = render(outcome, Format::Markdown, 0).await.unwrap();
    assert!(out.contains("byte cap"), "{out}");
}

#[test]
fn long_content_pages_at_the_output_cap_on_char_boundaries() {
    let content = "é".repeat(MAX_OUTPUT_BYTES); // 2 bytes each → two pages
    let first = page(&content, 0).unwrap();
    assert!(
        first.contains("call again with offset="),
        "{}",
        &first[first.len() - 80..]
    );
    let next: usize = first
        .rsplit("offset=")
        .next()
        .and_then(|s| s.trim_end_matches(" for more]").parse().ok())
        .unwrap();
    assert!(content.is_char_boundary(next));
    let second = page(&content, next).unwrap();
    assert!(!second.contains("call again"), "last page has no pointer");
    // An odd offset lands mid-char and snaps back instead of panicking.
    assert!(page(&content, 1).unwrap().starts_with('é'));
    assert!(page(&content, content.len() + 1).is_err());
}

#[tokio::test]
async fn run_refuses_private_targets_before_any_request() {
    let tool = WebFetchTool::new().unwrap();
    for url in [
        "http://127.0.0.1/",
        "http://[::1]/",
        "http://2130706433/",
        "http://169.254.169.254/latest/meta-data/",
        "http://localhost:8080/",
    ] {
        let input = serde_json::json!({ "url": url }).to_string();
        assert!(tool.run(&input).await.is_err(), "{url} must be refused");
    }
    assert!(tool.run(r#"{"url":"file:///etc/passwd"}"#).await.is_err());
    assert!(tool.run(r#"{"nope":1}"#).await.is_err());
}

#[test]
fn builtin_modes_grade_web_fetch_like_an_outbound_command() {
    use entanglement_core::Permission;
    let table = crate::mode::ModeTable::builtin().unwrap();
    let caps = WebFetchTool::new().unwrap().capabilities();
    let url = Some("https://docs.rs/tokio");
    let grade = |mode: &str| {
        table
            .get(mode)
            .unwrap()
            .resolve(WEB_FETCH_TOOL, caps, url, None)
    };
    assert_eq!(grade("build"), Permission::Allow);
    assert_eq!(grade("research"), Permission::Ask);
    assert_eq!(grade("plan"), Permission::Ask);
    assert_eq!(grade("auto"), Permission::Ask);
}

#[test]
fn grading_uses_the_normalized_url_so_spelling_tricks_cannot_dodge_a_deny() {
    use crate::mode::{Limits, Mode, Rules};
    use entanglement_core::Permission;
    let mode = Mode {
        name: "build".into(),
        default: Permission::Ask,
        rules: Rules::from_lists(
            &["web_fetch(https://pastebin.com/*)".to_string()],
            &["web_fetch".to_string()],
            &[],
        ),
        limits: Limits::default(),
        sandbox: None,
        sandbox_network: false,
    };
    let caps = [Capability::Exec];
    for raw in [
        "HTTPS://PASTEBIN.COM/x",
        "https://pastebin.com./x",
        "https://pastebin.com:443/x",
        " https://pastebin.com/x",
    ] {
        let input = serde_json::json!({ "url": raw }).to_string();
        let arg = crate::permission::permission_arg(WEB_FETCH_TOOL, &input);
        let grade = mode.resolve(WEB_FETCH_TOOL, &caps, arg.as_deref(), None);
        assert_eq!(grade, Permission::Deny, "{raw:?} must stay denied");
    }
}

#[tokio::test]
async fn deeply_nested_html_falls_back_to_plain_text_without_crashing() {
    // ~100 KB of nesting: overflowed the stack and aborted the process
    // before the depth gate.
    let html = format!(
        "{}deep text{}",
        "<div>".repeat(20_000),
        "</div>".repeat(20_000)
    );
    let out = render(text_outcome("text/html", &html), Format::Markdown, 0)
        .await
        .unwrap();
    assert!(
        out.contains("nested too deeply"),
        "{}",
        &out[..200.min(out.len())]
    );
    assert!(out.contains("deep text"));

    // 200k levels: must come back fast (the scan is linear), not burn a minute.
    let started = std::time::Instant::now();
    let html = "<div>".repeat(200_000);
    let out = render(text_outcome("text/html", &html), Format::Text, 0)
        .await
        .unwrap();
    assert!(out.contains("nested too deeply"));
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}
