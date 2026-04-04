---
globs:
  - "**/exit_management/**"
---

# Exit Management Conventions

Placeholder IDs use prefixes `exit:sl:` and `exit:tp:` — use `ExitHandler::is_placeholder_id()` to detect them.

Two separate circuit breakers exist: `AdapterCircuitBreaker` (in each adapter, for broker API failures) and `ExitOrderCircuitBreaker` (in `ExitHandler`, for exit order placement failures). Don't confuse them.

For testing: use `MockExitHandler` from test-support (records `handle_fill` calls via `.fill_calls()`). The `test-utils` feature gate on core exposes `insert_entry()` for directly manipulating exit state.
