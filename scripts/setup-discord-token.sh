#!/bin/bash
# Setup script to store Discord bot token in keychain

set -e

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

# Option 2: Store in macOS keychain (permanent)
if [[ "$OSTYPE" == "darwin"* ]]; then
    echo "Storing in macOS Keychain..."
    security add-generic-password \
        -a "$USER" \
        -s "threshold" \
        -w "$TOKEN" \
        -l "discord-bot-token" \
        -U 2>/dev/null || {
        # Update if already exists
        security delete-generic-password -s "threshold" -a "$USER" 2>/dev/null || true
        security add-generic-password \
            -a "$USER" \
            -s "threshold" \
            -w "$TOKEN" \
            -l "discord-bot-token"
    }
    echo "✅ Discord token stored in macOS Keychain"
    echo "   Service: threshold"
    echo "   Account: discord-bot-token"
else
    echo "⚠️  Not on macOS - using environment variable instead"
    echo "   Add this to your shell profile:"
    echo "   export DISCORD_BOT_TOKEN='$TOKEN'"
fi

echo ""
echo "Setup complete! You can now start Threshold."
