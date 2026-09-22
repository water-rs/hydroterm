# PARITY.md — hydroterm modern-terminal checklist

Scope: capabilities that Ghostty, kitty and WezTerm share — the working
definition of "modern terminal" we build against. This is NOT a feature-by-
feature clone of any one emulator: config format, keybindings and interaction
design are our own.

- **Shell UI is WaterUI**: tab bar, split layout, search bar, settings and
  command palette are composed from WaterUI views. Only terminal cell content
  goes through the GPU surface. Missing WaterUI capabilities are logged in
  WATERUI_FEEDBACK.md.
- **Metrics are ours**: vttest/esctest pass rates and throughput/latency
  numbers are measured on this build alone (no cross-emulator comparison).
- **Config format**: Ghostty-style `key = value` lines (flat, one setting per
  line, comments `#`). Chosen over TOML/JSON because the file is
  hand-editable, trivially diffable, and needs no sections for a flat option
  space; `keybind` is repeatable.

Status legend: ✅ implemented · 🟡 partial · ❌ missing · 🚫 not applicable / blocked (see WATERUI_FEEDBACK.md)

| Area | Feature | Status | How verified / gap |
|------|---------|--------|--------------------|
| Core | PTY + VT100/xterm emulation (alacritty_terminal 0.26) | ✅ | run `vim`, `less`, `htop` — alt screen, scroll region, DECAWM |
| Core | True color + 256/indexed + named palette | ✅ | `printf '\e[38;2;200;100;50mcolored\e[0m\n'` screenshot |
| Core | Scrollback buffer + scrollbar | ✅ | `seq 1 500`, wheel scroll; HISTORY_LINES cap |
| Core | Synchronized output (mode 2026) | ✅ | alacritty parser buffers frames; no wakeup mid-sync |
| Core | Text reflow on resize | ❌ | alacritty grid is fixed-width — upstream gap |
| Input | Full key encoding (arrows, F-keys, keypad, modifiers) | ✅ | `showkey -a` |
| Input | Kitty keyboard protocol (disambiguate/event types/alternates) | ✅ | `kitty +kitten show_key -m kitty` |
| Input | IME / preedit at caret + composition | ✅ | type CJK via IME; preedit drawn at caret |
| Input | Mouse: X10/normal/SGR + UTF8 + all-motion + focus events | ✅ | `vim` mouse mode, `htop` clicks |
| Input | Bracketed paste | ✅ | paste into `cat -v` shows \x1b[200~ wrap |
| Input | Alternate scroll (wheel → arrows in alt screen) | ✅ | scroll in `less` |
| Clipboard | Copy selection (primary + clipboard) | ✅ | drag + Ctrl+Shift+C, `xclip -o` verify |
| Clipboard | Paste via Ctrl+Shift+V / middle-click (bracketed) | ✅ | paste `hello` at prompt |
| Clipboard | OSC 52 write + read | ✅ | `printf '\e]52;c;aGVsbG8=\a'` → `xclip -o` = hello |
| Selection | Click-drag / word (dbl) / line (triple), right-click extend | ✅ | drag/select; timing multi-click |
| Selection | Copy-on-select | ❌ | config key planned |
| Links | OSC 8 hyperlink render + Ctrl+click open | ✅ | `printf '\e]8;;https://example.com\e\\link\e]8;;\e\\'` |
| Links | Plain-text URL detect + open | ❌ | regex over grid rows on click — planned |
| Tabs | New/close/select/cycle tabs | ✅ | Ctrl+Shift+T / Ctrl+Shift+W / Ctrl+1..8 |
| Splits | Horizontal/vertical split, resize, focus nav | ❌ | pane tree inside tab — planned (needs custom container; hydrolysis has no splitter) |
| Windows | Multi-window | 🚫 | hydrolysis winit runner spawns 1 window — needs upstream API |
| Quick terminal | Drop-down/global-hotkey terminal | 🚫 | X11 global grab needs x11 dep; deferred |
| Config | `key = value` config file (~/.config/hydroterm/config) | ❌ | parser + unit tests planned |
| Config | Hot reload on file change | ❌ | mtime poll → apply |
| Config | `keybind` bindings (trigger=action) | ❌ | planned |
| Theme | Built-in themes | ❌ | planned |
| Theme | Auto light/dark follow system | ❌ | `gsettings monitor` when present |
| Font | Fallback chain (fontconfig) | ✅ | missing glyph → fallback font |
| Font | Ligatures | 🟡 | rustybuzz shapes runs — verify `=>`, `!=` visually |
| Font | Emoji + grapheme clusters (ZWJ, VS16) | 🟡 | single emoji ok; ZWJ via segmentation — verify |
| Font | Synthetic bold/italic | ✅ | stroke emboss / skew |
| Font | Color emoji (COLR/CBDT) | ❌ | outline-only pipeline; bitmap emoji needs blit path |
| Font | Zoom in/out/reset | ✅ | Ctrl+Shift +/- and 0 |
| Shell int. | zsh/fish integration auto-inject | 🟡 | bash auto-inject via `--rcfile`; zsh/fish TODO |
| Shell int. | OSC 133 prompt marks + jump prev/next prompt (Ctrl+Shift+Up/Down) | ✅ | auto-injected bash rc emits marks; verified: scrollback jump lands mark at viewport top |
| Shell int. | OSC 7 cwd → new tab inherits cwd | ✅ | bash integration auto-reports; verified: new tab spawns in /tmp |
| Shell int. | Shell-integration scripts (bash/zsh/fish inject) | ❌ | inject via ENV at spawn |
| Shell int. | OSC 9/777 notifications | 🟡 | tap emits; surface via notifier |
| Bell | Visual bell (flash) | ✅ | Bell event → flash overlay |
| Bell | Audible bell | ❌ | needs audio stack; low priority |
| Search | Scrollback search + highlight + jump | ✅ | Ctrl+Shift+F |
| A11y | Screen reader (accesskit) | 🚫 | GpuSurface can't emit a11y tree — WATERUI_FEEDBACK #10 |
| Graphics | kitty image protocol | ❌ | APC captured by tap; decode+render planned |
| Graphics | sixel | 🚫 | None of Ghostty/kitty-ref/WezTerm treat sixel as core — skipped |
| Perf | Scrolling/render performance | 🟡 | llvmpipe here; benchmark task pending |
| Window | Title reporting (OSC 0/2) | ✅ | title → window title binding |
| Window | Fullscreen | 🟡 | WindowHandle.fullscreen exists — needs keybind |
| Window | Confirm-close when child running | 🚫 | no close-request hook — WATERUI_FEEDBACK #4 |
| Misc | `-e cmd`, `--config` args | ❌ | planned with config work |
| Misc | Quit action, process-group cleanup | ✅ | window close → Shutdown |

## Metrics (to be filled by benchmark task)

| Test | Result | Method |
|------|--------|--------|
| vttest | _pending_ | `vttest` suite in hydroterm |
| esctest | _pending_ | esctest subset |
| Throughput | _pending_ | `time cat bigfile` (≥64MB) in hydroterm |
| Input latency | _pending_ | keypress→echo render time |
