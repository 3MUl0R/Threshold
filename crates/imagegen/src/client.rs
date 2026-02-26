//! Google Gemini image generation API client.
//!
//! Wraps the Gemini `generateContent` endpoint to produce images from text
//! prompts. Uses the `gemini-2.5-flash-image` model (NanoBanana).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::Deserialize;
use threshold_core::SecretStore;

// ── Error type ──

#[derive(Debug, thiserror::Error)]
pub enum ImageGenError {
    #[error("API key not found: configure 'google-api-key' in secret store or set GOOGLE_API_KEY")]
    SecretNotFound,

    #[error("API request failed: {0}")]
    RequestFailed(String),

    #[error("Failed to parse response: {0}")]
    ParseError(String),

    #[error("Image generation not enabled")]
    NotEnabled,

    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

// ── Public types ──

/// Options controlling image generation.
#[derive(Debug, Clone, Default)]
pub struct ImageGenOptions {
    /// Style hint prepended to the prompt (e.g., "flat-design", "watercolor").
    pub style: Option<String>,
    /// Negative prompt — things to avoid in the generated image.
    pub negative_prompt: Option<String>,
}

/// A successfully generated image.
#[derive(Debug, Clone)]
pub struct GeneratedImage {
    /// Raw image bytes (PNG, JPEG, or WebP).
    pub data: Vec<u8>,
    /// MIME type reported by the API (e.g., "image/png").
    pub mime_type: String,
    /// The prompt that was sent to the API (after style/negative processing).
    pub prompt: String,
}

// ── Client ──

/// Client for the Google Gemini image generation API.
pub struct ImageGenClient {
    http: reqwest::Client,
    secret_store: Arc<SecretStore>,
    model: String,
}

impl ImageGenClient {
    /// Create a new client with default model (`gemini-2.5-flash-image`).
    pub fn new(secret_store: Arc<SecretStore>) -> Self {
        Self {
            http: reqwest::Client::new(),
            secret_store,
            model: "gemini-2.5-flash-image".to_string(),
        }
    }

    /// Generate an image from a text prompt.
    pub async fn generate(
        &self,
        prompt: &str,
        options: &ImageGenOptions,
    ) -> Result<GeneratedImage, ImageGenError> {
        let api_key = self
            .secret_store
            .resolve("google-api-key", "GOOGLE_API_KEY")
            .map_err(|e| ImageGenError::RequestFailed(format!("Secret store error: {}", e)))?
            .ok_or(ImageGenError::SecretNotFound)?;

        let effective_prompt = build_effective_prompt(prompt, options);
        let body = build_request_body(&effective_prompt);

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            self.model
        );

        let response = self
            .http
            .post(&url)
            .header("x-goog-api-key", &api_key)
            .json(&body)
            .timeout(Duration::from_secs(60))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(ImageGenError::RequestFailed(format!(
                "{}: {}",
                status, error_text
            )));
        }

        parse_response(response, &effective_prompt).await
    }
}

// ── Request building ──

/// Build the effective prompt by combining style, base prompt, and negative prompt.
fn build_effective_prompt(prompt: &str, options: &ImageGenOptions) -> String {
    let mut parts = Vec::new();

    if let Some(style) = &options.style {
        parts.push(format!("Generate an image in {} style:", style));
    }

    parts.push(prompt.to_string());

    if let Some(neg) = &options.negative_prompt {
        parts.push(format!("Avoid: {}", neg));
    }

    parts.join(" ")
}

/// Build the JSON request body for the Gemini generateContent API.
pub fn build_request_body(prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "contents": [{
            "parts": [{
                "text": prompt
            }]
        }],
        "generationConfig": {
            "responseModalities": ["TEXT", "IMAGE"]
        }
    })
}

// ── Response parsing ──

/// Gemini API response structure (partial — only fields we need).
#[derive(Debug, Deserialize)]
struct GeminiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiContent>,
}

#[derive(Debug, Deserialize)]
struct GeminiContent {
    parts: Option<Vec<GeminiPart>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiPart {
    inline_data: Option<GeminiInlineData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiInlineData {
    mime_type: String,
    data: String,
}

/// Maximum response body size (50 MB). Prevents OOM from malicious/unexpected payloads.
const MAX_RESPONSE_BYTES: usize = 50 * 1024 * 1024;

/// Parse the Gemini API response and extract the generated image.
async fn parse_response(
    response: reqwest::Response,
    prompt: &str,
) -> Result<GeneratedImage, ImageGenError> {
    // Read response with size limit to prevent OOM
    let bytes = response
        .bytes()
        .await
        .map_err(|e| ImageGenError::ParseError(format!("Failed to read response body: {}", e)))?;

    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(ImageGenError::ParseError(format!(
            "Response too large: {} bytes (max {})",
            bytes.len(),
            MAX_RESPONSE_BYTES
        )));
    }

    let body: GeminiResponse = serde_json::from_slice(&bytes)
        .map_err(|e| ImageGenError::ParseError(format!("Failed to deserialize response: {}", e)))?;

    extract_image_from_response(&body, prompt)
}

/// Extract the first image from a parsed Gemini response.
fn extract_image_from_response(
    body: &GeminiResponse,
    prompt: &str,
) -> Result<GeneratedImage, ImageGenError> {
    let candidates = body
        .candidates
        .as_ref()
        .ok_or_else(|| ImageGenError::ParseError("No candidates in response".to_string()))?;

    for candidate in candidates {
        let Some(content) = &candidate.content else {
            continue;
        };
        let Some(parts) = &content.parts else {
            continue;
        };
        for part in parts {
            if let Some(inline_data) = &part.inline_data
                && inline_data.mime_type.starts_with("image/")
            {
                let data = BASE64_STANDARD.decode(&inline_data.data).map_err(|e| {
                    ImageGenError::ParseError(format!("Failed to decode base64 image: {}", e))
                })?;

                return Ok(GeneratedImage {
                    data,
                    mime_type: inline_data.mime_type.clone(),
                    prompt: prompt.to_string(),
                });
            }
        }
    }

    Err(ImageGenError::ParseError(
        "No image data found in response".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_body_includes_prompt_and_modalities() {
        let body = build_request_body("a blue gear icon");
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 1);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["text"], "a blue gear icon");

        let modalities = body["generationConfig"]["responseModalities"]
            .as_array()
            .unwrap();
        assert!(modalities.contains(&serde_json::json!("TEXT")));
        assert!(modalities.contains(&serde_json::json!("IMAGE")));
    }

    #[test]
    fn build_effective_prompt_plain() {
        let opts = ImageGenOptions::default();
        let result = build_effective_prompt("a cat", &opts);
        assert_eq!(result, "a cat");
    }

    #[test]
    fn build_effective_prompt_with_style() {
        let opts = ImageGenOptions {
            style: Some("watercolor".to_string()),
            negative_prompt: None,
        };
        let result = build_effective_prompt("a cat", &opts);
        assert_eq!(result, "Generate an image in watercolor style: a cat");
    }

    #[test]
    fn build_effective_prompt_with_negative() {
        let opts = ImageGenOptions {
            style: None,
            negative_prompt: Some("blurry, low quality".to_string()),
        };
        let result = build_effective_prompt("a cat", &opts);
        assert_eq!(result, "a cat Avoid: blurry, low quality");
    }

    #[test]
    fn build_effective_prompt_with_all_options() {
        let opts = ImageGenOptions {
            style: Some("flat-design".to_string()),
            negative_prompt: Some("realistic, photo".to_string()),
        };
        let result = build_effective_prompt("a gear icon", &opts);
        assert_eq!(
            result,
            "Generate an image in flat-design style: a gear icon Avoid: realistic, photo"
        );
    }

    #[test]
    fn parse_valid_gemini_response_with_image() {
        // Encode a small "image" as base64
        let fake_png = b"fake-png-data";
        let b64 = BASE64_STANDARD.encode(fake_png);

        let response = GeminiResponse {
            candidates: Some(vec![GeminiCandidate {
                content: Some(GeminiContent {
                    parts: Some(vec![GeminiPart {
                        inline_data: Some(GeminiInlineData {
                            mime_type: "image/png".to_string(),
                            data: b64,
                        }),
                    }]),
                }),
            }]),
        };

        let result = extract_image_from_response(&response, "test prompt").unwrap();
        assert_eq!(result.data, b"fake-png-data");
        assert_eq!(result.mime_type, "image/png");
        assert_eq!(result.prompt, "test prompt");
    }

    #[test]
    fn parse_response_no_candidates() {
        let response = GeminiResponse { candidates: None };
        let err = extract_image_from_response(&response, "test").unwrap_err();
        assert!(matches!(err, ImageGenError::ParseError(_)));
        assert!(err.to_string().contains("No candidates"));
    }

    #[test]
    fn parse_response_no_image_parts() {
        let response = GeminiResponse {
            candidates: Some(vec![GeminiCandidate {
                content: Some(GeminiContent {
                    parts: Some(vec![GeminiPart { inline_data: None }]),
                }),
            }]),
        };
        let err = extract_image_from_response(&response, "test").unwrap_err();
        assert!(matches!(err, ImageGenError::ParseError(_)));
        assert!(err.to_string().contains("No image data"));
    }

    #[test]
    fn parse_response_text_only_parts() {
        // Part with non-image inline_data
        let response = GeminiResponse {
            candidates: Some(vec![GeminiCandidate {
                content: Some(GeminiContent {
                    parts: Some(vec![GeminiPart {
                        inline_data: Some(GeminiInlineData {
                            mime_type: "text/plain".to_string(),
                            data: "aGVsbG8=".to_string(), // "hello" in base64
                        }),
                    }]),
                }),
            }]),
        };
        let err = extract_image_from_response(&response, "test").unwrap_err();
        assert!(matches!(err, ImageGenError::ParseError(_)));
        assert!(err.to_string().contains("No image data"));
    }

    #[test]
    fn parse_response_empty_candidate_content() {
        let response = GeminiResponse {
            candidates: Some(vec![GeminiCandidate { content: None }]),
        };
        let err = extract_image_from_response(&response, "test").unwrap_err();
        assert!(err.to_string().contains("No image data"));
    }

    #[test]
    fn parse_response_invalid_base64() {
        let response = GeminiResponse {
            candidates: Some(vec![GeminiCandidate {
                content: Some(GeminiContent {
                    parts: Some(vec![GeminiPart {
                        inline_data: Some(GeminiInlineData {
                            mime_type: "image/png".to_string(),
                            data: "not-valid-base64!!!".to_string(),
                        }),
                    }]),
                }),
            }]),
        };
        let err = extract_image_from_response(&response, "test").unwrap_err();
        assert!(err.to_string().contains("base64"));
    }

    #[test]
    fn image_gen_options_default() {
        let opts = ImageGenOptions::default();
        assert!(opts.style.is_none());
        assert!(opts.negative_prompt.is_none());
    }

    #[test]
    fn error_display_strings() {
        assert!(
            ImageGenError::SecretNotFound
                .to_string()
                .contains("google-api-key")
        );
        assert!(
            ImageGenError::RequestFailed("500".into())
                .to_string()
                .contains("500")
        );
        assert!(
            ImageGenError::ParseError("bad json".into())
                .to_string()
                .contains("bad json")
        );
        assert!(
            ImageGenError::NotEnabled
                .to_string()
                .contains("not enabled")
        );
    }

    #[test]
    fn parse_response_picks_first_image_from_mixed_parts() {
        let png_data = BASE64_STANDARD.encode(b"first-image");
        let jpeg_data = BASE64_STANDARD.encode(b"second-image");

        let response = GeminiResponse {
            candidates: Some(vec![GeminiCandidate {
                content: Some(GeminiContent {
                    parts: Some(vec![
                        GeminiPart { inline_data: None }, // text part
                        GeminiPart {
                            inline_data: Some(GeminiInlineData {
                                mime_type: "image/png".to_string(),
                                data: png_data,
                            }),
                        },
                        GeminiPart {
                            inline_data: Some(GeminiInlineData {
                                mime_type: "image/jpeg".to_string(),
                                data: jpeg_data,
                            }),
                        },
                    ]),
                }),
            }]),
        };

        let result = extract_image_from_response(&response, "test").unwrap();
        assert_eq!(result.data, b"first-image");
        assert_eq!(result.mime_type, "image/png");
    }
}
