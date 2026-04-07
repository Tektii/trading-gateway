## What

<!-- Brief description of the change -->

## Why

<!-- Link to issue or explain motivation -->

## How to test

<!-- How can a reviewer verify this works? -->

## Broker impact

<!-- Which adapters does this affect? All / specific / none -->

## Checklist

- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy --workspace --all-features -- -D warnings` passes
- [ ] `cargo nextest run --workspace --all-features` passes
- [ ] OpenAPI spec regenerated if routes changed (`cargo run --bin openapi-export`)
- [ ] No new `unwrap()` or `expect()` in non-test code
