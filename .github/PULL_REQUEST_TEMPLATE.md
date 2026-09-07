## Summary

<!-- What changed, and why? -->

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --workspace`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Windows cross-build or manual testing, if the change touches Windows-only code

## Safety impact

<!-- Describe any effect on reference counting, the protected-file rules, snapshot creation or restore, the preview default, or the schema-version guard. Write "none" if there is none. -->
