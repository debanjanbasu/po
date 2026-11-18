# po

**po** is a native, chat-driven AI harness for the desktop. Built on
[gpui-ce](https://github.com/gpui-ce/gpui-ce) (a community fork of Zed's GPUI),
powered by [nano-ai](https://github.com/debanjanbasu/nano-ai), and shaped after
the interaction model of [oh-my-pi](https://github.com/can1357/oh-my-pi).

Everything in po is driven from the chat. Slash commands, the command palette
(cmd-k), and the buttons in the UI all dispatch into the same explicit state
machine — a feature that can be clicked can also be scripted from the
transcript.

## Highlights

- **Native, GPU-accelerated UI** — gpui-ce 0.3.0 with the `gpui_ce_platform`
  bootstrap crate; window, sidebar, transcript, composer, status bar.
- **Local-first inference** — the [nano-ai](https://github.com/debanjanbasu/nano-ai)
  engine (`nano-engine`) is consumed straight from its Git repository, pinned
  to a commit. On Apple Silicon with a linked model, po streams real MiniCPM
  tokens in-process; without one, a deterministic echo provider keeps the
  whole flow demonstrable. Any OpenAI-compatible endpoint (`nano-ai serve`)
  works too.
- **Chat-driven control** — `/session new|list|switch|fork`, `/compact`,
  `/theme`, `/tools`, `/clear`, `/quit` — plus cmd-k palette with live
  filtering and keyboard navigation. One reducer owns the lifecycle; the UI
  (`Entity<T>` + `cx.notify()`) is a projection of it.
- **Append-only sessions** — transcripts are JSONL logs on disk; switch
  reloads from the log, fork writes a branch, replay is deterministic.
- **Tool approvals** — `read_file`, `list_dir`, `grep`, `edit_file` (unified
  diff previews), and `run_shell` run behind an approval card.
- **Auth flows** — local login/signup with salted-SHA-256 credentials; an
  app-mode FSM (login/signup/chat/settings) gates the surfaces.
- **Retro/noir look** — amber-on-near-black bumblebee palette ported 1:1 from
  OKLCH, IBM 3270 typeface bundled in the binary.

## Quick start

```sh
cargo build --workspace
cargo run -p po-app                 # echo provider — full chat flow, no model
```

With a local model (macOS, Apple Silicon):

```sh
ln -s ../nano-ai/models/minicpm-v-4.7-1b/minicpm-v-4.7-1b-tern4.bin \
      models/minicpm-v-4.7-1b-tern4.bin
cargo run -p po-app --features po-app/local-engine
```

Or against any OpenAI-compatible endpoint (`nano-ai serve`):

```sh
# select at boot via config; see crates/po-core/src/provider.rs
```

## Keyboard

| Key | Action |
| --- | --- |
| `cmd-enter` | send the composer line (or pick the highlighted palette row) |
| `cmd-k` | open the command palette |
| `↑` / `↓` / `esc` | palette navigation (while open) |
| `cmd-q` | quit |

## Workspace layout

| Crate | Purpose |
| --- | --- |
| `crates/po-core` | Platform-agnostic harness: agent FSM, sessions, tools, commands, auth, providers. No GUI dependencies. |
| `crates/po-ui` | GPUI presentation: theme, font, widgets, palette, settings, approval card. |
| `crates/po-app` | The `po` binary: window shell and provider boot. |

## Testing

```sh
cargo test --workspace              # unit tests (FSM, sessions, tools, widgets)
cargo clippy --workspace --all-targets
tests/e2e/README.md                 # native e2e harness (po:// deep links)
```

## Configuration

Settings live at `config_dir()/po/config.json`; sessions at
`data_dir()/po/sessions/*.jsonl`; local accounts at
`config_dir()/po/auth.json`. API keys belong in the environment
(`PO_API_KEY`) — never in the repository.

## License

Dual licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at
your option. See [NOTICE](NOTICE) for third-party attribution.
