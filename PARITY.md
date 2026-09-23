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
| Core | Underline variants + strikethrough + SGR 58 | ✅ | `\e[4:1..5m` single/double/curly/dotted/dashed + `\e[9m` strike + `\e[58;2;..m` colored underline — all verified in one live screenshot |
| Core | Scrollback buffer + scrollbar | ✅ | `seq 1 500`, wheel scroll; HISTORY_LINES cap |
| Core | Synchronized output (mode 2026) | ✅ | alacritty parser buffers frames; no wakeup mid-sync |
| Core | Text reflow on resize | ✅ | upstream `Grid::resize(reflow)` re-wraps soft-wrapped primary-screen rows on column change (alt screen untouched; cursor anchored). Verified live: window resize narrow↔wide, font zoom, split → pane-zoom full width, unzoom, pane close — wrap-chains rejoin byte-identically, zero residue (the reported mid-row prompt duplication is gone) |
| Input | Full key encoding (arrows, F-keys, keypad, modifiers) | ✅ | `showkey -a` |
| Input | Kitty keyboard protocol (disambiguate/event types/alternates) | ✅ | `kitty +kitten show_key -m kitty` |
| Input | IME / preedit at caret + composition | ✅ | type CJK via IME; preedit drawn at caret |
| Input | Mouse: X10/normal/SGR + UTF8 + all-motion + focus events | ✅ | `vim` mouse mode, `htop` clicks |
| Input | Bracketed paste | ✅ | paste into `cat -v` shows \x1b[200~ wrap |
| Input | Alternate scroll (wheel → arrows in alt screen) | ✅ | scroll in `less` |
| Input | Keyboard scrolling | ✅ | Shift+Up/Down scrolls a line, Shift+PageUp/PageDown a page, Ctrl+Shift+Home/End jumps to scrollback top/bottom (verified live) |
| Input | Snap to bottom on keyboard input | ✅ | a key that writes bytes to the PTY (`key_to_bytes` path) snaps the viewport to the live edge — the alacritty/kitty convention (verified live: scroll into scrollback, type, viewport jumps to bottom) |
| Clipboard | Copy selection (primary + clipboard) | ✅ | drag + Ctrl+Shift+C, `xclip -o` verify |
| Clipboard | Paste via Ctrl+Shift+V / Shift+Insert / middle-click (bracketed) | ✅ | paste `hello` at prompt |
| Clipboard | OSC 52 write + read | ✅ | `printf '\e]52;c;aGVsbG8=\a'` → `xclip -o` = hello |
| Selection | Click-drag / word (dbl) / line (triple), right-click extend | ✅ | drag/select; timing multi-click |
| Selection | Copy-on-select | ✅ | `copy-on-select = true` in config |
| Links | OSC 8 hyperlink render + Ctrl+click open | ✅ | `printf '\e]8;;https://example.com\e\\link\e]8;;\e\\'` |
| Links | Plain-text URL detect + open | ✅ | scheme-scan over the clicked row's cells; Ctrl+click → xdg-open (verified: navigated Chrome) |
| Links | URL hint mode | ✅ | Ctrl+Shift+U draws opaque Accent badges (AccentContainer chip, AccentForeground label) at each link's start + an Accent underline over the URL span — link text stays readable; typing the label + Enter opens it via xdg-open, digits/Backspace edit, Esc cancels (verified live). URLs are detected on logical lines: a URL split across a soft wrap gets one hint whose underline spans both rows |
| Tabs | New/close/select/cycle tabs | ✅ | Ctrl+Shift+T / Ctrl+Shift+W / Ctrl+1..8 |
| Tabs | Move tab left/right | ✅ | Ctrl+Shift+PageUp/PageDown reorder the tab strip (`AppState::move_tab(∓1)` on the `NamiList`) |
| Splits | Horizontal/vertical split, close pane | ✅ | Ctrl+Shift+E / Ctrl+Shift+D; verified live (nested split per-pane, close collapses) |
| Splits | Pane zoom toggle (tmux-style) | ✅ | Ctrl+Shift+Z → focused pane fills the tab (`PaneTab.zoomed: Binding<Option<u64>>`); toggling restores the split — verified live both ways |
| Splits | Pane resize (drag divider) | 🚫 | equal splits via HStack/VStack — no resizable-divider view in WaterUI (feedback #13 → water-rs/waterui#1203) |
| Splits | Focus nav: click + cycle + directional keybinds | 🟡 | click focuses ✓; Ctrl+Shift+[ / ] cycle and Ctrl+Alt+Arrows jump directionally through the split tree (nearest leaf in that direction) — both move the focused-pane record; GUI key focus still needs a click until hydrolysis#90 lands (also #19/#103 — keys keep routing to a hidden focused surface) |
| Windows | Multi-window | ✅ | Ctrl+Shift+N → `Window::show(env)` via the runner's `WindowManager` mounts a real winit window with a fresh session set (verified live: `<2>` title, independent session) |
| Quick terminal | Drop-down/global-hotkey terminal | ✅ | F12 via X11 `GrabKey` on root (4 lock-mask variants — `ModMask::ANY` conflicts with WM grabs; x11rb connection on a grab thread → channel → `spawn_local` drain → `WindowState` toggle). `conditional_window` mounts a `WindowStyle::Borderless` non-resizable window docked top / full-width / 45% height with its own persistent session set (verified live: toggle open/close, scrollback + shell preserved across cycles, keys reach its surface after click-focus per #90). Position applied while the X11 window is still invisible is dropped by the WM (hydrolysis#105) — NO app workaround by design: until the fix lands the drop-down may open wherever the window manager places it (size still applies; position may not). Verified live: toggles open/close, preserved session |
| Config | `key = value` config file (~/.config/hydroterm/config) | ✅ | 8 parser unit tests; template auto-written |
| Config | Hot reload on file change | ✅ | mtime poll (400ms); verified live font-size + theme swap |
| Config | `keybind = chord=action` incl. unbind | ✅ | unit tests + live `ctrl+shift+q=quit` verified |
| Theme | Built-in themes (hydroterm-dark/light, solarized-dark/light) | ✅ | `theme =` config; live swap verified |
| Theme | `theme = auto` follows desktop color-scheme | ✅ | gsettings get at resolve + `gsettings monitor` subscription re-resolves palette live (KDE box lacks GNOME schemas — dark fallback verified; monitor spawn is no-op there) |
| Cursor | `cursor-shape`/`cursor-blink` config | ✅ | `cursor-shape = block|beam|underline|hollow` + `cursor-blink = bool`; hot-reloads via `set_options` rebuild (r13: previously spawn-only) — verified: `beam` renders a blinking bar at the live prompt |
| Cursor | DECSCUSR escape-driven shape (`\e[N q`) | ✅ | `\e[6 q` → steady bar cursor verified live (block/beam/underline/hollow all drawn) |
| Font | Fallback chain (fontconfig) | ✅ | 中文カタ rendered via Noto CJK fallback (screenshot-verified) |
| Font | Ligatures | ✅ | `->`, `=>`, `!=`, `ffi`, `fl` shape to ligature glyphs (screenshot-verified) |
| Font | Emoji + grapheme clusters (ZWJ, VS16) | ✅ | single emoji ✅, flag merges ✅, combining é ✅; 👨‍👩‍👧‍👦 occupies **exactly 2 cells** (app-side `fixup_graphemes` merges ZWJ-split scalar cells into one cluster cell; `zwj_cluster_occupies_two_cells` test). **Color attribution proven (r8 item 1)**: (a) Chrome on this VM renders `A👨‍👩‍👧‍👦B 🇫🇷C` as `AB C` — zero glyphs, no color anywhere; (b) `fc-match -s 'emoji'` resolves every scalar to `/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf`, whose GSUB maps the family sequence to `glyph02113` — its extracted CBDT bitmap is a **monochrome boxed pictograph by the font's own design**; (c) `zwj_cluster_shapes_as_one_ligature` test byte-compares the shaped run's font data to NotoColorEmoji.ttf — equal. Our fallback uses the same font Chrome would; the monochrome artwork is the font's own, not a fallback bug |
| Font | Synthetic bold/italic | ✅ | stroke emboss / skew — `\e[1m`/`\e[3m` visually distinct |
| Font | Color emoji (CBDT) | ✅ | Noto Color Emoji bitmaps rasterize in color (screenshot-verified) |
| Font | Zoom in/out/reset | ✅ | Ctrl+Shift+`=`/`+` up, Ctrl+Shift+`-` down, Ctrl+Shift+`0` reset — verified live |
| Scrollback | Clear scrollback | ✅ | Ctrl+Shift+K — `grid().clear_history()` + snap to bottom (verified live) |
| Scrollback | Open scrollback in editor | ✅ | Ctrl+Shift+G — dumps scrollback + screen via `bounds_to_string` to a temp file, opens `$VISUAL`/`$EDITOR` (fallback `vi`) on it in a new tab (verified live) |
| Shell int. | zsh/fish integration auto-inject | ✅ | bash `--rcfile`, zsh `ZDOTDIR` env, fish `-C` init — all emit OSC 133 A/B/C/D + OSC 7 (verified live) |
| Shell int. | OSC 133 prompt marks + jump prev/next prompt (Ctrl+Shift+Up/Down) | ✅ | auto-injected integration for bash (--rcfile), zsh (ZDOTDIR), fish (-C): all emit A/B/C/D; verified live jump in zsh + fish |
| Shell int. | OSC 7 cwd → new tab inherits cwd | ✅ | all three shells report cwd; verified: fish tab title shows cwd, new bash tab spawns in /tmp |
| Shell int. | OSC 9/777 notifications | ✅ | bell flash + 🔔 badge on title until user input + freedesktop `notify-send` desktop notification where available (badge verified; notify-send spawned, best-effort) |
| Shell int. | Copy last command output | ✅ | Ctrl+Shift+O copies the grid rows between the last OSC 133 C (output-start) and D (output-end) marks — verified live via xclip; text left of the C mark on its row comes along (same as a full-row copy) |
| Bell | Visual bell (flash) | ✅ | Bell event → flash overlay |
| Bell | Audible bell | ✅ | `audible-bell = true` rings the X11 keyboard bell via `xkbbell` (what xterm rings), 120 ms throttled — verified end-to-end with a logging shim |
| Search | Scrollback search + highlight + jump | ✅ | Ctrl+Shift+F opens a WaterUI `TextField` bar (query bound via nami); matches found on logical lines (soft wraps joined via `LineMap`) and mapped back to grid cells — a match crossing a wrap highlights both parts; re-runs on reflow/output via a (cols, history, lines, content_gen) stamp; Enter jumps to next |
| Search | Prev/next match navigation | ✅ | Shift+Enter jumps to previous match; bar ↑/↓ buttons step up/down (`SearchNext`/`SearchPrev` actions, wrap-around via `rem_euclid`, scrolls match to mid-viewport) — verified live both directions |
| Shell UI | Settings page | ✅ | Ctrl+Shift+, → opaque token panel over a dim mask; labeled theme picker, font-size value + stepper, toggle; Apply writes config file → hot reload |
| Shell UI | Command palette | ✅ | Ctrl+Shift+P → WaterUI field + `List` rows (name left, chord right); panel `max_height` capped inside window, list scrolls internally with a scrollbar, `ScrollController<usize>` keeps the selected row visible on Up/Down, Enter/click runs (rows are `button`s — List styles them via the row env; `.on_tap` inside `List` rows never fires — water-rs/hydrolysis#111), Esc closes |
| Shell UI | Context menu (right-click) | 🟡 | Copy / Paste / Select All / Clear / Search wired via `.context_menu` on the pane surface (hit-testing fixed upstream by water-rs/hydrolysis#114). The menu list is a `Computed<Vec<MenuItem>>` over `Session::mouse_reporting`: while the program reports the mouse (DECSET 1000/1002/1006) the list is empty so the secondary click falls through to the embedded surface as program input — verified live both ways. **Blocked upstream:** the popup window mounts correctly-sized at the right origin but paints only the uniform Surface fill — zero item pixels, items can't be clicked (WATERUI_FEEDBACK #25, minimal repro in the hydrolysis checkout) |
| A11y | Screen reader (accesskit) | 🚫 | GpuSurface can't emit a11y tree — WATERUI_FEEDBACK #10 |
| Graphics | kitty image protocol | 🟡 | `a=T` PNG/RGB/RGBA + `m=` chunks + `i=`/`p=` image+placement ids + `\x1b_G…;OK` replies + `t=d` direct/`t=f` file/`t=s` shared-memory mediums + `x,y,w,h` source crop (verified: solid half drawn) + signed `z` z-index (`z<0` over cell bg, below text — verified with text over image) + `c`/`r` cell span + `a=d` delete with `d=a|i|p|z|c` selectors (verified live) + palette/grayscale/16-bit PNG normalize; no unicode placements/animation |
| Graphics | sixel | 🚫 | None of Ghostty/kitty-ref/WezTerm treat sixel as core — skipped |
| Perf | Scrolling/render performance | 🟡 | 10.8 MB/s cat-throughput, ~72ms input latency (llvmpipe CPU render — see Metrics) |
| Window | Title reporting (OSC 0/2) | ✅ | title → window title binding |
| Window | Fullscreen | ✅ | F11 (plain) / `keybind = ...=fullscreen` — own `WindowState` binding → `Fullscreen::Borderless` via `App::new_with_windows`. r13: exit-to-maximized bug verified FIXED upstream at d957748 (#122+#123 frame-push-on-change) — F11 exit restores pre-fullscreen 800×600; still broken at delivered dev pin 47b1081 until those PRs land on dev (WATERUI_FEEDBACK #26) |
| Window | Confirm-close when child running | 🚫 | no close-request hook — WATERUI_FEEDBACK #4 |
| Window | Background transparency | ✅ | `background-opacity` config live-verified after hydrolysis#109 landed (transparency-capable alpha mode + `with_transparent` in the pin): 0.55 composites to a blended (254,247,255) against the desktop, 1.0 gives exact base3 (253,246,227) |
| Misc | `-e cmd`, `--config` args | ✅ | verified: `-e bash -c 'echo E_MARKER'` |
| Misc | Quit action, process-group cleanup | ✅ | window close → Shutdown |

## Metrics (to be filled by benchmark task)

| Test | Result | Method |
|------|--------|--------|
| vttest | visual spot-check only (interactive suite, no automated score) | menu + "test of cursor movements" DECALN frame rendered correctly on 2026-09-22 run |
| Line discipline | Prompt redraw / wide-char wrap | ✅ | OSC133 marks in injected PS1/PS0 wrapped in `\[ \]` (bash) / `%{ %}` (zsh) so readline counts zero prompt-width cells — unit tests `bash/zsh_prompt_marks_are_zero_width`; live: paste CJK+emoji long line, arrow keys, end-of-line wrap — no ghosts |
| esctest | 57/559 pass (10%) | `esctest --expected-terminal xterm`, 2026-09-22; ~450 tests verify screen contents via DECRQCRA (CSI ... * y) which alacritty_terminal does not implement — those all timeout-fail. Real failures: winops reports (14t/18t resize-px), BS/wrap semantics, OSC4 color queries |
| Throughput | 48.7MB in 4.53s (~10.8 MB/s, llvmpipe CPU render) | `time cat` of 800k-line file; pty backpressure = read+parse+render drain |
| Input latency | ~72ms keypress→PTY byte (upper bound, includes X/xdotool dispatch; adds ≤1-2 render frames for echo) | raw-tty `os.read` timestamp vs send timestamp, same clock |
| Input | Paste protection | ✅ | `paste-protection` / `clipboard-paste-protection` config (default on): multi-line paste while bracketed-paste is disarmed stashes the text and shows a confirmation via the framework `Snackbar` overlay mounted by `Window::new` — no hand-rolled scrim layer (r13: the earlier grey-column artifact was our overlay sized to content; the snackbar layers correctly). Paste button / Enter commits, Esc drops (snackbar is single-action — no Cancel slot, see WATERUI_FEEDBACK #30), all other typing swallowed; armed bracketed mode pastes through unguarded like Ghostty's `clipboard-paste-bracketed-safe`; verified end-to-end in `sh` (r13_snack/2) |
| Font | `font-family` config + hot reload | ✅ | config key takes a comma-separated fontconfig-style list incl. generic aliases (monospace/sans-serif/serif/cursive/fantasy/system-ui/emoji/math); resolves through `FontCollection::family_by_name`, falls back to the built-in mono scan; hot-reloaded live via `reload_family` (Liberation Mono picked up in a running session) |
| Buffer | `scrollback-limit` config + hot reload | ✅ | `scrollback = N` in config; applied live to running sessions through `Term::set_options` (alacritty's own reconfigure path) — verified: appending `scrollback = 40` to the config of a running session truncated its history immediately (top of buffer shows line 40 of 100) |
| Input | Mouse-hide-while-typing | ✅ | `mouse-hide-while-typing` config (default on): typing calls XFixes `hide_cursor` on our own toplevel via x11rb (any pointer event calls `show_cursor`) — verified: hide/show requests succeed with real sequence numbers (seq 9/10/11) alternating with each typed char and pointer move |
| Config | `window-padding` config + hot reload | ✅ | `window-padding = N` (0-200pt, or `window-padding-x`/`window-padding-y` separately) drives a `.padding_with(Computed<EdgeInsets>)` around each pane's `SceneView` — verified: appending `window-padding = 20` to a running session's config re-insets the grid with a ~27px margin (r13_pad) |
| Config | `term` config | ✅ | `term = hydroterm-test` sets `$TERM` for spawned shells (`Terminal::spawn` env map) — verified: new tab's `echo $TERM` prints `hydroterm-test` (r13_term2) |
| Config | `osc52-write` / `clipboard-write` config | ✅ | `osc52-write = allow|deny` gates program-initiated `TermEvent::ClipboardStore` writes — verified live: with `deny`, `printf '\e]52;c;aGk=\a'` left the clipboard's old content intact (r13_term2) |
| Selection | Double-click word / triple-click line | ✅ | multi-click by timing (400ms/±1cell window → `SelectionType::Semantic`/`Lines`) — verified live via pixel check on solarized-light's subtle base2 `selection_bg`: double-click selected `target_word` (cols 26-167), triple-click the full row to col 766 (r13_dbl4/tri2) |
| Links | Ctrl+click opens plain-text + OSC8 URLs | ✅ | `open_link_at`: OSC8 `hyperlink().uri()` first, else scheme-scan of the logical line under the click — verified live: Ctrl+click on `https://example.com/p` launched Chrome to Example Domain |
| Input loss | 0 lost / 10,000 (lossless) | `xdotool type --delay 5` ×20 of a known 500-char string into raw-mode `cat`; received text byte-identical. Drain rate ~13-20 keys/s — see Key throughput below (earlier 'dropped/garbled input' reports were this drain-lag plus focus bugs #90/#103, not byte loss). Test artifact note: `cat >>file` in canonical mode truncates at 4095 bytes (kernel MAX_CANON), which is why the first capture looked truncated |
| Key throughput | burst drains ~16-21 keys/s while echo repaints; ~10,000/s with echo off | `HYDROTERM_INPUT_STATS=1` + python-xlib XTEST burst (300 keys in 19ms → all 900 X events arrive inside 15ms). With `stty -echo` all 300 keys deliver into ONE draw (p50 gap 0.10ms) — the framework batches queued input before drawing, no per-key-redraw serialization. With shell echo on, each key's echo schedules a redraw (draw 7ms + llvmpipe present ~40-50ms) that interleaves between input deliveries → ~54ms/key. Bottleneck = software-render present cost, not event delivery or app invalidation (plain keys return needs_frame=false) |
