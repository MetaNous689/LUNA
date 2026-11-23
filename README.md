# Luna

A premium terminal system monitor for Linux that also shows what your coding
agents are doing. One full-screen view on a pure black canvas: CPU with per-core
bars, RAM with swap and memory pressure (PSI), GPU and VRAM, network, every
volume, local servers you can stop, the weekly usage limits of Claude Code and
Codex, and the live status of every Claude Code and Codex agent.

Luna stands alone. It needs no other tool, and it reads Claude Code and Codex
from their own backends.

```
╭ CPU ───────────────────────╮╭ RAM ───────────────────╮╭ GPU ───────────────────╮
│ 22 % belegt   78 % frei    ││ 12.0 GiB belegt  von … ││ 1 %   Auslastung       │
│ ████████▊················· ││ ████████████▍········· ││ ▍······················ │
╰────────────────────────────╯╰────────────────────────╯╰────────────────────────╯
╭ WOCHENLIMIT ──────────────╮╭ SERVER ─────────────────╮╭ AGENTEN  9 ────────────╮
│ CODEX   11.0 % genutzt    ││ › python3   :4785       ││ ● api       claude  WORKING │
│ CLAUDE  48.0 % genutzt    ││   steam     :38483      ││ ● docs      codex   BLOCKED │
╰───────────────────────────╯╰─────────────────────────╯╰─────────────────────────╯
```

The interface text is German. Contributions that add other languages are welcome.

## Requirements

> GPU and VRAM come from the AMD `amdgpu` sysfs interface. NVIDIA and Intel GPUs are
> not read yet and show `—`. Support for them is a good first contribution.

- Linux (Luna reads `/proc` and `/sys`; macOS and Windows are not supported)
- Rust 1.88 or newer to build
- `tmux` for the managed background session (`luna on`, `luna off`). Running
  `luna-monitor` directly needs no tmux.
- Optional: `claude` (Claude Code) and `codex` (Codex CLI) for the agent panel
  and the weekly limits, an AMD GPU (`amdgpu` sysfs) for GPU and VRAM readings, `python3` to stop listening processes, and `tailscale` for the optional
  phone view.

## Install

```sh
cargo build --release
./scripts/install.sh
```

The installer puts `luna` in `~/.local/bin` and the monitor in
`~/.local/libexec/luna`. Make sure `~/.local/bin` is on your `PATH`.

```sh
luna          # open the monitor, starting it if needed
luna on       # start it in the background
luna off      # stop it
q or Esc      # leave the view, the monitor keeps running
r             # refresh, ↑/↓ select a server, x stop it (asks first)
```

To run without tmux: `./target/release/luna-monitor --no-pwa`.

## Agents

Luna lists every running agent with its backend, name and one of four states:
`WORKING`, `IDLE`, `BLOCKED`, `DONE`. Status comes from the agents' own backends
and is never guessed.

| Backend | Source | Mapping |
| --- | --- | --- |
| Claude Code, interactive | `sessions/<pid>.json` in `$CLAUDE_CONFIG_DIR` (default `~/.claude`), only for live processes | `busy`, `shell` → WORKING · `waiting` → BLOCKED · `idle` → IDLE |
| Claude Code, background | `claude agents --json` every 15 s, only agents whose worker process in `daemon/roster.json` is alive | `working` → WORKING · `blocked` → BLOCKED · `done` → DONE · `idle` → IDLE |
| Codex | the app-server control socket in `$CODEX_HOME` (default `~/.codex`) | active → WORKING · waiting on approval or input → BLOCKED · idle → IDLE |

Luna shows agents that are in use, not everything a backend remembers:

- working and blocked agents are always shown;
- an idle Claude Code session is shown while it is attached to an open terminal;
- an idle Codex thread is shown once it has a name or a first message, so empty
  drafts are not;
- a background agent is shown only while its worker is running. Agents the CLI
  still lists as blocked or failed, but that nothing runs, are not.

`DONE` is shown for five minutes after an agent that was working turns idle.
Backends only report what an agent does right now, so Luna remembers the
transition. A status Luna does not know is shown without a label.

A backend that is not installed or not running is simply absent. Only names,
folders and states are read. Prompts, histories and tokens in these files are
not.

## Weekly limits

- **Codex:** asked from the running Codex app-server (`account/rateLimits/read`
  over its control socket), weekly window only. Without a running app-server, the
  Codex CLI is asked instead.
- **Claude Code:** asked from the account usage endpoint that Claude Code's
  `/usage` uses. **This endpoint is undocumented and may change or be rate
  limited.** For it, Luna reads the OAuth access token from Claude Code's
  `.credentials.json` and hands it to `curl` on stdin, never as an argument. If
  you do not want that, set `LUNA_NO_CLAUDE_USAGE=1` and the token is never read.

A limit is shown only for an agent that exists on the machine: Codex when its
app-server or CLI is there, Claude Code when a subscription login is. With
neither, the whole panel is left out. A limit is never shown as "unreachable"
just because the tool is not installed.

A rate-limited answer (`429`) is shown as "gedrosselt" with the last known value
kept as stale, and Luna retries after the server's `Retry-After`.

## Phone view (optional)

The monitor serves a read-only PWA on `127.0.0.1:4782`. `luna pwa on` publishes
it to your own tailnet with `tailscale serve`, through a small read-only relay
(`python3`). `luna pwa off` removes only the route Luna created. Nothing is
exposed unless you run `luna pwa on`.

## Privacy

Luna does not scan process arguments, environments, IP addresses, prompts or
histories. Local servers are reduced to name, port and process id. Stopping one
asks for confirmation and only affects your own user services and processes.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --release --all-targets -- -D warnings
cargo test --release
```

`unsafe` code is forbidden. The code is organised as `src/agents/` (Claude Code
and Codex backends), `src/view.rs` (all drawing), `src/dashboard.rs` (state and
input), `src/system_stats.rs`, `src/storage.rs`, `src/services.rs` and
`src/listeners.rs`.

## License

MIT
