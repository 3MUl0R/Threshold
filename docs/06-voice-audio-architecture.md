# Voice & Audio Architecture — Patterns for Home Assistant

## Summary

OpenClaw has production-grade voice capabilities that are directly relevant
to the room-portal vision. It supports TTS (3 providers), STT (OpenAI Realtime
API), bidirectional voice calls (Twilio/Telnyx/Plivo), and native mobile
voice interfaces with wake word detection and barge-in support.

This is the most directly useful part of the reconnaissance for our home
assistant project.

---

## Text-to-Speech (TTS)

### Supported Providers

| Provider | API Key Required | Quality | Latency | Cost |
|----------|-----------------|---------|---------|------|
| **ElevenLabs** | Yes | Excellent | Medium | Per-character |
| **OpenAI TTS** | Yes | Very Good | Low | Per-character |
| **Edge TTS** | No | Good | Low | Free |

### Provider Details

**ElevenLabs** (highest quality):
- Models: `eleven_multilingual_v2` and others
- Voice customization: stability, similarity boost, style, speed
- Named voice aliases: `"alice" → "pMsXgVXv3BLzUgSXRplE"`
- Output: MP3, Opus (for Telegram voice notes), PCM (for telephony)

**OpenAI TTS** (best latency):
- Models: `gpt-4o-mini-tts`, `tts-1`, `tts-1-hd`
- Voices: alloy, ash, coral, echo, fable, onyx, nova, sage, shimmer
- Output: MP3, Opus, PCM
- Custom endpoint support via `OPENAI_TTS_BASE_URL`

**Edge TTS** (free fallback):
- Microsoft Edge neural voices (e.g., `en-US-MichelleNeural`)
- No API key, no cost
- Output: MP3, WebM, Ogg Opus, WAV
- Built-in retry with format fallback

### Auto-TTS Modes
- `off` — no automatic speech
- `always` — speak every response
- `inbound` — only speak after voice input (natural back-and-forth)
- `tagged` — only speak `[[tts]]`-tagged segments from the AI response

### TTS Directives
The AI model can embed TTS instructions in responses:
```
[[tts:provider=elevenlabs voice=alice speed=1.2]]
This part will be spoken with custom settings.
```

### Auto-Summarization
When responses exceed length limits (default 1500 chars):
- Runs a separate summarization model call
- Summarizes to ~1500 chars at temperature 0.3
- The full text is still delivered as text; only the TTS is summarized

---

## Speech-to-Text (STT)

### OpenAI Realtime API

Primary STT method — streaming real-time transcription:

- **Protocol**: WebSocket to `wss://api.openai.com/v1/realtime`
- **Audio format**: mu-law (G.711) 8kHz mono (direct from telephony)
- **Voice Activity Detection**: Server-side, configurable threshold (0-1)
- **Partial transcripts**: Real-time streaming as speech is recognized
- **Auto-reconnect**: 5 attempts with exponential backoff

Callbacks:
```typescript
onPartial(text: string)    // Streaming partial transcript
onTranscript(text: string) // Final transcript after silence
onSpeechStart()            // Barge-in: user started speaking
```

### Native Mobile STT
- **iOS**: `SFSpeechRecognizer` (Apple's on-device speech framework)
- **Android**: Native `SpeechRecognizer` API

Both support continuous listening and push-to-talk modes.

---

## Voice Call Pipeline (Telephony)

### Bidirectional Media Streaming

```
Phone Call (PSTN/VoIP)
    │
    ▼
Twilio/Telnyx/Plivo ──WebSocket──→ OpenClaw Gateway
    │                                     │
    │ mu-law 8kHz audio                   │ Forward to STT
    │                                     ▼
    │                              OpenAI Realtime API
    │                                     │
    │                                     │ Transcript
    │                                     ▼
    │                              AI Agent (Claude/GPT)
    │                                     │
    │                                     │ Response text
    │                                     ▼
    │                              TTS Provider
    │                                     │
    │ mu-law 8kHz audio ←────────────────┘
    │ (converted from TTS output)
    ▼
Phone Speaker
```

### Audio Processing Details

| Operation | Details |
|-----------|---------|
| Input codec | mu-law (G.711), 8kHz, mono |
| Resampling | Linear interpolation to target rate |
| Frame size | 20ms chunks for streaming |
| Clipping | 16-bit boundary clamping |
| Output codec | mu-law encoded back for telephony |

### Barge-In Support
When user speaks during TTS playback:
1. `onSpeechStart()` callback fires
2. TTS queue is cleared
3. Audio buffer is flushed
4. System switches to listening mode
5. New transcription begins immediately

---

## Wake Word Detection

### Global Wake Words
- Stored at: `~/.openclaw/settings/voicewake.json`
- Default triggers: `"openclaw"`, `"claude"`, `"computer"`
- Synced across all connected devices via Gateway RPC
- Editable from any device

### Flow
1. Device listens passively for wake word
2. Wake word detected → activate continuous microphone capture
3. Speech captured → STT transcription
4. Transcript → AI agent
5. Response → TTS playback on device
6. Return to passive listening

---

## Mobile Voice Architecture (Talk Mode)

### iOS Implementation

```
SFSpeechRecognizer (on-device)
    │
    │ transcript
    ▼
Gateway WebSocket ──→ chat.send RPC
    │
    │ streaming agent events
    ▼
Incremental TTS (ElevenLabs PCM @ 44.1kHz)
    │
    │ audio chunks
    ▼
AVAudioPlayer (speaker output)
```

Key features:
- **Incremental TTS**: Starts speaking before full response is generated.
  Parses response in real-time, extracts sentence boundaries, generates
  TTS for each segment as it arrives.
- **Interrupt on speech**: User can speak mid-response to interrupt
- **Audio visualization**: Real-time mic level display
- **System TTS fallback**: Uses iOS native TTS if ElevenLabs unavailable

### Android Implementation

Similar architecture with Android-native equivalents:
- `SpeechRecognizer` for STT
- `AudioTrack` for low-latency PCM playback
- 700ms silence window for auto-finalization
- Dynamic noise floor adaptation

---

## Gateway Real-Time Events

The WebSocket gateway supports these voice-relevant events:

| Event | Direction | Purpose |
|-------|-----------|---------|
| `chat.subscribe` | Client→Server | Subscribe to session updates |
| `chat.unsubscribe` | Client→Server | Stop receiving updates |
| `chat` | Server→Client | Session state (final/aborted/error) |
| `agent` | Server→Client | Streaming text chunks |
| `voicewake.changed` | Server→Clients | Wake word list updated |

This is the backbone for real-time voice portals — devices subscribe to
a conversation and receive streaming text that they convert to speech locally.

---

## Takeaways for Our Project

### What to adopt

**1. TTS Provider Fallback Chain**
```
ElevenLabs (best quality) → OpenAI TTS (fastest) → Edge TTS (free)
```
Start with Edge TTS (free, no config) for development. Add ElevenLabs
for production quality. The fallback pattern is sound.

**2. Incremental TTS**
Don't wait for the full AI response before speaking. Parse sentence
boundaries as text streams in and generate TTS per segment. This is
the single biggest latency improvement for voice interfaces.

**3. Gateway WebSocket for Portal Sync**
All portals (room speakers, phone, Discord) connect via persistent
WebSocket. When the AI responds, ALL connected portals in the same
conversation receive the streaming text. Each portal decides locally
whether to TTS it (voice portals) or display it (text portals).

**4. Barge-In**
Users must be able to interrupt the AI mid-response by speaking.
Clear TTS queue, flush audio buffer, switch to listening. Critical
for natural conversation.

**5. Wake Word → Listen → Respond → Idle cycle**
The basic state machine for room portals:
```
IDLE (passive wake word detection)
  → LISTENING (continuous STT)
  → PROCESSING (AI thinking)
  → SPEAKING (TTS playback, interruptible)
  → IDLE
```

### What to redesign

**1. STT should be local-first**
OpenClaw relies on OpenAI Realtime API for STT. For a privacy-focused
home assistant, prefer local STT:
- **Whisper.cpp** — runs locally, excellent accuracy
- **Vosk** — lightweight, offline, many languages
- Fallback to cloud STT only if local fails or user opts in

**2. TTS should support local options too**
- **Piper TTS** — fast local neural TTS, many voices
- **Coqui TTS** — open-source, customizable
- Cloud TTS as opt-in upgrade, not requirement

**3. Wake word detection should be local**
- **OpenWakeWord** — lightweight, customizable trigger words
- **Porcupine** — Picovoice's wake word engine (has free tier)
- Never send audio to cloud until wake word is detected locally

**4. Audio I/O in Rust**
- **cpal** crate — cross-platform audio I/O
- **rodio** crate — audio playback
- **symphonia** crate — audio decoding
- **opus** crate — for Opus encoding/decoding

### Room Portal Hardware Vision

```
Raspberry Pi / old phone / smart speaker
    ├── Microphone array
    ├── Speaker
    ├── Local wake word detection (OpenWakeWord)
    ├── Local STT (Whisper.cpp)
    ├── WebSocket connection to home server
    └── Local TTS playback (Piper or cloud)

Home Server (Rust backend)
    ├── Gateway WebSocket server
    ├── Conversation manager
    ├── CLI subprocess manager (claude/codex)
    ├── Portal registry
    └── Session persistence (JSONL)
```

Each room portal is a thin client that does:
1. Local wake word detection
2. Local STT (or streams audio to server)
3. Sends transcript text over WebSocket
4. Receives response text over WebSocket
5. Local TTS playback

The intelligence lives on the home server. The portals are just
microphone + speaker + network connection.

### Privacy Architecture

```
Audio captured in room
    ↓ (stays local)
Wake word detected locally (never leaves device)
    ↓
Speech-to-text locally (Whisper.cpp on device or server)
    ↓ (only text leaves your network)
Text sent to Claude/Codex CLI
    ↓
Response text received
    ↓ (only text)
Text-to-speech locally (Piper on device or server)
    ↓ (stays local)
Audio played on room speaker
```

Raw audio NEVER leaves your home network. Only transcribed text
goes to the AI provider, and even that goes through the official
CLI's encrypted channel.
