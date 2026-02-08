# ElevenLabs Integration — Detailed Analysis

## Summary

OpenClaw's ElevenLabs integration is **exclusively text-to-speech** — no
Conversational AI, no voice cloning, no sound effects. But the TTS integration
is production-grade and spans three distinct subsystems:

1. **Gateway TTS** (`messages.tts` config) — buffered HTTP, serves chat replies
2. **Talk Mode** (`talk.*` config) — streamed PCM on iOS/macOS/Android, direct API
3. **Voice-Call Plugin** — telephony TTS for Twilio (not all providers use ElevenLabs)

These subsystems have different config surfaces, directive formats, and streaming
behavior. This document covers all three but labels which subsystem each section
applies to.

For our project, ElevenLabs would be an optional cloud TTS upgrade, not a
default dependency. But the integration patterns here are worth studying.

---

## API Integration

### Endpoint
```
POST https://api.elevenlabs.io/v1/text-to-speech/{voiceId}?output_format={format}
```

### Authentication
Header: `xi-api-key: <apiKey>`

Resolution order:
1. Config override (`messages.tts.elevenlabs.apiKey`)
2. Environment: `ELEVENLABS_API_KEY`
3. Environment: `XI_API_KEY` (legacy)

### Request Payload
```json
{
  "text": "string",
  "model_id": "eleven_multilingual_v2",
  "seed": 12345,
  "apply_text_normalization": "auto",
  "language_code": "en",
  "voice_settings": {
    "stability": 0.5,
    "similarity_boost": 0.75,
    "style": 0.0,
    "use_speaker_boost": true,
    "speed": 1.0
  }
}
```

### Response
Binary audio buffer in the requested format (MP3, Opus, or PCM).

**Important subsystem distinction:**
- **Gateway TTS**: buffered HTTP — waits for complete audio buffer before delivery
- **Talk Mode (mobile)**: uses streaming synthesis (`streamSynthesize()`) — plays
  PCM chunks as they arrive for low-latency playback

### Timeout
Default 30 seconds, configurable. Uses `AbortController` for cancellation.

---

## Models

### Advertised in Gateway RPC (`tts.providers`)

| Model ID | Description |
|----------|-------------|
| `eleven_multilingual_v2` | Default for gateway. Multi-language, high quality |
| `eleven_turbo_v2_5` | Lower latency, slightly lower quality |
| `eleven_monolingual_v1` | English only, legacy |

### Talk Mode Defaults
Mobile apps default to `eleven_v3` (iOS/macOS/Android). The runtime accepts
any model ID configured — the list above is what the RPC advertises, not a
hard constraint.

---

## Voice Configuration

### Voice IDs
- Format: 10-40 alphanumeric characters (validated by regex)
- Default voice ID: `pMsXgVXv3BLzUgSXRplE`
- Passed as URL path parameter, not in body

### Voice Aliases (Talk Mode only)
Friendly names mapped to voice IDs in config:
```json
{
  "talk": {
    "voiceAliases": {
      "alice": "pMsXgVXv3BLzUgSXRplE",
      "bella": "EXAVITQu4vr4xnSDxMaL",
      "rachel": "21m00Tcm4TlvDq8ikWAM"
    }
  }
}
```

Resolution: case-insensitive lookup. If alias not found, treated as raw voice ID.

**Important**: voice aliases are resolved in Talk Mode (mobile apps) only. The
gateway TTS directive parser (`[[tts:voiceId=...]]`) validates voice IDs as
10-40 alphanumeric characters and does NOT perform alias resolution. Using an
alias name in a gateway directive will fail validation.

### Voice Settings

| Setting | Range | Default | Description |
|---------|-------|---------|-------------|
| `stability` | 0–1 | 0.5 | Higher = more consistent, less expressive |
| `similarityBoost` | 0–1 | 0.75 | Fidelity to original voice |
| `style` | 0–1 | 0.0 | Style exaggeration (v2 models only) |
| `useSpeakerBoost` | bool | true | Clarity enhancement |
| `speed` | 0.5–2.0 | 1.0 | Playback speed |

All settings validated before API call with range assertions.

---

## Platform-Specific Output Formats

### Gateway TTS (chat replies)

| Destination | Format String | Details |
|------------|---------------|---------|
| Default | `mp3_44100_128` | MP3 @ 44.1kHz, 128kbps |
| Telegram voice notes | `opus_48000_64` | Opus @ 48kHz, 64kbps |

Gateway selects format by channel only (Telegram gets Opus, everything else MP3).

### Talk Mode (mobile apps — direct API)

| Platform | Format String | Details |
|----------|---------------|---------|
| iOS/macOS | `pcm_44100` | Raw PCM @ 44.1kHz, streamed playback |
| Android | `pcm_24000` | Raw PCM @ 24kHz, AudioTrack playback |

Mobile defaults are in Talk runtime config, not `messages.tts`.

### Voice-Call Plugin (telephony)

| Provider | Format | Details |
|----------|--------|---------|
| Twilio (streaming) | `pcm_22050` | PCM → resample → mu-law 8kHz G.711 |
| Telnyx | Provider-native TTS | Uses Telnyx `speak` action, not ElevenLabs |
| Plivo | Provider-native TTS | Uses Plivo TTS action, not ElevenLabs |

**Important**: only the Twilio streaming path uses ElevenLabs for voice calls.
Telnyx and Plivo use their own provider-native TTS actions. Non-streaming Twilio
falls back to TwiML `<Say>`.

---

## AI-Controlled Voice Directives

The AI model can embed TTS control directives in its responses:

### Gateway Directive Syntax (`[[tts:...]]` tags)
```
[[tts:provider=elevenlabs voiceId=pMsXgVXv3BLzUgSXRplE stability=0.4 speed=1.1]]
This text will be spoken with custom settings.
[[tts:text]]Alternative speech text (different from displayed text)[[/tts:text]]
```

Note: `voiceId` must be a raw ElevenLabs ID (10-40 alphanumeric), not an alias.

### Talk Mode Directive Format (JSON first-line)
Mobile Talk Mode uses a different directive format — JSON on the first line
of the response, parsed by `TalkDirective.swift` / equivalent Kotlin code.
Not interchangeable with gateway `[[tts:...]]` tags.

### Available Gateway Directive Parameters
- `provider` — switch TTS provider mid-response
- `voiceId` — ElevenLabs voice ID (raw ID only, no alias resolution)
- `modelId` — model selection
- `stability`, `similarityBoost`, `style`, `speed` — voice settings
- `speakerBoost` / `useSpeakerBoost` — clarity toggle
- `normalize` / `applyTextNormalization` — text normalization mode
- `language` / `languageCode` — synthesis language
- `seed` — deterministic output

### Policy Control
Administrators can restrict which directives the AI is allowed to use:
```json
{
  "modelOverrides": {
    "enabled": true,
    "allowText": true,
    "allowProvider": false,
    "allowVoice": true,
    "allowModelId": false,
    "allowVoiceSettings": true,
    "allowNormalization": true,
    "allowSeed": false
  }
}
```

---

## Auto-TTS Modes

| Mode | Behavior |
|------|----------|
| `off` | No automatic speech |
| `always` | Speak every response |
| `inbound` | Speak only after voice input (natural conversation) |
| `tagged` | Only speak `[[tts:...]]`-tagged segments |

Preference hierarchy: session override → user prefs (`~/.openclaw/settings/tts.json`)
→ gateway config → default (`off`).

---

## Auto-Summarization for Long Responses

When response text exceeds `maxLength` (default 1500 chars):
1. A separate LLM call summarizes to ~1500 chars
2. Temperature: 0.3 (consistent, concise)
3. The summary is spoken; the full text is still delivered as text
4. Prevents long API calls and expensive character billing

**Auth dependency**: summarization uses the agent's primary model, which
requires valid model authentication. If auth fails, falls back to truncation
instead of summarization.

---

## Provider Fallback Chain (Gateway TTS)

The fallback order is **dynamic**, not fixed. The selected/configured provider
is tried first, then remaining providers with valid API keys:

```
Selected provider (user's choice or config default)
    ↓ on failure
Next available provider (has API key configured)
    ↓ on failure
Edge TTS (always available, free, no key — unless explicitly disabled)
    ↓ on failure
Error returned to caller
```

If the user selects OpenAI as primary, ElevenLabs becomes the fallback (not the
other way around). Each failure (4xx, 5xx, timeout) is caught and the next
provider attempted. Edge TTS has its own internal retry with format fallback.

---

## Mobile Implementations

### iOS
- Calls ElevenLabs API **directly from the device** (not through gateway)
- Uses `PCMStreamingAudioPlayer` for low-latency streamed playback
- **Incremental TTS**: parses response in real-time, extracts sentence boundaries,
  generates TTS per segment as text arrives. Starts speaking before full response.
- Falls back to `AVSpeechSynthesizer` (system TTS) when no API key
- Supports barge-in: user speech interrupts TTS playback
- Voice resolution: tries alias → falls back to `listVoices()` API → first result

### macOS
- Also calls ElevenLabs directly, similar architecture to iOS
- **Does NOT use incremental TTS** — synthesizes full cleaned text in one call
  (unlike iOS which queues sentence-by-sentence)
- Falls back to system speech synthesizer

### Android
- Also calls ElevenLabs directly from device
- `AudioTrack` for low-latency PCM playback, `MediaPlayer` for MP3
- 700ms silence detection window for auto-finalization
- Falls back to Android `TextToSpeech` system service
- Default format: `pcm_24000`

### Key Pattern
Mobile devices call ElevenLabs directly rather than routing through the gateway.
This reduces latency (one fewer network hop) and keeps the gateway from being
a bottleneck for audio data.

---

## Voice Call Integration

**Only the Twilio streaming path uses ElevenLabs.** Telnyx and Plivo use their
own provider-native TTS actions and do not route through ElevenLabs at all.

For Twilio streaming:
- Separate TTS config per voice-call plugin (deep-merged with core config)
- Different voice/settings for calls vs. chat is common
- Audio conversion pipeline: PCM 22kHz → resample → mu-law 8kHz
- Non-streaming Twilio falls back to TwiML `<Say>` (provider-native)

---

## Gateway RPC Methods

| Method | Returns |
|--------|---------|
| `tts.status` | Enabled state, active provider, available providers, API key presence |
| `tts.enable` / `tts.disable` | Toggle TTS |
| `tts.setProvider` | Switch active provider |
| `tts.convert` | Generate audio from text, return file path |
| `tts.providers` | List all providers with configured status and available models |

---

## User Commands

```
/tts on                              Enable TTS
/tts off                             Disable TTS
/tts status                          Show current config
/tts provider [openai|elevenlabs|edge]   Switch provider
/tts limit [chars]                   Set text length limit
/tts summary [on|off]                Toggle auto-summarization
/tts audio <text>                    Generate speech from text

/voice status                        Show configured voice        (requires talk-voice extension)
/voice list [limit]                  List available ElevenLabs voices  (requires talk-voice extension)
/voice set <id|alias>                Set default voice              (requires talk-voice extension)
```

Note: `/voice` commands are provided by the optional `talk-voice` extension,
not built into core. They also require `talk.apiKey` to be configured.

---

## What ElevenLabs is NOT Used For

- Conversational AI / Agent API
- Voice cloning
- Sound effects or non-speech audio generation
- WebSocket streaming API (gateway uses buffered HTTP; mobile uses streamed HTTP)
- Pronunciations API
- Project/workspace management

---

## Takeaways for Our Project

### What to adopt
- **Fallback chain pattern** — try premium provider, fall back to free. Essential
  for reliability. ElevenLabs → OpenAI → Piper (local) for our system.
- **AI-controlled voice directives** — letting the model adjust voice parameters
  contextually is genuinely useful. Calmer voice for evening, energetic for morning.
- **Auto-summarization** — summarize long responses for speech while delivering
  full text. Saves TTS costs and improves listening experience.
- **Platform-specific output formats** — each playback device has optimal formats.
  Don't send MP3 when PCM is better for low-latency streaming.
- **Voice aliases** — friendly names are much better UX than raw voice IDs.

### What to do differently
- **Local TTS first** — Piper TTS runs locally with near-zero latency and no cost.
  ElevenLabs should be opt-in for premium quality, not the default.
- **WebSocket streaming** — ElevenLabs offers a WebSocket streaming API that
  OpenClaw doesn't use. For room portals where latency matters, this would be
  worth exploring if the user opts into cloud TTS.
- **Device-side TTS for room portals** — mobile devices calling ElevenLabs directly
  is a good pattern. Room portals (Raspberry Pi) should do the same with local
  Piper, only routing to cloud if configured.
- **Rust audio pipeline** — use `rodio` / `cpal` / `symphonia` crates instead
  of Node.js Buffer manipulation for audio processing. More efficient for
  the always-on server.
