---
name: verify
description: Build, launch, and drive vix (a crossterm/ratatui TUI) headlessly to verify changes end-to-end.
---

# Verifying vix

Build: `cargo build --release` → `./target/release/vix`.

No tmux on this machine — use GNU screen (`/usr/bin/screen`) to drive the TUI:

```sh
# Isolate persistence so dogfooding never touches ~/.local/share/vix:
screen -dmS vixv env VIX_DATA_DIR=<tempdir> ./target/release/vix
sleep 1.5                                    # let the file scan stream in
screen -S vixv -p 0 -X stuff "moti"          # type into the omnibox
screen -S vixv -p 0 -X hardcopy /tmp/cap.txt # dump the visible pane
screen -S vixv -p 0 -X width -w 40           # live-resize (degrade testing)
```

Gotchas learned the hard way:
- `stuff '...\r'` sends a literal backslash-r into the query — use shell `$'\r'` (Enter), `$'\t'` (Tab), `$'\033'` (Esc), `$'\020'` (Ctrl-P), `$'\016'` (Ctrl-N), `$'\177'` (Backspace).
- `hardcopy` needs `-p 0` or it writes an empty file; captures can race a mid-stream redraw — wait ~2s after launch before trusting footer/count lines.
- Hardcopy mangles `·` and box-drawing borders; assert on text content, not chrome.
- Esc at launch quits the whole process (by design), which kills the screen session — `screen -ls | grep -c <name>` returning 0 is the assertion.

Flows worth driving: launch omnibox (recents vs file-list fallback), query blend (file rows above `path:line` snippet rows), Tab filter cycle All→Files→Content, Enter on a grep hit (statusline shows the jumped-to line), Ctrl-P + Esc in-editor (must NOT quit), `:Grep pat` (Content-filtered prefill), narrow degrade at 40×11.
