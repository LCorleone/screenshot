//! OCR client abstraction.
//!
//! Phase 0 only defines the trait. The real HTTP-backed implementation
//! (calling a vision-capable LLM or a dedicated OCR endpoint) arrives in a
//! later phase.

/// Recognize text in images. Implementations may return plain text, markdown,
/// or HTML (including tables) depending on the underlying service.
#[allow(dead_code)]
pub trait OcrClient {
    /// Recognize text in the given PNG/JPEG bytes.
    /// Returns text (possibly markdown/HTML including tables).
    fn recognize(&self, image_bytes: &[u8]) -> anyhow::Result<String>;
}
