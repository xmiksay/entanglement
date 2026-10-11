use super::*;

fn tree(html: &str) -> Rc<Node> {
    htmd::HtmlToMarkdown::new().html_to_tree(html).unwrap()
}

/// Run `f` on a converter-sized thread, as production does.
fn on_converter_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(CONVERT_STACK)
        .spawn(f)
        .unwrap()
        .join()
        .expect("must not overflow the converter stack")
}

#[test]
fn depth_is_measured_on_the_real_tree() {
    // html > body > div > div > span (head is a sibling of body).
    assert_eq!(max_depth(&tree("<div><div><span>x</span></div></div>")), 5);
    // A `>` inside a quoted attribute fools a lexical scan, not the parser:
    // every `<div a="></div>">` really opens an unclosed div.
    let payload = "<div a=\"></div>\">".repeat(1000);
    assert!(max_depth(&tree(&payload)) >= 1000);
}

#[test]
fn tree_text_drops_skipped_elements_and_breaks_blocks() {
    let html = "<html><head><title>t</title><style>p{}</style></head><body><h1>Title</h1>\
                <script>alert(1)</script><p>a &amp; b&nbsp;&lt;c&gt;</p>\
                <ul><li>one</li><li>two</li></ul></body></html>";
    assert_eq!(tree_text(&tree(html)), "Title\na & b <c>\none\ntwo\n");
}

#[test]
fn token_scan_sees_through_quoted_gt_and_stray_end_tags() {
    assert_eq!(
        token_scan("<div><div><br><span>x</span></div></div>").depth,
        3
    );
    // The bypass that fooled the hand-rolled scan: the spec tokenizer knows
    // the `>` and `</div>` are attribute text.
    assert_eq!(token_scan(&"<div a=\"></div>\">".repeat(1000)).depth, 1000);
    // `</span>` matches nothing open, so it can't pop a `<div>`.
    assert_eq!(token_scan(&"<div></span>".repeat(500)).depth, 500);
    // Markup-looking text inside script/style/comments isn't nesting.
    let html = "<script>'<div><div>'</script><style>a{}</style><!-- <div><div> -->";
    assert_eq!(token_scan(html).depth, 1);
}

#[test]
fn token_scan_text_skips_scripts_and_decodes_entities() {
    let html = "<title>t</title><h1>Title</h1><script>alert(1)</script>\
                <p>a &amp; b&nbsp;&lt;c&gt;</p><ul><li>one</li><li>two</li></ul>";
    assert_eq!(token_scan(html).text, "Title\na & b <c>\none\ntwo\n");
}

#[test]
fn the_converter_stack_covers_four_times_the_depth_gate() {
    let depth = MAX_DEPTH * 4;
    let html = format!("{}x{}", "<div>".repeat(depth), "</div>".repeat(depth));
    let out = on_converter_stack(move || htmd::convert(&html).unwrap());
    assert_eq!(out.trim(), "x");
}

/// The reviewer's crash payload at the size that aborted the process
/// (1.87 MB, under the 2 MiB download cap): the real conversion path must
/// return a plain-text fallback — and fast, since it must never reach the
/// quadratic tree builder. An overflow here would abort the test binary — a
/// loud failure, never a silent pass.
#[test]
fn the_quoted_gt_payload_that_crashed_the_process_falls_back_fast() {
    let html = "<div a=\"></div>\">".repeat(110_000) + "tail";
    let started = std::time::Instant::now();
    let out = on_converter_stack(move || convert(&html));
    assert_eq!(out.fallback, Some("HTML nested too deeply to convert"));
    assert_eq!(out.text, "tail\n");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn the_real_tree_is_the_safety_net_when_the_token_model_undercounts() {
    // Deep on the tree but shallow per token stack: not constructible from
    // the open-element model alone, so exercise the tree gate directly.
    let depth = MAX_DEPTH + 10;
    let html = format!("{}deep{}", "<div>".repeat(depth), "</div>".repeat(depth));
    let t = tree(&html);
    assert!(max_depth(&t) > MAX_DEPTH);
    assert_eq!(tree_text(&t), "deep\n");
}

#[tokio::test]
async fn ordinary_html_converts_without_a_fallback() {
    let out = to_markdown("<h1>T</h1><p>body</p>".into()).await;
    assert!(out.fallback.is_none());
    assert!(out.text.contains("# T"), "{}", out.text);
}

#[tokio::test]
async fn over_the_gate_falls_back_to_plain_text() {
    let depth = MAX_DEPTH + 1;
    let html = format!("{}deep{}", "<div>".repeat(depth), "</div>".repeat(depth));
    let out = to_markdown(html).await;
    assert_eq!(out.fallback, Some("HTML nested too deeply to convert"));
    assert_eq!(out.text, "deep\n");
}

#[tokio::test]
async fn conversions_are_capped_and_a_held_slot_falls_back() {
    // Take every slot, as abandoned conversions still running would.
    let held = CONVERSIONS
        .clone()
        .acquire_many_owned(MAX_CONVERSIONS as u32)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(50);
    let result = run_converter("<p>x</p>".into(), deadline).await;
    assert_eq!(result.err(), Some("HTML converter busy"));
    drop(held);
}
