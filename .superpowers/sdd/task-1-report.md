# Task 1: Configuration Contract

## Implementation

- Added public `StreamContinuationConfig` in `src/target.rs` with serde support.
- Added conservative serde defaults: `max_attempts = 1`, `max_buffered_bytes = 1024 * 1024`, and `idle_timeout_ms = None`.
- Added `FallbackConfig::stream_continuation: Option<StreamContinuationConfig>` with `#[serde(default)]`, preserving existing `FallbackConfig` construction patterns using `Default`.
- Added `StreamContinuationConfig::enabled_for_path`, which requires the feature to be enabled and matches configured endpoint paths exactly.

## Files Changed

- `src/target.rs`
- `.superpowers/sdd/task-1-report.md`

## TDD Evidence

### RED

Added the two required tests before production implementation and ran:

```text
cargo test target::tests::stream_continuation -- --nocapture
```

Result: compilation failed as expected. The compiler reported that `StreamContinuationConfig` did not exist and that `FallbackConfig` had no `stream_continuation` field.

### GREEN

Implemented the contract and reran:

```text
cargo test target::tests::stream_continuation -- --nocapture
```

Result: 2 passed, 0 failed; 420 filtered out. The binary target had 0 tests.

## Verification

Final focused test:

```text
cargo test target::tests::stream_continuation -- --nocapture
```

Result: 2 passed, 0 failed; 420 filtered out. The binary target had 0 tests.

Directly affected target tests:

```text
cargo test target::tests -- --nocapture
```

Result: 67 passed, 0 failed; 355 filtered out. The binary target had 0 tests.

Formatting and diff checks:

```text
rustfmt --edition 2024 src/target.rs
git diff --check
```

Result: passed.

```text
cargo fmt --all -- --check
```

Result: failed because of pre-existing formatting differences in `src/lib.rs` and `src/response_sanitizer.rs`. Those unrelated files were not modified.

## Self-Review

- The public names, field types, serde attributes, defaults, and exact endpoint predicate match the task brief.
- The feature remains opt-in: a missing configuration field deserializes to `None`, and an enabled config with no matching endpoint returns `false`.
- Existing `FallbackConfig` literals using `..Default::default()` remain source-compatible; the full target test module compiled and passed.
- No later stream continuation behavior was added.

## Concerns

- Repository-wide `cargo fmt --all -- --check` remains red due to unrelated pre-existing formatting drift outside this task.

## Commit

The implementation and this report are committed as:

`feat: add stream continuation configuration contract`
