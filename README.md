# po

**po** is a native desktop AI harness built on [gpui-ce](https://github.com/gpui-ce/gpui-ce),
powered by [micro-ai](https://github.com/debanjanbasu/micro-ai), and shaped after the
interaction model of [oh-my-pi](https://github.com/can1357/oh-my-pi).

Everything in po is driven from the chat. Slash commands, the command palette, and the
buttons in the UI all dispatch into the same explicit state machine, so a feature that can
be clicked can also be scripted from the transcript.

## Highlights

- **Native, GPU-accelerated UI** — `gpui-ce` 0.3.0 (a community fork of Zed's GPUI) with the
  `gpui_ce_platform` bootstrap crate.
- **Local-first inference** — the [micro-ai](https://github.com/debanjanbasu/micro-ai) engine
  (`micro-engine`) is consumed directly from its GitHub repository and pinned to a commit.
  On Apple Silicon it runs the MiniCPM-V 4.7 1B model locally; elsewhere po falls back to an
  OpenAI-compatible remote provider.
- **Append-only sessions** — transcripts are JSONL logs that can be branched, forked, and
  compacted, in the style of oh-my-pi's session format.
- **Explicit agent state machine** — one serializable reducer owns the agent lifecycle;
  the UI is a reactive projection of it (`Entity<T>` + `cx.notify()`).
- **Tool approvals** — file reads, edits, greps, and shell commands run behind an approval
  prompt, with diff previews for edits.
- **IBM 3270 typography** — the bundled IBM 3270 font is the app's typeface.
- **Orange bumblebee theme** — light and dark palettes ported from the sibling `oo` project.

## Workspace layout

| Crate | Purpose |
| --- | --- |
| `crates/po-core` | Platform-agnostic harness: sessions, agent FSM, tools, commands, config, providers. No GUI dependencies. |
| `crates/po-ui` | GPUI presentation layer: orange theme, IBM 3270 font, chat widgets. |
| `crates/po-app` | The `po` binary: window shell, transcript, composer, status bar, command palette. |

## Building

```bash
cargo build --workspace
cargo run -p po-app
cargo test --workspace
```

Requirements: Rust 1.101 or newer (see `rust-toolchain.toml`; the pinned nightly is used for
reproducibility) and, for local inference, an Apple Silicon Mac.

## Configuration

po reads settings from `dirs::config_dir()/po/config.json` and keeps session transcripts under
`dirs::data_dir()/po/sessions/`. Keys such as provider API tokens belong in the environment
(`PO_API_KEY`) or an untracked local config file — never in the repository.

## License

Dual licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
See [NOTICE](NOTICE) for third-party attribution.
