use entanglement_core::{ContentPart, OutEvent, SessionId};

use super::*;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

fn app_on(provider: &str, model: &str) -> App {
    let sid = SessionId::new("test");
    let mut app = App::new_for_test(sid.clone());
    app.handle_out_event(OutEvent::ModelChanged {
        session: sid,
        provider: provider.to_string(),
        model: model.to_string(),
        context_window: None,
    });
    app
}

fn image_count(parts: &[ContentPart]) -> usize {
    parts
        .iter()
        .filter(|p| matches!(p, ContentPart::Image { .. }))
        .count()
}

#[test]
fn sniffs_the_four_supported_formats_by_magic_bytes() {
    assert_eq!(sniff_media_type(PNG), Some("image/png"));
    assert_eq!(
        sniff_media_type(&[0xff, 0xd8, 0xff, 0xe0]),
        Some("image/jpeg")
    );
    assert_eq!(sniff_media_type(b"GIF89a...."), Some("image/gif"));
    assert_eq!(
        sniff_media_type(b"RIFF\0\0\0\0WEBPVP8 "),
        Some("image/webp")
    );
    assert_eq!(sniff_media_type(b"BM\0\0"), None, "bmp is not accepted");
    assert_eq!(sniff_media_type(b"<svg"), None);
    assert_eq!(sniff_media_type(b"RIFF\0\0\0\0WAVE"), None);
    assert_eq!(sniff_media_type(b""), None);
}

#[test]
fn prepare_rejects_bad_formats_and_oversize_images() {
    let ok = prepare(PNG).unwrap();
    assert_eq!(ok.media_type, "image/png");
    assert!(!ok.data.is_empty());

    let err = prepare(b"%PDF-1.7").unwrap_err();
    assert!(err.contains("not PNG"), "{err}");

    // Raw size whose base64 lands one block past the cap.
    let mut big = PNG.to_vec();
    big.resize(MAX_IMAGE_ENCODED_BYTES / 4 * 3 + 1, 0);
    let err = prepare(&big).unwrap_err();
    assert!(err.contains("too large"), "{err}");
    // Exactly at the cap is fine.
    big.truncate(MAX_IMAGE_ENCODED_BYTES / 4 * 3);
    assert!(prepare(&big).is_ok());
}

#[test]
fn vision_follows_the_catalog_flag_and_fails_closed_for_unknown_models() {
    assert!(app_on("anthropic", "claude-sonnet-5").model_supports_vision());
    assert!(app_on("openai", "gpt-4o").model_supports_vision());
    assert!(!app_on("zai", "glm-5.3").model_supports_vision());
    assert!(!app_on("anthropic", "some-custom-model").model_supports_vision());
}

#[test]
fn attaching_inserts_a_chip_and_submit_carries_the_image() {
    let mut app = app_on("anthropic", "claude-sonnet-5");
    app.input().insert_str("look: ");
    app.attach_image(prepare(PNG));
    let images = app.input().take_images();
    let text = app.take_input_text();
    assert_eq!(text, "look: [Image #1]");
    let parts = app.prompt_content(&text, images);
    assert_eq!(parts[0], ContentPart::text("look: [Image #1]"));
    assert_eq!(image_count(&parts), 1);
}

#[test]
fn deleting_the_chip_drops_its_image() {
    let images = vec![prepare(PNG).unwrap(), prepare(PNG).unwrap()];
    let parts = prompt_content("only [Image #2] stays", images);
    assert_eq!(image_count(&parts), 1);
}

#[test]
fn a_bad_paste_attaches_nothing_and_leaves_the_input_alone() {
    let mut app = app_on("anthropic", "claude-sonnet-5");
    app.attach_image(prepare(b"not an image"));
    assert!(app.input().take_images().is_empty());
    assert_eq!(app.input_text(), "");
    assert!(app.toast().is_some_and(|t| t.contains("not attached")));
}

#[tokio::test]
async fn a_non_vision_model_refuses_the_paste_with_a_notice() {
    let mut app = app_on("zai", "glm-5.3");
    app.paste_clipboard_image().await;
    assert!(app.input().take_images().is_empty());
    assert_eq!(app.input_text(), "");
    let toast = app.toast().unwrap_or_default().to_string();
    assert!(toast.contains("no image input"), "{toast}");
}

#[test]
fn switching_to_a_text_model_before_submit_drops_parked_images() {
    let mut app = app_on("zai", "glm-5.3");
    let parts = app.prompt_content("see [Image #1]", vec![prepare(PNG).unwrap()]);
    assert_eq!(image_count(&parts), 0);
    assert_eq!(parts, vec![ContentPart::text("see [Image #1]")]);
    assert!(app.toast().is_some());
}
