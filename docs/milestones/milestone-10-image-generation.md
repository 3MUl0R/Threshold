# Milestone 10 — Image Generation (NanoBanana)

**Crate:** `imagegen`
**Complexity:** Small
**Dependencies:** Milestone 1 (core, secrets), Milestone 5 (tool framework — CLI binary skeleton)

## Architecture Note: CLI Subcommands

> Per the CLI-based tool architecture (see Milestone 5), image generation is
> exposed as `threshold imagegen generate` CLI subcommand rather than a Tool
> trait implementation. Claude invokes it via native shell execution.
>
> ```
> Claude needs to generate an image:
>     → exec("threshold imagegen generate --prompt 'a gear icon, blue gradient' --style flat-design")
>     → image saved to file, stdout returns JSON with file path and metadata
>     → Claude references the file in its response
> ```
>
> The Gemini API client and image generation logic remain unchanged from the
> designs below. Only the interface changes from Tool trait to clap subcommand.
>
> **Artifact delivery:** The CLI writes the image to a temp file and returns
> the path. The conversation engine or Discord handler picks up the file path
> from Claude's response and delivers it as an attachment.

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

## Phase 10.2 — Image Generation CLI Subcommand

### `crates/imagegen/src/cli.rs`

```rust
use clap::Parser;

#[derive(Parser)]
pub struct ImagegenArgs {
    #[command(subcommand)]
    pub command: ImagegenCommands,
}

#[derive(clap::Subcommand)]
pub enum ImagegenCommands {
    /// Generate an image from a text description
    Generate {
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        #[arg(long)]
        style: Option<String>,
        #[arg(long)]
        negative_prompt: Option<String>,
        /// Output directory (default: system temp dir)
        #[arg(long)]
        output_dir: Option<String>,
    },
}

pub async fn handle_imagegen_command(args: ImagegenArgs) -> Result<()> {
    let secret_store = Arc::new(SecretStore::new());
    let client = ImageGenClient::new(secret_store);
    let audit = AuditTrail::new(/* ... */);

    match args.command {
        ImagegenCommands::Generate { prompt, width, height, style, negative_prompt, output_dir } => {
            let options = ImageGenOptions { width, height, style, negative_prompt };
            let image = client.generate(&prompt, &options).await?;

            // Determine file extension from MIME type
            let ext = match image.mime_type.as_str() {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/webp" => "webp",
                _ => "png",
            };

            // Write to output directory or trusted artifacts dir
            let dir = output_dir
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("threshold-generated"));
            std::fs::create_dir_all(&dir)?;
            let filename = format!("generated-{}.{}", Uuid::new_v4(), ext);
            let path = dir.join(&filename);
            std::fs::write(&path, &image.data)?;

            // Output JSON with file path for Claude to reference
            let output = json!({
                "status": "ok",
                "file_path": path.to_string_lossy(),
                "filename": filename,
                "mime_type": image.mime_type,
                "size_bytes": image.data.len(),
                "prompt": prompt,
            });
            audit.append_event("imagegen", &json!({"action": "generate", "prompt": prompt})).ok();
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
    }

    Ok(())
}
```

### Artifact Delivery Model

The CLI writes the generated image to a file and returns the path in JSON.
Claude includes the file path in its response to the user. The Discord
handler (or other portal) detects file paths in the response and delivers
them as attachments.

**Why file paths instead of inline artifacts:**
- No need for base64 encoding/decoding in the CLI↔Claude pipeline
- Files can be large; stdout is not ideal for binary data
- Claude can reference the same file multiple times
- Works naturally with Claude's existing file awareness

### Discord Attachment Delivery

The Discord handler extracts file paths from Claude's responses and
attaches them. The contract: generated files follow the naming pattern
`generated-<uuid>.<ext>` in the system temp directory.

```rust
use std::path::{Path, PathBuf};
use regex::Regex;

/// Trusted directory for generated artifacts. All generated files MUST
/// be written here; the extractor only attaches files from this root.
///
/// Returns the canonicalized path (resolves symlinks like /tmp → /private/tmp
/// on macOS) so that `starts_with` comparisons work correctly.
fn generated_artifacts_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("threshold-generated");
    std::fs::create_dir_all(&dir).ok();
    // Canonicalize to handle OS-specific symlinks (e.g., /tmp → /private/tmp)
    dir.canonicalize().unwrap_or(dir)
}

/// Extract generated file paths from Claude's response text.
///
/// Security: only returns paths that are:
/// 1. Under the trusted artifacts directory (no path traversal)
/// 2. Match the `generated-<uuid>.<ext>` naming convention
/// 3. Actually exist on disk
fn extract_file_attachments(response: &str) -> Vec<PathBuf> {
    let trusted_root = generated_artifacts_dir();
    let re = Regex::new(r"(/[^\s\"']+/generated-[0-9a-f-]+\.\w+)").unwrap();
    re.captures_iter(response)
        .filter_map(|cap| {
            let path = PathBuf::from(&cap[1]);
            let canonical = path.canonicalize().ok()?;
            if canonical.starts_with(&trusted_root) {
                Some(canonical)
            } else {
                None
            }
        })
        .collect()
}
```

This is a post-processing step in the Discord portal, not part of the
imagegen crate itself. The trusted-root constraint ensures only files
generated by Threshold are attached — arbitrary file paths mentioned in
conversation are ignored.

**Note on `--output-dir`:** When the user specifies `--output-dir`, files
are written outside the trusted root. These files will NOT be auto-attached
as Discord attachments — the user is responsible for retrieving them. Only
files in the default `generated_artifacts_dir()` are candidates for
auto-attachment. This is intentional: `--output-dir` is for saving files
to a specific location (e.g., a project's `assets/` directory), not for
Discord delivery.

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
  lib.rs            — re-exports public API
  cli.rs            — clap subcommand definition and handler
  client.rs         — ImageGenClient (Google Gemini API wrapper)
```

---

## Verification Checklist

- [ ] Unit test: request body construction with all options
- [ ] Unit test: response parsing (base64 image extraction)
- [ ] Unit test: file extension selection from MIME type
- [ ] Integration test (with Google API key): generate an image
- [ ] Integration test: generated image written to file, JSON output includes path
- [ ] Integration test: Discord handler delivers file as attachment
- [ ] Integration test: tool is blocked when config has `enabled = false`
- [ ] Integration test: tool fails gracefully when API key is missing
- [ ] Integration test: audit trail records image generation requests
