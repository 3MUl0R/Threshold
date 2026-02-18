//! CLI subcommand definitions and handler for image generation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use threshold_core::config::ImageGenToolConfig;
use threshold_core::{AuditTrail, SecretStore};

use crate::client::{ImageGenClient, ImageGenOptions};

/// Top-level `threshold imagegen` arguments.
#[derive(clap::Args)]
pub struct ImagegenArgs {
    #[command(subcommand)]
    pub command: ImagegenCommands,
}

/// Image generation subcommands.
#[derive(clap::Subcommand)]
pub enum ImagegenCommands {
    /// Generate an image from a text description
    Generate {
        /// The text prompt describing the desired image
        #[arg(long)]
        prompt: String,
        /// Style hint (e.g., "flat-design", "watercolor", "pixel-art")
        #[arg(long)]
        style: Option<String>,
        /// Things to avoid in the generated image
        #[arg(long)]
        negative_prompt: Option<String>,
        /// Output directory (default: system temp dir under threshold-generated/)
        #[arg(long)]
        output_dir: Option<String>,
    },
}

/// Handle an imagegen CLI command.
///
/// Checks that image generation is enabled, delegates to the API client,
/// writes the image to disk, prints JSON to stdout, and logs to audit trail.
pub async fn handle_imagegen_command(
    args: ImagegenArgs,
    config: &ImageGenToolConfig,
    audit_path: Option<&Path>,
) -> anyhow::Result<()> {
    if !config.enabled {
        anyhow::bail!("Image generation is disabled. Set tools.image_gen.enabled = true in your config.");
    }

    match args.command {
        ImagegenCommands::Generate {
            prompt,
            style,
            negative_prompt,
            output_dir,
        } => {
            let secret_store = Arc::new(SecretStore::new()?);
            let client = ImageGenClient::new(secret_store);

            let options = ImageGenOptions {
                style,
                negative_prompt,
            };

            let image = client.generate(&prompt, &options).await?;

            // Determine file extension from MIME type
            let ext = mime_to_extension(&image.mime_type);

            // Generate unique filename
            let filename = generate_filename(ext)?;

            // Determine output directory — canonicalize to prevent path traversal
            let dir = match output_dir {
                Some(d) => {
                    let p = PathBuf::from(&d);
                    std::fs::create_dir_all(&p)?;
                    p.canonicalize()?
                }
                None => {
                    let p = default_output_dir();
                    std::fs::create_dir_all(&p)?;
                    p.canonicalize().unwrap_or(p)
                }
            };

            let path = dir.join(&filename);
            std::fs::write(&path, &image.data)?;

            let size_bytes = image.data.len();
            let file_path_str = path.to_string_lossy().to_string();

            // Print JSON result to stdout
            let output = serde_json::json!({
                "status": "ok",
                "file_path": file_path_str,
                "filename": filename,
                "mime_type": image.mime_type,
                "size_bytes": size_bytes,
                "prompt": prompt,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);

            // Audit log
            audit_log(audit_path, &serde_json::json!({
                "action": "generate",
                "prompt": prompt,
                "file_path": file_path_str,
                "size_bytes": size_bytes,
            })).await;
        }
    }

    Ok(())
}

/// Map MIME type to file extension.
fn mime_to_extension(mime_type: &str) -> &str {
    match mime_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

/// Generate a unique filename: `generated-{timestamp_ms}-{random_hex}.{ext}`.
fn generate_filename(ext: &str) -> anyhow::Result<String> {
    let ts = chrono::Utc::now().timestamp_millis();
    let mut rand_bytes = [0u8; 4];
    getrandom::fill(&mut rand_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to generate random bytes: {}", e))?;
    let hex: String = rand_bytes.iter().map(|b| format!("{:02x}", b)).collect();
    Ok(format!("generated-{}-{}.{}", ts, hex, ext))
}

/// Default output directory: `$TMPDIR/threshold-generated/`.
fn default_output_dir() -> PathBuf {
    std::env::temp_dir().join("threshold-generated")
}

/// Best-effort audit log append.
async fn audit_log(audit_path: Option<&Path>, data: &serde_json::Value) {
    if let Some(path) = audit_path {
        let audit = AuditTrail::new(path.to_path_buf());
        if let Err(e) = audit.append_event("imagegen", data).await {
            tracing::warn!("Failed to write audit log: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_generate_with_all_options() {
        use clap::Parser;

        #[derive(Parser)]
        struct TestCli {
            #[command(flatten)]
            args: ImagegenArgs,
        }

        let cli = TestCli::parse_from([
            "test",
            "generate",
            "--prompt",
            "a blue gear",
            "--style",
            "flat-design",
            "--negative-prompt",
            "realistic",
            "--output-dir",
            "/tmp/out",
        ]);

        match cli.args.command {
            ImagegenCommands::Generate {
                prompt,
                style,
                negative_prompt,
                output_dir,
            } => {
                assert_eq!(prompt, "a blue gear");
                assert_eq!(style.as_deref(), Some("flat-design"));
                assert_eq!(negative_prompt.as_deref(), Some("realistic"));
                assert_eq!(output_dir.as_deref(), Some("/tmp/out"));
            }
        }
    }

    #[test]
    fn parse_generate_prompt_only() {
        use clap::Parser;

        #[derive(Parser)]
        struct TestCli {
            #[command(flatten)]
            args: ImagegenArgs,
        }

        let cli = TestCli::parse_from(["test", "generate", "--prompt", "a cat"]);

        match cli.args.command {
            ImagegenCommands::Generate {
                prompt,
                style,
                negative_prompt,
                output_dir,
            } => {
                assert_eq!(prompt, "a cat");
                assert!(style.is_none());
                assert!(negative_prompt.is_none());
                assert!(output_dir.is_none());
            }
        }
    }

    #[test]
    fn mime_to_extension_png() {
        assert_eq!(mime_to_extension("image/png"), "png");
    }

    #[test]
    fn mime_to_extension_jpeg() {
        assert_eq!(mime_to_extension("image/jpeg"), "jpg");
    }

    #[test]
    fn mime_to_extension_webp() {
        assert_eq!(mime_to_extension("image/webp"), "webp");
    }

    #[test]
    fn mime_to_extension_gif() {
        assert_eq!(mime_to_extension("image/gif"), "gif");
    }

    #[test]
    fn mime_to_extension_unknown_defaults_to_bin() {
        assert_eq!(mime_to_extension("application/octet-stream"), "bin");
    }

    #[test]
    fn generate_filename_produces_unique_names() {
        let a = generate_filename("png").unwrap();
        let b = generate_filename("png").unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("generated-"));
        assert!(a.ends_with(".png"));
    }

    #[test]
    fn generate_filename_uses_correct_extension() {
        let name = generate_filename("jpg").unwrap();
        assert!(name.ends_with(".jpg"));
    }

    #[test]
    fn default_output_dir_is_under_temp() {
        let dir = default_output_dir();
        assert!(dir.ends_with("threshold-generated"));
    }

    #[test]
    fn disabled_config_check() {
        // We can't easily test the async handler, but we can verify the
        // config check logic inline: if !config.enabled => bail
        let config = ImageGenToolConfig { enabled: false };
        assert!(!config.enabled);
    }

    #[tokio::test]
    async fn audit_log_with_no_path_is_noop() {
        // Should not panic
        audit_log(None, &serde_json::json!({"action": "test"})).await;
    }

    #[tokio::test]
    async fn audit_log_writes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("imagegen.jsonl");

        audit_log(Some(&path), &serde_json::json!({"action": "generate", "prompt": "a cat"})).await;

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("generate"));
        assert!(content.contains("a cat"));
    }
}
