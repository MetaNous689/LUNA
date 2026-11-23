#!/bin/sh
set -eu

project_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmux_socket=${LUNA_TMUX_SOCKET:-luna}
monitor_source="$project_dir/target/release/luna-monitor"
launcher_source="$project_dir/scripts/luna"
relay_source="$project_dir/scripts/luna-pwa-relay.py"
relay_unit_source="$project_dir/scripts/luna-pwa-relay.service"
local_bin="${HOME:?HOME must be set}/.local/bin"
libexec_dir="$HOME/.local/libexec/luna"
user_unit_dir="$HOME/.config/systemd/user"
relay_unit_target="$user_unit_dir/luna-pwa-relay.service"
desktop_source="$project_dir/packaging/luna.desktop"
desktop_dir="$HOME/.local/share/applications"
icon_source="$project_dir/src/pwa_assets/icon-512.png"
icon_dir="$HOME/.local/share/icons/hicolor/512x512/apps"

if [ ! -x "$monitor_source" ]; then
    echo "Release-Monitor fehlt: $monitor_source" >&2
    echo "Zuerst cargo build --release --bin luna-monitor ausführen." >&2
    exit 1
fi

if [ ! -f "$relay_source" ]; then
    echo "Luna-PWA-Relay fehlt: $relay_source" >&2
    exit 1
fi
if [ ! -f "$relay_unit_source" ]; then
    echo "Luna-PWA-Relay-Service fehlt: $relay_unit_source" >&2
    exit 1
fi

mkdir -p "$local_bin" "$libexec_dir" "$user_unit_dir" "$desktop_dir" "$icon_dir"

monitor_temp="$libexec_dir/.luna-monitor.$$"
launcher_temp="$local_bin/.luna-launcher.$$"
relay_temp="$libexec_dir/.luna-pwa-relay.$$"
unit_temp="$user_unit_dir/.luna-pwa-relay.$$"
desktop_temp="$desktop_dir/.luna-desktop.$$"
icon_temp="$icon_dir/.luna-icon.$$"
trap 'rm -f "$monitor_temp" "$launcher_temp" "$relay_temp" "$unit_temp" "$desktop_temp" "$icon_temp"' EXIT HUP INT TERM
install -m 755 "$monitor_source" "$monitor_temp"
mv -f "$monitor_temp" "$libexec_dir/luna-monitor"
install -m 755 "$relay_source" "$relay_temp"
mv -f "$relay_temp" "$libexec_dir/luna-pwa-relay.py"
install -m 644 "$relay_unit_source" "$unit_temp"
mv -f "$unit_temp" "$relay_unit_target"
install -m 755 "$launcher_source" "$launcher_temp"
mv -f "$launcher_temp" "$local_bin/luna"
install -m 644 "$desktop_source" "$desktop_temp"
mv -f "$desktop_temp" "$desktop_dir/luna.desktop"
install -m 644 "$icon_source" "$icon_temp"
mv -f "$icon_temp" "$icon_dir/luna.png"
trap - EXIT HUP INT TERM
systemctl --user daemon-reload

# An atomic file replacement does not update an already running monitor.
# Preserve stopped sessions; activate an update only in Luna's own managed pane.
if command -v tmux >/dev/null 2>&1 \
    && tmux -L "$tmux_socket" has-session -t luna-monitor >/dev/null 2>&1; then
    pane_pid=$(tmux -L "$tmux_socket" display-message -p -t luna-monitor:0.0 '#{pane_pid}')
    activation=0
    python3 - "$pane_pid" "$libexec_dir/luna-monitor" <<'PY' || activation=$?
import hashlib
import os
from pathlib import Path
import sys

try:
    process = Path('/proc') / str(int(sys.argv[1]))
    installed = Path(sys.argv[2]).resolve(strict=True)
    executable = os.readlink(process / 'exe').removesuffix(' (deleted)')
    if (process.stat().st_uid != os.getuid()
            or (process / 'comm').read_text().strip() != 'luna-monitor'
            or executable != str(installed)):
        raise ValueError('Not the managed Luna monitor')
    running_hash = hashlib.sha256((process / 'exe').read_bytes()).digest()
    installed_hash = hashlib.sha256(installed.read_bytes()).digest()
except (OSError, ValueError):
    raise SystemExit(2)
raise SystemExit(0 if running_hash == installed_hash else 10)
PY
    case "$activation" in
        0) ;;
        10)
            # Resolve the pane again before respawning it. Preserve a replaced
            # pane instead of disrupting a process started during installation.
            current_pid=$(tmux -L "$tmux_socket" display-message -p -t luna-monitor:0.0 '#{pane_pid}')
            if [ "$current_pid" = "$pane_pid" ]; then
                tmux -L "$tmux_socket" respawn-pane -k -t luna-monitor:0.0 -c "$libexec_dir" \
                    'exec env -u NO_COLOR ./luna-monitor'
                echo "Luna-Monitor auf die installierte Version aktualisiert."
            else
                echo "Luna-Pane hat sich geändert; laufender Prozess blieb unverändert." >&2
            fi
            ;;
        *)
            echo "Luna-Monitorprozess nicht sicher zugeordnet; laufender Prozess blieb unverändert." >&2
            ;;
    esac
fi

echo "Installiert: luna on/off verwaltet den Monitor; luna pwa on/off verwaltet die Tailscale-PWA."
