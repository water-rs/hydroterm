# SKILL_FEEDBACK.md

What `water-rs/waterui`'s `.claude/skills/waterui/` skill and `water mcp`
made easy or failed to cover while dogfooding hydroterm. Each entry
records: what I tried, what the skill said (or didn't), what was actually
true (framework `file:line`), and the concrete edit that would have saved
the trip. Skill installed at `~/.claude/skills/waterui/` from pin
`0340571`.

## Install / distribution

1. **No `water` CLI command installs the skill.** `water --help`
   (0.4.3 @ 4c71570b) has `create init backend run bench build package
   doctor device fetch preview inspector mcp update completions` —
   nothing that installs `.claude/skills/waterui/` into an agent
   environment. I copied the directory to `~/.claude/skills/waterui/`
   by hand. An app author's agent will hit the same wall.
   Fix: a `water skill install [--global]` (or a documented snippet in
   `water init` output), or at minimum a line in SKILL.md/forge docs
   saying "copy `.claude/skills/waterui/` into `~/.claude/skills/`".

## `water mcp`

2. **`water mcp` exits the moment stdin closes** — scripting it with a
   here-doc or closed pipe kills the session before it can answer. Hold
   the pipe open (a python `subprocess` wrapper works). `references/mcp.md`
   documents the 10 tools accurately (`snapshot`, `find`, `act`,
   `pointer`, `key`, `type_text`, `wait`, `screenshot`, `advance`,
   `restart`, `preview`) but says nothing about the stdio lifecycle.
   Fix: one line in `references/mcp.md` — "the server exits on stdin EOF;
   keep stdin open between calls".

3. **`water mcp` hides the app's panic behind "app binary did not answer
   initialize".** On a GPU-less host the launched app panics on adapter
   init; `water mcp` reports only that initialize went unanswered. With
   `WATER_HYDROLYSIS_FORCE_FALLBACK_ADAPTER=1` exported the same binary
   answers and serves the tree/screenshot fine. The panic text never
   surfaces. Fix: forward the child process's stderr (first N lines) into
   the error, and mention `WATER_HYDROLYSIS_FORCE_FALLBACK_ADAPTER` for
   headless machines in `references/mcp.md` / `troubleshooting.md`.

4. **`water mcp` worked first try for real inspection** — the r45 tooltip
   defect (WATERUI_FEEDBACK #54) was found by hovering the tab `+` via
   `pointer` + reading the a11y bounds (`y=-10`) + `screenshot`. No
   feedback; it did what it should.

## Documentation gaps hit this round (r45)

5. **Tooltips are absent from the skill entirely.** `components.md` lists
   controls but there is no `Tooltip`/`plain_tooltip`/`rich_tooltip`
   section, and SKILL.md's component table doesn't mention it. The real
   API is `hydrolysis_m3::plain_tooltip("…").for_target(target)` →
   `TooltipAnchor` (hydrolysis-m3 `tooltip.rs:270`, re-exported at
   `lib.rs:145`), and its placement never flips/clamps — a defect I'd have
   expected a documented component to warn about. Fix: a
   `references/components.md` section covering `plain_tooltip`,
   `rich_tooltip`, `.for_target`, `persistent`, and the known edge-clip
   limitation.

6. **Pinning a content-sized child inside a `zstack` — skill gap, not a
   framework defect (root-caused r46).** `references/components.md:146-162`
   covers `absolute` + `position_in`/`position_in_offset`/`UnitPoint`
   accurately, and nothing in the skill explains *why* a
   `vstack().alignment(HorizontalAlignment::*)` child inside a `zstack`
   has no effect. Verified against source: it is **documented, intended
   behaviour** — `ZStackLayout::place` (waterui
   `src/layout/zstack.rs`, `place`/`size_that_fits` docstring: "Each
   child is sized independently, and the container's final width/height
   are the maxima of the children's reported sizes") sizes each child
   independently and positions it on the stack's shared alignment line;
   a content-sized `vstack` shrinks to its content, so its own
   `.alignment` has no slack to distribute — identical to SwiftUI. The
   correct primitive for pinning inside a `zstack` is `absolute(...)`
   + `position_in(UnitPoint)`/`position_in_offset`. Fix: one sentence in
   components.md's `zstack` entry — "children are sized independently;
   to pin a content-sized child inside a `zstack`, use
   `absolute(...)`/`position_in(UnitPoint)` — stack `.alignment` only
   distributes slack a child actually takes".

## Seeded from WATERUI_FEEDBACK — things an app author needs that the
## skill never mentions

7. **`TextField` has no `on_key`/`on_submit` and a focused field eats
   unconsumed keys** (WATERUI_FEEDBACK #50 → waterui#1265). Overlay bars
   (search/palette/title-prompt) need `.hittable(false)` + `.on_tap`
   refocus handlers or Escape/arrows die in the field. `field(...)`'s
   documented options (`prompt`, `keyboard`, `value_binding`,
   `line_limit`) do not include any key hook. Fix: document the
   limitation and the pattern in `references/interaction.md` (or the
   TextField section) until #1265 lands.

8. **`DragData` only carries `Text`/`Url`** — an app-internal drag id has
   to ride as a prefixed string and every `drop_destination` must filter
   it (WATERUI_FEEDBACK #48 → waterui#1254). The skill's drag/drop section
   documents Text/Url but not that there is no typed-payload channel.
   Fix: one line noting payloads are strings only and showing the
   prefix-filter pattern, until typed payloads land.

9. **`when(cond, view)` + shared binding ghost frame** (WATERUI_FEEDBACK
   #36): a `when`-mounted subtree that binds a `Binding<T>` written the
   same frame can render one stale frame. App authors hitting badge/label
   flicker will want this. Fix: troubleshooting.md entry on same-frame
   mount+binding-write ordering (keep if already documented under
   reactivity timing — verify at seed time: not present).

10. **Window states** — `WindowState` is `{Normal, Closed, Minimized,
    Fullscreen}` only; no `Maximized`, no always-on-top, no
    `request_user_attention`, no resize-increments (WATERUI_FEEDBACK
    #49→waterui#1264, #51→waterui#1268). `conditional_window` mounts on
    `!= Closed`. Fix: a `references/` note listing the window-state enum
    verbatim and what's absent, so authors stop searching.

11. **Runtime scale-factor changes do not fire on X11** (winit defect,
    WATERUI_FEEDBACK #52 → hydrolysis#230): an app seeing a stale scale
    after `xrdb -merge Xft.dpi` is hitting dead code in winit, not its
    own handler. Fix: one line in troubleshooting.md.

12. **`.context_menu` claims the secondary press itself** — a surface
    wrapped in `.context_menu` receives `set_keyboard_focus` +
    `pointer_move` on secondary-down, but NOT `pointer_button`
    (hydrolysis `hit_test.rs:851-858`: "the menu's actions act on the
    focused surface, so the button itself is not delivered"). An app that
    wants per-click menu rows (e.g. enable Copy only when the pointer is
    over a link) cannot snapshot at press time — it must keep menu state
    refreshed on pointer move and gate item construction on it. The
    skill's menus/context section documents `.context_menu` but not this
    claim order. Fix: one line in the menus reference + the interaction
    page noting Secondary never reaches the view under a context menu.

13. **Menu `Command` options discovered only in source** —
    `.subtitle()`, `.disabled(impl IntoComputed<bool>)`,
    `.shortcut(Shortcut)` exist on `MenuItem::Command`
    (waterui `menu.rs:200-270`) but no reference page lists them;
    `references/components.md` has no context-menu/menu section at all.
    Also framework-side: `.shortcut` is dropped by the hydrolysis popup
    renderer and `.disabled` has no visual treatment (WATERUI_FEEDBACK
    #58/#59). Fix: add a "Menus and context menus" section covering
    `MenuItem::{Command,Divider,Menu}`, per-item modifiers, and the
    Computed-list pattern.

14. **`water run` framework compat check is opaque about which manifest
    is stale** — after repinning git revs in the app's Cargo.toml, `water
    run` failed with "the project does not resolve its selected framework
    revision". The actual fix: update `revision` AND every
    `framework.packages.*` `rev` in Water.toml AND the duplicated `rev`s
    in the generated `hydrolysis/Cargo.toml` scaffold — three places,
    error names none. The skill's build/run docs don't mention that the
    scaffold manifest embeds a second copy of the pins. Fix: document in
    the `water run`/framework-upgrade reference that a git-rev bump must
    be mirrored into Water.toml and `<backend>/Cargo.toml`, or make the
    CLI name the mismatched package.

## Verified accurate (no change needed)

- `absolute` / `position_in` / `position_in_offset` / `UnitPoint`
  constants — `references/components.md:146-162` is correct and complete.
- The 10 `water mcp` tools — `references/mcp.md` matches the shipped
  server.
- `when(cond, fn)`, `vstack`/`hstack`/`zstack` basics — correct.

## Documentation gaps hit this round (r47)

7. **`.state(&x)` ordering relative to event handlers is undocumented and
   silently wrong.** Nothing in the skill (or anywhere) explains that a
   `State`-injecting modifier only extends the environment of the node's
   *descendants* — `v.state(&b).on_hover_exit(h)` compiles, then panics at
   runtime on the first event ("not found at position 0" via
   `apply_on_event` capturing `env` before the injection, hydrolysis
   `metadata.rs:855` → waterui `handler.rs:31` `extract_or_panic`). The
   natural reading — set the state, then attach the handler — is the
   broken one. Correct form: `.on_hover_exit(h).state(&b)` (state
   outermost). Filed as WATERUI_FEEDBACK #61. Fix: a short
   "environment injection ordering" paragraph in the skill wherever
   `.state`/`State<T>` handlers are introduced — "`.state` must wrap the
   modifiers whose handlers extract it; handlers attached on the same
   node *after* `.state` see an env without it".

## Lint candidates

Patterns from this repo where a water-rs/lints rule would have caught a
real mistake, or where an existing lint misfired. Maintained per
water-rs/waterui#1269; reported upstream via the maintainer, never
patched in-repo.

- *(r40)* `qualified_waterui_path` fired correctly on `use` items
  importing `waterui::…` in code that already had a `waterui::` path in
  scope — real cleanup, zero false positives so far.
- *(r45)* Candidate: **per-instance `Binding` shadowed across a window
  boundary.** The quick-terminal autohide bug (this round) was a child
  `AppState` owning its own `quick_state` `Binding` while the host's
  `conditional_window` watched the host's copy — a field that *looks*
  shared but isn't. Not obviously lintable (it's a design-level aliasing
  mistake, not a syntactic pattern); recorded so the maintainer can
  judge. No mechanical rule proposed.
- *(r45)* No new `#[expect]` needed; `cargo dylint --all` at lints dev
  head `8f74a56ad126` is zero-warning on this code.
- *(r46)* `if_else_view` misfired again on a non-View if/else — this time
  arms producing `Color` inside `zip(a,h).map(...)` for a hover-state
  layer (`app.rs:2734`). Same class as the earlier `Str`-producing
  false positive: the lint should check that both arms resolve to
  `impl View` before firing. Worked around by arithmetic
  (`with_opacity(0.08 * f32::from(h && !a))`) rather than `#[expect]` —
  the rewrite is arguably cleaner, so: false positive recorded, no
  expect needed.
- *(r46)* `handler_captures_binding` ×2 and `needless_computed` ×1 were
  real positives on the tab-chip hover handlers — fixed per suggestion:
  `.state(&hovered)` + `State(h): State<Binding<bool>>` params, and
  `signal_color(hover_bg)` without `.computed()`.

- *(r47)* Candidate: **`RefCell<Vec<T>>` (or any non-`Rc`-shared
  `RefCell`/`Cell`) field inside a `#[derive(Clone)]` struct silently
  forks state.** `AppState` derives `Clone` and is cloned into every
  view/handler; `closed_stack: RefCell<Vec<ClosedTab>>` meant "the" undo
  stack but each clone owned a deep-copied `Vec` — `capture_closed`
  pushed into clone A while `undo_close` popped clone B, so undo was a
  silent no-op. The correct field is `Rc<RefCell<Vec<ClosedTab>>>`
  (already the convention for `sessions`). A lint could flag
  `RefCell<Vec<_>>`/`RefCell<HashMap<_,_>>` fields in structs deriving
  `Clone` where sibling fields use `Rc<RefCell<_>>` — the pattern is
  almost certainly a missed sharing intent.

- *(r47)* `Water.lock` + `lock_sha256` semantics, learned by reading
  cli-src `src/project_model/framework.rs`: `Water.lock` must be a
  byte-copy of the waterui repo's `Cargo.lock` at the pinned revision
  (from `~/.cargo/git/checkouts/waterui-*/<rev>/Cargo.lock`), and
  `Water.toml`'s `lock_sha256` is the sha256 of those remote bytes.
  `validate_dependencies` walks the resolved graph: every
  framework-sourced or ecosystem-named package must appear in the
  `allowed` set (Water.lock entries remapped through `[framework.
  patches]` revs — which must equal the framework lock's own git
  sources) or have a `sanctioned_source` — a `[framework.packages.X]`
  `git+rev` or `=version` pin matching OUR resolved version, not the
  framework's. Practical consequence: `hydrolysis`/`hydrolysis-m3` must
  be pinned to the framework manifest's submodule gitlinks (e.g.
  `926424bc`/`631ba9d6`), NOT the repos' standalone dev heads, or the
  compat check fails on transitive-version mismatches. `water build
  --platform linux` targets gtk4; the hydrolysis path is `water run`
  with no `--platform` flag.

### Reactive font family on text (`window-title-font-family`, r50) → water-rs/waterui#1303
- **Tried:** apply a per-config font family to tab-chip labels so a
  config reload changes them live.
- **Skill said:** `.font(...)` and `font::Body.family(name)` are
  documented — but `family(impl Into<Str> + Clone + 'static)` bakes the
  name in at construction; nothing in the skill covers a *reactive*
  family.
- **Actually true:** `Font::new` takes any `impl Resolvable<Resolved =
  ResolvedFont>` (`waterui components/foundation/text/src/font.rs:187`).
  Font slots resolve via `Resolvable::resolve(env)` → `Signal`; combine
  with `nami::zip` + `.map` to rewrite `ResolvedFont.family` from a
  `Binding<Option<Str>>` — hydroterm's `TitleFont` in `src/app.rs` is
  the working shape (a `Binding` is already a `Signal`; `zip` accepts
  mixed signal types and `.computed()` is needless — `needless_computed`
  flags it).
- **Concrete edit:** a `references/` section "Reactive fonts" — show
  `Font::new(custom Resolvable)` + the zip-with-binding pattern; state
  plainly that `Font::family`/slot `.family()` are static-only.

### Showing a snackbar from non-handler code (`app-notifications`, r51)
- **Tried:** `SnackbarManager::show()` from a copy handler that lives in
  non-view code (`surface.rs` — not a `.action` closure).
- **Skill said:** `references/components.md` documents "Every `Window`
  installs a `SnackbarManager` through `.state()`" and shows
  `.action(|m: SnackbarManager| …)` injection — but only inside view
  action callbacks. Nothing covers reaching the manager from code that
  is not a view handler.
- **Actually true:** the manager is env-scoped to the whole window
  (`runtime/window.rs:225` installs it via `.state(&snackbar_manager)`),
  so any mounted view can capture it once through `.on_appear(|m:
  SnackbarManager, …|)` and stash it — hydroterm keeps it in
  `Session.snackbar: RefCell<Option<SnackbarManager>>` behind an
  always-mounted `Spacer::new(0.0)` (`app.rs:2678`). Only mounting it
  inside a conditional prompt overlay means the capture silently never
  ran when no prompt was up — toasts were a no-op.
- **Concrete edit:** components.md snackbar section — add a
  "outside `.action` handlers" snippet: `Spacer::new(0).on_appear(|m:
  SnackbarManager| stash(m))`, and warn that conditional overlays only
  capture while mounted.

## Lint candidates

- `qualified_waterui_path` (new at lints `a9058391`): real positive —
  flagged my `waterui::snackbar::Snackbar::new(..)` written inline;
  fixed with `use waterui::snackbar::Snackbar;`. Not a false positive.
- `unused_spawn_handle` (r53): `spawn_local(...)` / `spawn(...)`
  discarded (statement position or `let _ =`) → warn. async-task's
  `Task` cancels on drop, so the spawned future never executes —
  silent dead code. Correct: `.detach()` for fire-and-forget, or keep
  the handle for cancellation. Real positive found in-tree:
  hydroterm's `animate_frame` animation task (see entry above).
- `refcell_borrow_in_if_let_scrutinee` (r55): `if let X = cell.borrow()
  { ... cell.borrow_mut() ... }` — the temporary borrow lives for the
  whole `if let` block (it's part of the scrutinee expression), so the
  borrow_mut inside panics at runtime. Real positive hit live:
  surface.rs one-shot key-table retire crashed with `RefCell already
  borrowed` on first invoke. Fix shape: bind the value in a `let`
  before the `if let` so the borrow drops at the semicolon. Worth a
  lints-crate rule: borrow_mut/borrow in the body of an `if let` whose
  scrutinee borrows the same cell.
- Existing entries unchanged.

- **Physical-key (`physical:`) keybinds need winit `Code`, and it is already plumbed.** Tried: binding `physical:` triggers. Skill said nothing about physical vs logical keys; `keyboard-types` `Key` is the layout-translated character only, and on a non-QWERTY layout a char-based bind silently binds the wrong position. Actually true: hydrolysis forwards winit `physical_key` as `physical_code` on every key event (`src/platform.rs` `from_winit_code`/`logical:`-`code:` pair), and `keyboard_types::Code` has `FromStr` for the CamelCase names (`KeyA`, `Digit0`, `ArrowUp`, `F1`) — so position-based matching is a `Code` compare, no keymap work needed. Skill edit: one line in the input/keys reference — "key events carry both `key` (logical) and `physical_code` (position); use the latter for layout-independent binds."

### `spawn_local` returns a cancel-on-drop handle — fire-and-forget needs `.detach()` (r53)
- **Tried:** `spawn_local(async move { …animation loop… })` as a bare
  statement for the quick-terminal slide.
- **Skill said:** the task reference shows `spawn_local` for background
  work but does not state the handle's drop semantics.
- **Actually true:** `waterui::task::spawn_local` returns
  `executor_core`'s `AsyncTask` (async-task 4.x semantics) — **dropping
  the handle cancels the task**: the runnable is scheduled once, then
  run() sees the cancelled flag and drops the future WITHOUT polling it
  — no code inside the async block ever runs, and no error is logged.
  Hydroterm's animation task was silently dead for rounds; the only
  symptom was "the animation never moved and the end-state write never
  fired". `sleep`/`spawn_local` themselves work fine — long-running
  loops must `.detach()` (or store the handle), as the codebase already
  does for the hotkey drains.
- **Concrete edit:** task reference — one line: "`spawn_local`'s return
  is cancel-on-drop; a bare-statement spawn never polls. Call
  `.detach()` or store the handle."
- **Lint candidate:** `unused_spawn_handle` — recorded under "Lint
  candidates" above.

### Committed text bypasses `on_key` — input gates need both paths (r54)
- **Tried:** gating KAM (ANSI mode 2 keyboard lock) on `on_key`'s return.
- **Skill said:** nothing — the input reference documents `on_key` only.
- **Actually true:** text that arrives through IME/XIM commit —
  `SurfaceInputEvent::CompositionCommit` — is delivered to
  `Surface::on_text` (hydrolysis `src/runner/window.rs`), NOT `on_key`.
  `xdotool type` goes through the same path, so a key-only gate is
  trivially bypassed even without a real IME. Any handler that must
  drop input (KAM, a grab, a modal prompt) belongs in both `on_key`
  (raw keys) and `on_text` (committed strings).
- **Concrete edit:** input/keys reference — one line: "`on_key` sees
  key presses only; IME/commit text reaches `on_text`. Gate both when
  suppressing input."

### `water run` compat model — Water.lock must be the resolved graph (r54)
- **Tried:** repinning hydrolysis/nami/hydrolysis-m3 ahead of the
  framework manifest's gitlinks while Water.lock stayed a byte-copy of
  waterui's lock.
- **Skill said:** (r47 entry) Water.lock = byte-copy of waterui's
  Cargo.lock — true only while pins equal the gitlinks.
- **Actually true:** `prepare_build` (cli
  `src/project_model/framework.rs:767`) merges three locks into the
  scaffold's Cargo.lock keyed by (name,version,source): the previous
  scaffold lock + `cargo_lock(Water.lock)` (no-source packages are
  rewritten to `git+<repo>?rev=<framework rev>`) + the app Cargo.lock.
  A version the canonical lock and the resolved graph disagree on
  (e.g. accesskit 0.25.0 vs 0.25.1) enters the seed twice and cargo
  metadata cannot unify the edges — hard fail. Then
  `validate_dependencies` walks the scaffold graph and fails any
  ecosystem package whose (name,version,source) ∉ lock ∪
  `[framework.packages]` sanctions; `[framework.patches]` does NOT
  sanction.
- **Working recipe for ahead-of-gitlink pins:** pin every rev in
  `[framework.packages.<name>] git+rev`; resolve the app
  (`cargo metadata`) and the scaffold (`cargo metadata` in
  `hydrolysis/`) once; set Water.lock = union of both resolved locks;
  refresh `[framework].lock_sha256 = sha256(Water.lock)`. Then the
  merge is consistent and validation passes trivially.
- **Concrete edit:** the Water.toml section — document the three-input
  merge and that Water.lock is the certified resolution of the WHOLE
  managed build (app + scaffold extras like waterui-mcp), not a copy of
  upstream's lock.

- **r55 cli behavior:** `water build`/`water run` regenerates
  `hydrolysis/src/main.rs` from the scaffold template on every run —
  local edits to that file (e.g. the `material_style()` call the
  template drops) are overwritten silently. Keep the restored version
  on disk and re-check it after any `water` invocation; do not rely on
  it persisting.

- **r55 update — byte-copy stays retired even after the upstream fix.**
  Lexo asked whether water-rs/cli dev (`073a457` "seed from one
  resolution" + `b93cd92`, plus our regression test at `34aa4fb`) lets
  Water.lock return to the r47 byte-copy recipe. Verified empirically
  (pristine clone at `2c141d5`, byte-copied waterui@ee85dc4 lock,
  `water build --platform linux` with a `cargo install`ed fixed cli):
  the seed now resolves — canonical lock owns every name it records —
  but `validate_dependencies` still fails the build: any resolved
  package that *replaces* a canonical identity within a compatible
  range (`replaces_locked_package`: same name+source, resolved version
  satisfies `^locked`) is a conflict. accesskit 0.25.1 vs canonical
  0.25.0 is exactly that, plus ~30 more (objc2 family, wasm-bindgen
  set, zerocopy ×2…). Plain byte-copy would need a
  `[framework.packages.<name>] = "=<our version>"` sanction per
  divergent package — strictly worse than the resolved-graph recipe,
  which IS the sanctioned resolution. Verdict: **keep
  resolved-graph Water.lock**; byte-copy only works when every pin
  matches the framework's gitlinks AND the resolved graph adds
  nothing the canonical lock doesn't already name.
