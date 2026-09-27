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

### Reactive font family on text (`window-title-font-family`, r50)
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

## Lint candidates

(no new entries this round)
