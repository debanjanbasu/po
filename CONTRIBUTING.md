# Contributing to po

Thanks for helping improve po.

## Before you start

- Open an issue first for anything larger than a bug fix, so the design can be discussed
  before code is written.
- Keep pull requests focused: one logical change per pull request.

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
cargo build --workspace
```

`po-core` is the platform-agnostic core and must stay free of GUI dependencies — if you
need a new capability, model it as data or an event in `po-core` and project it in `po-ui`.

## Rules

- **No secrets.** Never commit API keys, tokens, model weights, or local session logs. The
  repository `.gitignore` already excludes `.env*`, `*.key`, `*.pem`, `models/`, and session
  data directories.
- **No large binaries.** Model weights and generated assets do not belong in the repository.
  The bundled font is the only binary asset.
- **License headers are not required**, but every contribution is dual licensed under
  MIT OR Apache-2.0, matching the rest of the project.
- **Match the existing style**: rustfmt defaults, descriptive commit messages
  (`feat:`, `fix:`, `docs:`, `chore:`), and unit tests for new `po-core` behaviour.

## Reporting bugs

Please include the po version, your operating system and architecture, the model backend in
use, and a copy of the relevant session JSONL with any secrets redacted.
