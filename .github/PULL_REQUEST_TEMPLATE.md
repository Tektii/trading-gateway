## What

<!-- Brief description of the change -->

## Why

<!-- Link to issue or explain motivation -->

## Checklist

- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy --workspace --all-features -- -D warnings` passes
- [ ] `cargo nextest run --workspace --all-features` passes
- [ ] OpenAPI spec regenerated if routes changed (`cargo run --bin openapi-export`)
