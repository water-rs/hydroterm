# WATERUI_FEEDBACK — hydroterm dogfooding notes

Real gaps/wishes found while building a modern terminal on hydrolysis. Each item notes severity for a terminal workload. Filed upstream issues are referenced per item; entries whose number is missing were not filed (or are purely informational).

Pins under test: waterui `c8a78fe8`, hydrolysis `12175f8c`.

## Findings

1. **Keyboard focus for an input-receiving surface is click-only.** A surface with `wants_input_events()` receives `Focus(true)` only after a pointer press. A terminal must accept keyboard input at app launch, on a newly created tab, and after a subtree rebuild, without requiring a click — we observed all three: new surfaces get no `Focus` event, and a rebuild (split-pane tree swapping `Leaf` → `HStack{child, child}` inside `watch`) remounts every surface and drops focus with keystrokes. Wish: programmatic focus (`request_focus()`, a `.focused(&binding)` modifier, or a stable caller-provided key that survives remounts). → **water-rs/hydrolysis#90** (Critical)

2. **No cursor-shape control from an input-receiving view.** A terminal wants I-beam over text, arrow over padding, pointer over a Ctrl-hovered OSC8 link. Wish: a cursor-style getter on the content/view. → **water-rs/waterui#1193** (Nice-to-have)

3. **`Tabs` requires ≥1 tab and has no dynamic collection API.** `TabsRenderState::from_tabs` asserts non-empty; `Tabs::new(selection, Vec<Tab>)` is a fixed set — adding/closing tabs rebuilds the whole `Tabs` view (all native tab state, each tab's surface re-setup). Wish: `Tabs` over a reactive collection preserving per-tab state. → **water-rs/waterui#1194** (Important for a tabbed terminal)

4. **No way to intercept a window close request / confirm "process still running".** Terminals warn before closing a live shell. Wish: `Window` close-request callback. → **water-rs/waterui#1195** (Nice-to-have)

5. **Pointer events carry no click count.** Word/line selection needs double/triple-click info; `PointerButton` gives none — measured in the view by timing. → **water-rs/waterui#1196** (Nice-to-have)

6. **IME `CompositionUpdate` works; the view draws preedit inside its own pixels** — fine for terminals (we draw preedit at the caret ourselves). `ime_caret()` positions the candidate window correctly. (OK — informational)

7. **No resize event / presentation timestamp distinct from scene builds.** Terminal recomputes cols/rows from `build_scene`'s width/height each frame — works, at frame rather than event granularity. → **water-rs/waterui#1197** (Minor)

8. **Transparent windows accept the flag but render no content.** → **water-rs/hydrolysis#96** `Window::background(Color..with_opacity(<1))` flips `window_requires_transparency` → winit `with_transparent(true)`, and the OS window does composite transparently — BUT the scene produces zero pixels: the whole window is see-through, with *no* view content at all (not even native `text!()`, so this is not a SceneView-specific path). Opaque windows render fine.

   Minimal repro (pins above, `hydrolysis` features `["winit"]`):
   ```rust
   let state = binding(WindowState::Normal);
   let window = Window::new("trans-repro", state, || {
       vstack((text!("TRANSPARENT WINDOW TEST"), text!("you should see this text")))
   })
   .background(Color::srgb(30, 30, 30).with_opacity(0.5));
   let app = App::new_with_windows([window], Environment::new());
   hydrolysis::run(app, hydrolysis_m3::Material3::defaults());
   ```
   Result on X11 + llvmpipe (`WATER_HYDROLYSIS_FORCE_FALLBACK_ADAPTER=1`): transparent window frame, desktop shows through, zero rendered content — looks like the surface/swapchain never composites. (Critical for the feature — transparency plumbing exists end-to-end but the renderer produces nothing. hydroterm wires `background-opacity` in config; disabled-by-default until this lands.)

9. **No scroll "momentum"/natural-scroll phase data** — deltas arrive but no phase beyond `finished`. Fine in practice. → **water-rs/waterui#1198** (Minor)

10. **Scene content's a11y output is backend-sampled** — `accessibility_label/value` are read by the backend at frame pace, and on hydrolysis the GpuSurface-path emitted neither. A terminal's content changes at PTY pace; sampling is acceptable, non-emission was not. → **water-rs/hydrolysis#91** (Minor)

11. **winit runner: no command-line args/env plumbing for the app** — the App root can't easily get argv for `-e command`; `std::env::args` works. (Minor)

12. **Clipboard paste must be implemented app-side** — `TextInput` covers typed text; a paste event / `ClipboardRequest` on input-receiving surfaces would simplify bracketed-paste correctness. → **water-rs/waterui#1199** (Nice-to-have)

13. **No splitter/resizable-divider component.** Split panes distribute space equally via `HStack`/`VStack`; no draggable divider, per-child weight, or pointer-grab affordance — "resize a split" can't be built without a custom divider surface. Wish: `SplitPane`/`ResizableStack` over `Vec<(weight, View)>` with drag handles. → **water-rs/waterui#1203** (Important — every terminal, IDE, multiplexer needs it)

14. **No `Send`-able frame request for merged `SceneContent`.** `SceneInvalidator` is `Rc<dyn Fn()>` — correct for signal-driven content on the main thread, but data arriving on a background thread (our PTY parser) has no safe way to request a frame. Workaround that works today: an `async-channel` ping consumed by a `waterui::task::spawn_local` drain future that calls the invalidator on the main thread — relying on hydrolysis pumping the local executor through `PollLocalTasks`. Wish: a `Send + Sync` wake/`request_frame` handle handed to the content. → **water-rs/waterui#1202** (Important — every producer-thread scene hits this)

15. **parley/ICU4X logs `No segmentation model for complex script: Chinese/Japanese`** on every CJK run in the shared text stack (the packaged data set lacks the segmenter models). Glyphs still shape and render correctly — cell anchoring doesn't depend on line breaking — but each CJK frame writes error lines. → **water-rs/waterui#1204** (Minor)

16. **`TextField` has no submit event.** Programmatic focus exists now — `ViewExt::focused(&binding, equals)` — and we tried it on the palette field. It doesn't help yet in practice: hydrolysis delivers key events to the *embedded* input surface before a focused text field, so the field never sees the typing, and when it is focused Enter is swallowed by the single-line edit model (no submit callback exists) instead of reaching our handler — so the surface still owns the keys and writes the field's binding manually. A real `on_submit` callback would unlock the field-owned design. → **water-rs/waterui#1205** (Nice-to-have)

17. **hydrolysis-m3 `picker` never draws its label.** → **water-rs/hydrolysis#97** `controls/picker.rs` builds no view from `config.label` (zero references) — `picker("Theme", items, &b)` shows only the value, so callers must compose their own label text and `.hide_label()`. (Minor)

18. **`.focused()` is TextField-only.** Hydrolysis's focused wiring asserts exactly one `TextField`/`SecureField` in the wrapped subtree — it cannot programmatically focus other views, so input-receiving surfaces (hydrolysis#90) still can't grab keyboard focus without a click. If `.focused` ever grows to cover arbitrary views, #1 is solved by the same API. (Informational — documents the scope)

   ~~Related symptom: rebuilding the tab content (a `watch` firing on the split tree) recreates the `SceneView` widgets and drops GUI focus~~ — RESOLVED as app misuse, not a framework gap: `watch` replaces its subtree by design (Principle 8), so rendering tabs via `watch(tab_ids)` was ours to fix. We now render the tab set as `ZStack::for_each`/`HStack::for_each` over a reactive `nami::collection::List<PaneTab>` (`#[derive(Identifiable)]`, stable `#[id]`), toggling each tab's `.visible(selected.equal_to(id))`. Adding/closing/switching tabs patches the collection in place — existing SceneViews stay mounted, and the active tab's caret + scrollback survive switches with no click required (verified live on 3 tabs).

19. **Key routing ignores visibility: a hidden but focused `SceneView` keeps receiving keys.** With the ForEach rework each tab's surface stays mounted under `.visible(false)` (opacity 0, `hittable(false)`). GUI focus, once acquired by pointer press, stays on that hidden surface — keystrokes typed after switching to another tab still route to the invisible, non-hittable one. Focus should follow the *hittable/visible* surface (or a surface should lose focus when it becomes unhittable). Same family as hydrolysis#90 — the real need is programmatic/blur focus for surfaces. Repro: two tabs, focus tab 1's surface, switch to tab 2 via `Ctrl+Tab`, type — text lands in tab 1's session. (Important — makes hidden-vs-visible semantics inconsistent for keyboard input)
