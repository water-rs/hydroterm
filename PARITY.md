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
| Selection | Copy-on-select | ✅ | `copy-on-select = true` in config |
| Links | OSC 8 hyperlink render + Ctrl+click open | ✅ | `printf '\e]8;;https://example.com\e\\link\e]8;;\e\\'` |
| Links | Plain-text URL detect + open | ❌ | regex over grid rows on click — planned |
| Tabs | New/close/select/cycle tabs | ✅ | Ctrl+Shift+T / Ctrl+Shift+W / Ctrl+1..8 |
| Splits | Horizontal/vertical split, close pane | ✅ | Ctrl+Shift+E / Ctrl+Shift+D; verified live (nested split per-pane, close collapses) |
| Splits | Pane resize (drag divider) | 🚫 | equal splits via HStack/VStack — no resizable-divider view in WaterUI (feedback #18) |
| Splits | Focus nav: click + cycle keybinds | 🟡 | click focuses ✓; Ctrl+Shift+[ / ] cycle the focused-pane record but hydrolysis has no programmatic focus (feedback #1/#16) |
| Windows | Multi-window | 🚫 | hydrolysis winit runner spawns 1 window — needs upstream API |
| Quick terminal | Drop-down/global-hotkey terminal | 🚫 | X11 global grab needs x11 dep; deferred |
| Config | `key = value` config file (~/.config/hydroterm/config) | ✅ | 8 parser unit tests; template auto-written |
| Config | Hot reload on file change | ✅ | mtime poll (400ms); verified live font-size + theme swap |
| Config | `keybind = chord=action` incl. unbind | ✅ | unit tests + live `ctrl+shift+q=quit` verified |
| Theme | Built-in themes (hydroterm-dark/light, solarized-dark/light) | ✅ | `theme =` config; live swap verified |
| Theme | `theme = auto` follows desktop color-scheme | 🟡 | resolved per (re)load via gsettings; no live OS-change subscription |
| Cursor | `cursor-shape`/`cursor-blink` config | ✅ | spawn-time (alacritty has no config setter) |
| Font | Fallback chain (fontconfig) | ✅ | 中文カタ rendered via Noto CJK fallback (screenshot-verified) |
| Font | Ligatures | ✅ | `->`, `=>`, `!=`, `ffi`, `fl` shape to ligature glyphs (screenshot-verified) |
| Font | Emoji + grapheme clusters (ZWJ, VS16) | 🟡 | single emoji ✅, regional-indicator flag merges ✅, combining é ✅; ZWJ family renders as separate heads (no cross-cell merge) |
| Font | Synthetic bold/italic | ✅ | stroke emboss / skew — `\e[1m`/`\e[3m` visually distinct |
| Font | Color emoji (CBDT) | ✅ | Noto Color Emoji bitmaps rasterize in color (screenshot-verified) |
| Font | Zoom in/out/reset | ✅ | Ctrl+Shift +/- and 0 |
| Shell int. | zsh/fish integration auto-inject | 🟡 | bash auto-inject via `--rcfile`; zsh/fish TODO |
| Shell int. | OSC 133 prompt marks + jump prev/next prompt (Ctrl+Shift+Up/Down) | ✅ | auto-injected bash rc emits marks; verified: scrollback jump lands mark at viewport top |
| Shell int. | OSC 7 cwd → new tab inherits cwd | ✅ | bash integration auto-reports; verified: new tab spawns in /tmp |
| Shell int. | Shell-integration scripts (bash/zsh/fish inject) | ❌ | inject via ENV at spawn |
| Shell int. | OSC 9/777 notifications | 🟡 | tap emits; surface via notifier |
| Bell | Visual bell (flash) | ✅ | Bell event → flash overlay |
| Bell | Audible bell | ❌ | needs audio stack; low priority |
| Search | Scrollback search + highlight + jump | ✅ | Ctrl+Shift+F opens a WaterUI `TextField` bar (query bound via nami); matches live-highlight in grid, Enter jumps to next |
| Shell UI | Settings page | 🟡 | deferred — config file + hot reload covers it; no settings GUI yet |
| Shell UI | Command palette | 🟡 | deferred — all actions reachable via keybinds; palette not built yet |
| A11y | Screen reader (accesskit) | 🚫 | GpuSurface can't emit a11y tree — WATERUI_FEEDBACK #10 |
| Graphics | kitty image protocol | ❌ | APC captured by tap; decode+render planned |
| Graphics | sixel | 🚫 | None of Ghostty/kitty-ref/WezTerm treat sixel as core — skipped |
| Perf | Scrolling/render performance | 🟡 | llvmpipe here; benchmark task pending |
| Window | Title reporting (OSC 0/2) | ✅ | title → window title binding |
| Window | Fullscreen | 🟡 | WindowHandle.fullscreen exists — needs keybind |
| Window | Confirm-close when child running | 🚫 | no close-request hook — WATERUI_FEEDBACK #4 |
| Misc | `-e cmd`, `--config` args | ✅ | verified: `-e bash -c 'echo E_MARKER'` |
| Misc | Quit action, process-group cleanup | ✅ | window close → Shutdown |

## Metrics (to be filled by benchmark task)

| Test | Result | Method |
|------|--------|--------|
| vttest | visual spot-check only (interactive suite, no automated score) | menu + "test of cursor movements" DECALN frame rendered correctly on 2026-09-22 run |
| esctest | 57/559 pass (10%) | `esctest --expected-terminal xterm`, 2026-09-22; ~450 tests verify screen contents via DECRQCRA (CSI ... * y) which alacritty_terminal does not implement — those all timeout-fail. Real failures: winops reports (14t/18t resize-px), BS/wrap semantics, OSC4 color queries |
| Throughput | 48.7MB in 4.53s (~10.8 MB/s, llvmpipe CPU render) | `time cat` of 800k-line file; pty backpressure = read+parse+render drain |
| Input latency | ~72ms keypress→PTY byte (upper bound, includes X/xdotool dispatch; adds ≤1-2 render frames for echo) | raw-tty `os.read` timestamp vs send timestamp, same clock |
