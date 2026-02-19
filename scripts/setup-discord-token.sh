#!/bin/bash
# Setup script to store Discord bot token in the secret store (secrets.toml)

set -e

# Parse --data-dir argument (defaults to ~/.threshold)
DATA_DIR="$HOME/.threshold"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --data-dir)
            DATA_DIR="$2"
            shift 2
            ;;
        *)
            echo "Unknown argument: $1"
            echo "Usage: $0 [--data-dir <path>]"
            exit 1
            ;;
    esac
done

# Extract token from .env file
TOKEN=$(grep "^token " .env | awk '{print $2}')

if [ -z "$TOKEN" ]; then
    echo "Error: Could not find Discord token in .env file"
    echo "Expected format: token <YOUR_TOKEN>"
    exit 1
fi

echo "Found Discord token in .env file"
echo "Token: ${TOKEN:0:20}..."

# Option 1: Set as environment variable (for this session)
echo ""
echo "To use as environment variable (temporary):"
echo "  export DISCORD_BOT_TOKEN='$TOKEN'"
echo ""

# Option 2: Store in secrets.toml (permanent, file backend)
SECRETS_FILE="$DATA_DIR/secrets.toml"
echo "Storing in secret store: $SECRETS_FILE"

# Create data directory if needed
mkdir -p "$DATA_DIR"

# Read existing secrets or start fresh
if [ -f "$SECRETS_FILE" ]; then
    # Remove existing discord-bot-token line if present
    EXISTING=$(grep -v '^discord-bot-token' "$SECRETS_FILE" || true)
else
    EXISTING="[secrets]"
fi

# Ensure [secrets] header exists
if ! echo "$EXISTING" | grep -q '^\[secrets\]'; then
    EXISTING="[secrets]
$EXISTING"
fi

# Write updated file atomically
TMP_FILE="$SECRETS_FILE.tmp"
echo "$EXISTING" > "$TMP_FILE"
echo "discord-bot-token = \"$TOKEN\"" >> "$TMP_FILE"
chmod 600 "$TMP_FILE"
mv "$TMP_FILE" "$SECRETS_FILE"

echo "Discord token stored in secret store"
echo "  File: $SECRETS_FILE"
echo "  Key: discord-bot-token"

echo ""
echo "Setup complete! You can now start Threshold."
