#!/bin/bash
# Setup script to store Discord bot token in the secret store (secrets.toml)

set -e

# Parse --data-dir argument (defaults to ~/.threshold)
DATA_DIR="$HOME/.threshold"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --data-dir)
            if [[ $# -lt 2 ]] || [[ -z "$2" ]]; then
                echo "Error: --data-dir requires a non-empty value"
                echo "Usage: $0 [--data-dir <path>]"
                exit 1
            fi
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

# Build new secrets.toml preserving existing keys.
# Uses a simple approach: read existing [secrets] key-value pairs,
# replace or add discord-bot-token, write back under [secrets] only.
TMP_FILE="$SECRETS_FILE.tmp"

{
    echo "[secrets]"

    # Copy existing key = value lines (except discord-bot-token) from [secrets] section
    if [ -f "$SECRETS_FILE" ]; then
        in_secrets=false
        while IFS= read -r line; do
            # Track which section we're in (allow leading whitespace, spaces, and quotes)
            stripped="${line#"${line%%[![:space:]]*}"}"
            if [[ "$stripped" =~ ^\[[[:space:]]*(secrets|\"secrets\"|\'secrets\')[[:space:]]*\][[:space:]]*(#.*)?$ ]]; then
                in_secrets=true
                continue
            # Match other TOML sections: require alpha/underscore/quote after [
            # to avoid matching array values like [3,4] in multiline arrays.
            # Note: string arrays like ["a","b"] can still false-positive, but
            # secrets.toml only contains simple key = "value" pairs, never arrays.
            elif [[ "$stripped" =~ ^\[[[:space:]]*[[:alpha:]_\"\'].*\][[:space:]]*(#.*)?$ ]]; then
                in_secrets=false
                continue
            fi
            # Copy non-empty lines from [secrets] section, except discord-bot-token
            # Handle bare key, single-quoted, and double-quoted TOML key forms
            if $in_secrets && [[ -n "$line" ]] && \
               [[ ! "$stripped" =~ ^discord-bot-token[[:space:]]*= ]] && \
               [[ ! "$stripped" =~ ^\"discord-bot-token\"[[:space:]]*= ]] && \
               [[ ! "$stripped" =~ ^\'discord-bot-token\'[[:space:]]*= ]]; then
                echo "$line"
            fi
        done < "$SECRETS_FILE"
    fi

    # Escape any double-quotes in token value for valid TOML
    ESCAPED_TOKEN="${TOKEN//\"/\\\"}"
    echo "discord-bot-token = \"$ESCAPED_TOKEN\""
} > "$TMP_FILE"

chmod 600 "$TMP_FILE"
mv "$TMP_FILE" "$SECRETS_FILE"

echo "Discord token stored in secret store"
echo "  File: $SECRETS_FILE"
echo "  Key: discord-bot-token"

echo ""
echo "Setup complete! You can now start Threshold."
