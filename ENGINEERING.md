# Threshold Engineering Principles

These principles are expected across all contributors (human or AI).

## 1) Conversation-Centric First

- Conversations are the primary unit of continuity.
- Interfaces are portals into conversations, not separate systems of record.
- New features should preserve shared context, memory, and auditability across interfaces.

## 2) Interface-Agnostic Portals

- Keep interface-specific behavior at the edges (`discord`, `web`, future platform crates).
- Avoid leaking interface assumptions into `core` and `conversation` unless unavoidable.
- Favor abstractions that make adding new portals incremental.

## 3) Provider-Agnostic Runtime Direction

- Current runtime uses Claude CLI.
- Design should keep provider boundaries explicit so additional providers can be integrated cleanly.
- Avoid provider-specific coupling in shared types or core orchestration paths.

## 4) Persistence and Auditability

- Conversation state and scheduler state must survive restart.
- Important actions should be auditable (JSONL trail, structured events).
- Schema evolution must preserve backward compatibility (`#[serde(default)]`, additive fields).

## 5) Operational Safety Over Convenience

- Daemon lifecycle actions must be safe by default.
- Prefer build-first and drain-aware restart semantics over fast-but-risky operations.
- Favor explicit failure behavior and rollback paths over implicit fallback magic.

## 6) Concurrency Discipline

- Respect documented lock ordering and avoid lock inversions.
- Do not hold locks across expensive/blocking operations where avoidable.
- Keep asynchronous paths cancellation-safe and avoid leaked in-flight state.

## 7) Quality Bar

- New behavior includes tests for success, failure, and compatibility paths.
- Run formatting/linting/tests before merge.
- Documentation must be updated when command surface or semantics change.

## 8) Review Standard

- Reviews prioritize correctness, regressions, and missing tests.
- Findings are addressed before merge (or explicitly tracked with rationale).
- “Works locally” is not sufficient without representative test coverage.
