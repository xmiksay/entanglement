use super::*;

#[test]
fn depth_counts_real_nesting_and_ignores_void_and_raw_text() {
    assert_eq!(nesting_depth("<p>hi</p>"), 1);
    assert_eq!(nesting_depth("<div><div><span>x</span></div></div>"), 3);
    assert_eq!(nesting_depth("<div><br><img src=x><hr></div>"), 1);
    // Tags inside script/style/comments don't count.
    assert_eq!(
        nesting_depth("<script>'<div><div><div>'</script><!-- <div><div> -->"),
        1
    );
    // Uppercase and attributes.
    assert_eq!(nesting_depth("<DIV class=\"a\"><Div>x</DIV></div>"), 2);
}

#[test]
fn stray_end_tags_cannot_hide_unclosed_nesting() {
    // `</span>` matches no open element, so it must not pop a `<div>`.
    let html = "<div></span>".repeat(1000);
    assert_eq!(nesting_depth(&html), 1000);
    // A matching end tag pops back past implicitly-open children.
    assert_eq!(nesting_depth("<div><p><p><p></div><div></div>"), 4);
}

#[test]
fn plain_text_strips_markup_scripts_and_entities() {
    let html = "<html><head><style>p{}</style></head><body><h1>Title</h1>\
                <script>alert(1)</script><p>a &amp; b&nbsp;&lt;c&gt;</p>\
                <ul><li>one</li><li>two</li></ul></body></html>";
    let text = plain_text(html);
    assert_eq!(text, "Title\na & b <c>\none\ntwo\n");
}

#[test]
fn plain_text_is_safe_on_pathological_input() {
    let html = "<div>".repeat(200_000) + "end";
    assert_eq!(plain_text(&html), "end\n");
    assert_eq!(plain_text("a < b"), "a < b\n");
}

#[test]
fn the_conversion_stack_covers_four_times_the_depth_gate() {
    // The gate is an estimate; the converter thread's stack must survive a
    // DOM well past it.
    let depth = MAX_DEPTH * 4;
    let html = format!("{}x{}", "<div>".repeat(depth), "</div>".repeat(depth));
    let out = std::thread::Builder::new()
        .stack_size(CONVERT_STACK)
        .spawn(move || convert(&html))
        .unwrap()
        .join()
        .expect("conversion at 4x the gate must not overflow");
    assert_eq!(out.as_deref().map(str::trim), Some("x"));
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
