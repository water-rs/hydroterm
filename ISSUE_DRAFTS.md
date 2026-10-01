# Issue drafts — r-next round

Observed-on pins: waterui `9dfe6bef45d7e33c358161cbcb5af47a56640dcf` (dev tip via
`water channel dev`), hydrolysis `06f0b2fafbd0e0fb722cacf993d56cd87f661666` (dev
tip, carries Maximized/level/resize-increments/attention), water CLI at repo
`water-rs/cli` dev, lints at `water-rs/lints` dev head.

Each draft: title / observed (file:line at the pin) / expected / minimal repro /
owning repo.

---

## Draft 1 — `water` CLI: framework compat-check failure names no remediation

**Repo:** water-rs/cli

**Observed:** `water mcp` (and `water run`) on a project whose Water.toml/Water.lock
don't match the resolved framework graph prints `project does not resolve its
selected framework revision` — neither the resolved revision nor a command to
repair is named. The only discoverable fix was re-running `water channel dev`.

**Expected:** the error should print the resolved revision(s) it compared
against and the command that reconciles (`water channel dev` / `water channel
stable`), matching how `prepare_build` reports lock divergence.

**Repro:** commit a Water.lock whose package rev differs from dev tip, run
`water mcp --path <proj>` — observe the message.

---

## Draft 2 — `water` CLI: no way to pin a non-HEAD framework revision

**Repo:** water-rs/cli

**Observed:** `water channel <name>` only resolves channel tips. There is no
`water channel dev@<sha>` / `water pin <repo> <sha>` form — pinning the
framework to an arbitrary commit (bisecting, reproducing a report on an older
rev) requires hand-editing Water.toml `framework` + `package` tables and
re-merging Water.lock, which is exactly what the tool is meant to own.

**Expected:** a CLI spelling for an explicit revision pin that produces the
same Water.toml/Water.lock edits `water channel dev` does.

**Repro:** `water channel dev@225259c8` → unknown syntax; the only path is
manual file edits.

---

## Draft 3 — `water` CLI: `water mcp` child panic reported as "did not answer initialize"

**Repo:** water-rs/cli

**Observed:** when the app binary panics during startup (here: no GPU on the
host), `water mcp` reports only `the app binary did not answer \`initialize\`:
Connection closed`. The child's stderr (which contains the real panic) reaches
the parent process's stderr but is never quoted in the error, so the MCP client
sees a transport failure with no cause.

**Expected:** the initialize failure should carry the tail of the child's
stderr (bounded, e.g. last ~4KB) in the tool error.

**Repro:** `water mcp --path <proj>` on a host where the app binary panics at
launch (e.g. no wgpu adapter).

---

## Draft 4 — `water mcp`: no tool to inject an OS drag-and-drop

**Repo:** water-rs/cli

**Observed:** the MCP tool surface (`act`, `advance`, `find`, `key`,
`pointer`, `preview`, `restart`, `screenshot`, `snapshot`, `type_text`,
`wait`) can inject keys and pointer moves but cannot synthesize an OS-level
file drop — so `drop_destination` / `FileHovered`/`FileDropped` paths are
undrivable through MCP. I had to drive a real Xdnd client (python-xlib source
window: `XdndEnter` → `XdndPosition`(+`XdndStatus`) → `SelectionNotify` →
`XdndDrop`+`XdndFinished`) against winit's X11 protocol on Xvfb to verify the
app's drop handling end-to-end.

**Expected:** a tool like `drop { paths: [...] }` (or `pointer` with a drag
payload) that performs a real OS DnD, since synthetic app-level events skip
the platform negotiation entirely.

**Repro:** try to file-drop onto a view through any MCP tool — no path exists.

---

## Draft 5 — waterui/hydrolysis: no close-veto hook on `Window`

**Repo:** water-rs/waterui

**Observed:** `InputEvent::CloseRequested` sets `should_close = true`
unconditionally (hydrolysis `src/runner/window.rs:1428-1433` at 06f0b2f);
waterui `Window` (225259c8/9dfe6be) has `closable` but no
`on_close_requested`/cancellable path. A terminal's `confirm-close` can only
interpose inside the tab model, not on the OS-level close of a window with
running processes — the window is already gone by the time app code could
veto.

**Expected:** a reactive veto — e.g. `Window::on_close_requested(impl
Fn() -> bool)` consulted by the runner before writing `Closed`, or a
`CloseRequested` event the app may consume.

**Repro:** run a process in a pane, click the window manager's close button —
the window closes regardless of any app-side confirmation state.

---

## Draft 6 — waterui: `WindowHandle`/`Window` surfaces cannot write decorations/background/icon/level post-mount

**Repo:** water-rs/waterui

**Observed:** at 9dfe6be `WindowHandle` (`src/runtime/window.rs:745`) exposes
`close`/`minimize`/`maximize`/`fullscreen`/`restore`/`request_attention`/
`cancel_attention`/`set_frame`. There is still no write path for decorations,
background-material/blur, icon, or vsync, so `window-decoration` toggles at
runtime, `background-opacity` toggles, `window-icon`, and `window-vsync` stay
framework gaps. (`level` became a `Window` builder attribute this pin — via
`Window::level(impl IntoComputed<WindowLevel>)` — which covers
always-on-top because the runner diffs it every pump.)

**Expected:** reactive `Window` attributes for decorations/icon (and a
documented note that `background` is evaluated live) so the remaining
window-chrome toggles are implementable.

**Repro:** grep the `Window`/`WindowHandle` API for any `decoration`/`icon`
setter — none exists.

---

## Draft 7 — hydrolysis: `PresentMode::AutoVsync` hardcoded in surface configure

**Repo:** water-rs/hydrolysis

**Observed:** `src/platform.rs:1763` at 06f0b2f configures the wgpu surface
with `wgpu::PresentMode::AutoVsync` unconditionally; `window-vsync = false`
(uncapped present) is unimplementable because no `Window` field reaches the
present mode. (Carries the earlier feedback entry → water-rs/waterui#1300.)

**Expected:** a `Window` `present_mode`/vsync attribute (or env-level app
default) the runner applies when creating/reconfiguring the surface.

**Repro:** `window-vsync = false` in config — no code path can honor it;
frame pacing stays vsync-locked.

---

## Draft 8 — waterui: `conditional_window` presenter dies with its host window

**Repo:** water-rs/waterui

**Observed:** `conditional_window(&presentation, creator)` mounts the window
through the *host window's* view tree. When that host is destroyed, the
presentation node is gone: flipping the `WindowState` binding back to
`Normal` mounts nothing and nothing reports the silent loss. An app-level
presenter (a global drop-down terminal, Ghostty `quick-terminal`) therefore
cannot resurrect once the last window closed — even though the event loop is
alive under `LastWindowPolicy::StayResident` and `Window::show(env)` still
works. I had to detect host death app-side (`window_state == Closed`) and
switch to `Window::show(env)`.

**Expected:** either document in `conditional_window`/`Window::show` docs that
app-level presenters must mount through `WindowManager` (`Window::show(env)`),
or provide an `App::`-level presentation API that outlives individual windows.

**Repro:** two windows both embedding `conditional_window`; close the one that
armed the presentation; set the shared state to `Normal` — nothing mounts.
(Alternatively: single-window app, `StayResident`, close the window, flip the
state — same result.)

---

## Draft 9 — `water` CLI: the generated Hydrolysis entry ignores the app's style

**Repo:** water-rs/cli

**Observed:** `src/templates/hydrolysis/src/main.rs.tpl:69` renders
`hydrolysis::run(app, hydrolysis_m3::Material3::defaults())` — the style is a
hardcoded default with no hook for the app to supply its own. Worse, the file
is CLI-managed: `HydrolysisBackend::requires_regeneration`
(`src/hydrolysis/backend.rs:58-77`) re-renders every managed output whenever
any file diverges from the template, so a hand-edit restoring the app's style
is silently clobbered on the next `water channel`/`water run`/`water mcp`.
Observed on hydroterm: `window-theme` seeds its `Material3` style from the
terminal palette via `hydroterm::material_style()`; after a `water channel
dev` repin regenerated the backend, the generated entry ran the default M3
style — the config still parses but the app's chrome theme under `water
run`/`water mcp` is silently not the app's.

**Expected:** the style should reach the backend the same way the app does.
Two shapes that fit the CLI's own rules: (a) make the style an
environment-installed plugin — the app installs it inside
`configure_environment!`/`app(env)` and `hydrolysis::run` reads a
default-style slot from the env, taking no style argument at all; or (b) a
manifest-declared style (`Water.toml`, e.g. `[backend.hydrolysis] style =
"crate::material_style"`) that the template renders into the generated
entry — a manifest fact, not source scraping, per the CLI's "never recover
semantics from user source code" rule.

**Repro:** give a WaterUI app a custom `hydrolysis::Style`, wire it into the
managed `hydrolysis/src/main.rs`, run `water channel dev` (or any command
that triggers regeneration) — the generated entry reverts to
`Material3::defaults()`.

---

## Lint/dogfooding notes (not issues — the lints worked)

`cargo dylint --all` at lints dev head flagged 5 real defects in this round's
new code and all were fixed in place: `binding(None)` → `Binding::default()`,
three `waterui::window::UserAttention::Informational` paths that should import
unqualified, and `quick_window` taking `Binding<T>` by value → `&Binding<T>`.
No false positives and no confusing diagnostics this round.

## App-side patterns that no lint caught (possible new lints)

- Dispatching onto per-window state after the window closed
  (`focused_session().pending_actions.push(..)` on a dead surface) — nothing
  flags it; a "signals/bindings of a Closed window" diagnostic is probably too
  app-specific to lint, but `window_state.snapshot() == WindowState::Closed`
  gating is the shape the fix took.
- `Binding::container(WindowState::Closed)` + `Rc<RefCell<Option<Binding<Rect>>>>`
  tuple for cross-window shared bindings was clippy's `type_complexity` only —
  fine.
