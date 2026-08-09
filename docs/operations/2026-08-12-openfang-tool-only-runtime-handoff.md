# OpenFang tool-only runtime handoff — 2026-08-12

## Delivered boundary

`[runtime] tool_only = true` is a model-free execution profile. It does not
select, initialize, call, or fall back to a model provider.

The built-in-tool classifier in `openfang-runtime::tool_runner` is the single
semantic source for model-inference requirements. The five classified builtins
are `media_describe`, `media_transcribe`, `image_generate`, `text_to_speech`,
and `speech_to_text`.

`OpenFangKernel::available_tools` applies that classification before every
consumer of the effective tool projection. Consequently, a tool-only MCP
caller cannot discover those tools with `tools/list`, and an attempted
`tools/call` is rejected as not permitted before runtime admission or the tool
runner dispatch boundary.

Configuration reload now validates the runtime profile before computing or
applying hot actions. A reload that adds explicit model-runtime configuration
to a tool-only instance fails closed.

## Evidence

Focused RED/GREEN evidence from this change:

- RED: the unrestricted tool-only kernel initially projected `media_describe`;
  the projection regression failed as intended.
- GREEN: the kernel projection test excludes all five model-inference builtins.
- RED/GREEN: a real HTTP MCP call with enabled runtime admission and the
  lifecycle-owned dispatch observer has a deterministic positive control. With
  the filter temporarily bypassed, `media_describe` created a new execution
  receipt (RED); with the filter restored, valid admission references leave
  both admission rows and the dispatch counter unchanged for that model-tool
  request (GREEN).
- GREEN: real HTTP MCP integration tests confirm both filtered `tools/list`
  and rejected `tools/call(media_describe)`.
- GREEN: the reload regression rejects an explicit non-default model provider
  before hot actions can run.

Validation recorded before publication:

- `cargo test -p openfang-types --lib`: 423 passed.
- `cargo test -p openfang-kernel --lib`: 307 passed.
- `cargo test -p openfang-runtime --lib`: 1,015 passed.
- `cargo test -p openfang-api --lib`: 96 passed.
- `cargo check --workspace`: passed.
- `rustfmt` was applied to the three changed Rust files; `git diff --check`
  passed. Repository-wide `cargo fmt --all -- --check` still reports unrelated
  pre-existing formatting drift in `background.rs` and `python_runtime.rs`, so
  those files were intentionally left untouched.

## Operational invariant and non-claims

The profile boundary is limited to tool eligibility and model-runtime
configuration validation. It is not evidence that an OpenFang daemon was
started, that an MCP client connected, or that any external tool ran.

No model, provider, runtime daemon, Proxmox host, or external service was
configured, contacted, started, or changed for this delivery. The tests use
only local temporary state and the in-process HTTP router.
