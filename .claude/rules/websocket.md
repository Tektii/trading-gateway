---
globs:
  - "**/websocket/**"
  - "**/websocket.rs"
  - "**/streaming.rs"
---

# WebSocket Conventions

Key types: `WebSocketProvider` trait (per-broker stream), `EventRouter` (normalises and broadcasts), `ReconnectionHandler` (backoff state machine), `StalenessTracker` (marks instruments stale on disconnect).

**Critical ordering invariant in EventRouter:**
1. `StateManager` updated FIRST (order/position cache)
2. Fills routed to `ExitHandler` for SL/TP placement
3. Events broadcast to strategy clients LAST

The `handle_ack` method on `WebSocketProvider` controls time progression for the Tektii engine provider but is a no-op for live providers.
