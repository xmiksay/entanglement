//! Clipboard image paste into the prompt (#736). `Ctrl+V` (or a bracketed
//! paste that arrives empty — what terminals send when the clipboard holds
//! only an image) reads the clipboard image, vets it, parks it on the input
//! buffer and inserts an `[Image #n]` chip at the cursor. On submit, every
//! parked image whose chip is still in the text rides the prompt as an image
//! content part; deleting the chip drops the image.
//!
//! Only a model whose catalog entry sets `supports_vision` gets one: a
//! text-only model would reject the request outright, so the paste is
//! refused up front with a notice instead — and re-checked at submit, since
//! `/model` can switch between paste and Enter.

use base64::Engine;
use entanglement_core::ContentPart;

use super::App;
use crate::tui::clipboard;

/// Encoded (base64) size cap — Anthropic's per-image limit, the strictest of
/// the wires that take inline images.
pub const MAX_IMAGE_ENCODED_BYTES: usize = 5 * 1024 * 1024;

/// One vetted clipboard image waiting on the input buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct PastedImage {
    pub media_type: &'static str,
    /// Base64 of the image bytes.
    pub data: String,
}

/// The chip text standing in for image `n` (1-based) in the input buffer.
pub fn chip(n: usize) -> String {
    format!("[Image #{n}]")
}

/// Identify an image by its magic bytes — the bytes decide, not whatever
/// type the clipboard tool claimed. Only the formats every vision wire
/// accepts inline.
pub fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Vet raw clipboard bytes: a supported format, and an encoded size within
/// [`MAX_IMAGE_ENCODED_BYTES`]. The size check runs on the computed encoded
/// length before encoding, so an oversized image is never base64'd.
pub fn prepare(bytes: &[u8]) -> Result<PastedImage, String> {
    let media_type = sniff_media_type(bytes)
        .ok_or("clipboard image is not PNG, JPEG, GIF or WebP — not attached")?;
    let encoded_len = bytes.len().div_ceil(3) * 4;
    if encoded_len > MAX_IMAGE_ENCODED_BYTES {
        return Err(format!(
            "clipboard image is too large ({} KiB encoded, limit {} KiB) — not attached",
            encoded_len / 1024,
            MAX_IMAGE_ENCODED_BYTES / 1024
        ));
    }
    Ok(PastedImage {
        media_type,
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

/// The prompt's content parts: the text (chips included, so the model can
/// tell which image the user referred to where), then each image whose chip
/// survived editing.
pub fn prompt_content(text: &str, images: Vec<PastedImage>) -> Vec<ContentPart> {
    let mut parts = vec![ContentPart::text(text)];
    parts.extend(
        images
            .into_iter()
            .enumerate()
            .filter(|(i, _)| text.contains(&chip(i + 1)))
            .map(|(_, img)| ContentPart::image(img.media_type, img.data)),
    );
    parts
}

impl App {
    /// Whether the active model's catalog entry declares image input. An
    /// uncatalogued model (an env override the user typed) is assumed
    /// text-only — fail closed rather than 400 the turn.
    pub fn model_supports_vision(&self) -> bool {
        self.settings
            .catalog()
            .model(&self.active_provider, &self.model_info.id)
            .is_some_and(|m| m.supports_vision)
    }

    fn vision_refusal(&self) -> String {
        format!(
            "model `{}` has no image input (catalog `supports_vision`) — image not attached",
            self.model_info.id
        )
    }

    /// The `Ctrl+V` / empty-paste path: read, vet and park a clipboard image.
    pub async fn paste_clipboard_image(&mut self) {
        if !self.model_supports_vision() {
            self.set_toast(self.vision_refusal());
            return;
        }
        let result = match clipboard::read_image().await {
            Ok(bytes) => prepare(&bytes),
            Err(e) => Err(format!("image paste: {e}")),
        };
        self.attach_image(result);
    }

    /// Park a vetted image and insert its chip, or surface why not.
    pub fn attach_image(&mut self, image: Result<PastedImage, String>) {
        match image {
            Ok(image) => {
                let n = self.input.push_image(image);
                self.input.insert_str(&chip(n));
                self.mark_dirty();
            }
            Err(message) => self.set_toast(message),
        }
    }

    /// Build the submitted prompt's content from `text` and the images taken
    /// off the input buffer, dropping the images (with a notice) if the
    /// model lost vision since they were pasted.
    pub fn prompt_content(&mut self, text: &str, images: Vec<PastedImage>) -> Vec<ContentPart> {
        if !images.is_empty() && !self.model_supports_vision() {
            self.set_toast(self.vision_refusal());
            return vec![ContentPart::text(text)];
        }
        prompt_content(text, images)
    }
}

#[cfg(test)]
mod tests;
