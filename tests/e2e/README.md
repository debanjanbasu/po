# po e2e harness

Drives scenarios against real surfaces of the native po app. Entry point:

```bash
node tests/e2e/run.ts --list                        # surfaces + scenarios
node tests/e2e/run.ts --scenario smoke --surfaces macos
```

Every run writes `tests/e2e/runs/<ts>-<SCENARIO>-<surfaces>/` with per-peer
logs, screenshots, and a `result.json` recording what each assertion saw.

## Surfaces

| Surface | Driver | Notes |
|---|---|---|
| `macos` | built `target/release/po` binary, `osascript` deep links (`po://`), `screencapture` | builds via `cargo build --release -p po-app` when stale |

The web/iOS/Android surfaces and the peer-to-peer scenarios migrated from `oo`
live under `surfaces/` and `scenarios/deferred/` until po grows its sync
layer; they are not wired into the default run.

## Build freshness

The macOS surface rebuilds automatically when `target/release/po` is missing
or older than the newest source file, so a stale binary can never silently
invalidate a run.
