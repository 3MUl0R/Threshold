# Milestone 10 — Image Generation (NanoBanana)

**Crate:** `imagegen`
**Complexity:** Small
**Dependencies:** Milestone 1 (core, secrets), Milestone 5 (tool framework)

## What This Milestone Delivers

Image generation via Google's NanoBanana model (Gemini image generation API).
The AI can generate icons, backgrounds, illustrations, and other visual assets
as part of building real projects. Results are delivered as file attachments
in Discord.

### Use Cases

- "Generate an icon for the settings page — a gear with a blue gradient"
- "Create a hero image for the landing page — modern, minimalist, tech theme"
- "Make a placeholder avatar — friendly robot face"
- Building out web projects with actual visual assets instead of lorem ipsum

---

## Phase 10.1 — Google Gemini Image Client

### `crates/imagegen/src/client.rs`

```rust
pub struct ImageGenClient {
    http: reqwest::Client,
    secret_store: Arc<SecretStore>,
    model: String,
}

impl ImageGenClient {
    pub fn new(secret_store: Arc<SecretStore>) -> Self {
        Self {
            http: reqwest::Client::new(),
            secret_store,
            model: "gemini-2.0-flash-exp".to_string(), // NanoBanana model
        }
    }

    pub async fn generate(
        &self,
        prompt: &str,
        options: &ImageGenOptions,
    ) -> Result<GeneratedImage> {
        let api_key = self.secret_store
            .resolve("google-api-key", "GOOGLE_API_KEY")
            .ok_or(ThresholdError::SecretNotFound {
                key: "google-api-key".into()
            })?;

        // Build request to Google Gemini API
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, api_key
        );

        let body = self.build_request_body(prompt, options);

        let response = self.http
            .post(&url)
            .json(&body)
            .timeout(Duration::from_secs(60))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(ThresholdError::ToolError {
                tool: "image_gen".into(),
                message: format!("Google API error: {}", error_text),
            });
        }

        self.parse_response(response, prompt).await
    }
}

pub struct ImageGenOptions {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub style: Option<String>,
    pub negative_prompt: Option<String>,
}

pub struct GeneratedImage {
    pub data: Vec<u8>,
    pub mime_type: String,
    pub prompt: String,
}
```

### Request/Response Format

The exact API format depends on the Google Gemini image generation endpoint.
Reference the OpenClaw integration docs (`docs/` in the OpenClaw repo) for
the specific request/response schema. The general pattern:

```json
{
  "contents": [{
    "parts": [{
      "text": "Generate an image: a friendly robot face icon, flat design, blue palette"
    }]
  }],
  "generationConfig": {
    "responseModalities": ["TEXT", "IMAGE"]
  }
}
```

Response contains base64-encoded image data in the `inlineData` field.

---

## Phase 10.2 — Image Generation Tool

### `crates/imagegen/src/tool.rs`

```rust
pub struct ImageGenTool {
    client: ImageGenClient,
}

#[async_trait]
impl Tool for ImageGenTool {
    fn name(&self) -> &str { "image_gen" }

    fn description(&self) -> &str {
        "Generate images from text descriptions using Google's NanoBanana \
         (Gemini image generation). Useful for creating icons, illustrations, \
         backgrounds, and other visual assets."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Text description of the image to generate. \
                                    Be specific about style, colors, composition."
                },
                "width": {
                    "type": "integer",
                    "description": "Image width in pixels (optional)"
                },
                "height": {
                    "type": "integer",
                    "description": "Image height in pixels (optional)"
                },
                "style": {
                    "type": "string",
                    "description": "Style hint: 'photorealistic', 'illustration', \
                                    'flat-design', 'pixel-art', etc."
                },
                "negative_prompt": {
                    "type": "string",
                    "description": "What to avoid in the image (optional)"
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let prompt = params["prompt"].as_str()
            .ok_or_else(|| tool_error("Missing 'prompt' parameter"))?;

        let options = ImageGenOptions {
            width: params["width"].as_u64().map(|n| n as u32),
            height: params["height"].as_u64().map(|n| n as u32),
            style: params["style"].as_str().map(String::from),
            negative_prompt: params["negative_prompt"].as_str().map(String::from),
        };

        let image = self.client.generate(prompt, &options).await?;

        // Determine file extension from MIME type
        let ext = match image.mime_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            _ => "png",
        };

        let filename = format!("generated-{}.{}", Uuid::new_v4(), ext);

        Ok(ToolResult {
            content: format!("Generated image for: {}", prompt),
            artifacts: vec![Artifact {
                name: filename,
                data: image.data,
                mime_type: image.mime_type,
            }],
            success: true,
        })
    }
}
```

---

## Phase 10.3 — Discord Artifact Delivery

Extend the Discord outbound handler to deliver image artifacts as file
attachments.

### In `crates/discord/src/outbound.rs`

```rust
impl DiscordOutbound {
    /// Send a message with file attachments.
    pub async fn send_with_attachments(
        &self,
        channel_id: u64,
        content: &str,
        attachments: Vec<(String, Vec<u8>)>,  // (filename, data)
    ) -> Result<()> {
        let channel = serenity::ChannelId::new(channel_id);

        let files: Vec<serenity::CreateAttachment> = attachments.iter()
            .map(|(name, data)| serenity::CreateAttachment::bytes(data.clone(), name))
            .collect();

        channel.send_files(
            &self.http,
            files,
            serenity::CreateMessage::new().content(content),
        ).await?;

        Ok(())
    }
}
```

### Integration with Message Handler

When the conversation engine returns a response that includes artifacts
(from any tool, not just image_gen), the Discord handler checks for them:

```rust
// In the portal listener
ConversationEvent::AssistantMessage { content, artifacts, .. } => {
    if artifacts.is_empty() {
        // Text-only response
        for chunk in chunk_message(&content, 2000) {
            channel_id.say(&http, &chunk).await.ok();
        }
    } else {
        // Response with attachments
        let files: Vec<_> = artifacts.iter()
            .map(|a| (a.name.clone(), a.data.clone()))
            .collect();
        outbound.send_with_attachments(channel_id, &content, files).await.ok();
    }
}
```

---

## Configuration

```toml
[tools.image_gen]
enabled = true
# Google API key stored in keychain as "google-api-key"
```

The Google API key is shared with Gmail (if both are enabled).

---

## Crate Module Structure

```
crates/imagegen/src/
  lib.rs            — re-exports ImageGenTool
  client.rs         — ImageGenClient (Google Gemini API wrapper)
  tool.rs           — ImageGenTool implementing the Tool trait
```

---

## Verification Checklist

- [ ] Unit test: request body construction with all options
- [ ] Unit test: response parsing (base64 image extraction)
- [ ] Unit test: file extension selection from MIME type
- [ ] Integration test (with Google API key): generate an image
- [ ] Integration test: generated image returned as artifact in ToolResult
- [ ] Integration test: artifact delivery to Discord as file attachment
- [ ] Integration test: tool is blocked when config has `enabled = false`
- [ ] Integration test: tool fails gracefully when API key is missing
- [ ] Integration test: audit trail records image generation requests
