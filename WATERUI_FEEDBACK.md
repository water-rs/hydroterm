# WATERUI_FEEDBACK — hydroterm dogfooding notes

Real gaps/wishes found while building a modern terminal on hydrolysis. Each item notes severity for a terminal workload. Filed upstream issues are referenced per item; entries whose number is missing were not filed (or are purely informational).

## Findings

1. **Keyboard focus for an input-receiving surface is click-only.** A surface with `wants_input_events()` receives `Focus(true)` only after a pointer press. A terminal must accept keyboard input at app launch, on a newly created tab, and after a subtree rebuild, without requiring a click — we observed all three: new surfaces get no `Focus` event, and a rebuild (split-pane tree swapping `Leaf` → `HStack{child, child}` inside `watch`) remounts every surface and drops focus with keystrokes. Wish: programmatic focus (`request_focus()`, a `.focused(&binding)` modifier, or a stable caller-provided key that survives remounts). → **water-rs/hydrolysis#90** (Critical)

2. **No cursor-shape control from an input-receiving view.** A terminal wants I-beam over text, arrow over padding, pointer over a Ctrl-hovered OSC8 link. Wish: a cursor-style getter on the content/view. → **water-rs/waterui#1193** (Nice-to-have)

3. **`Tabs` requires ≥1 tab and has no dynamic collection API.** `TabsRenderState::from_tabs` asserts non-empty; `Tabs::new(selection, Vec<Tab>)` is a fixed set — adding/closing tabs rebuilds the whole `Tabs` view (all native tab state, each tab's surface re-setup). Wish: `Tabs` over a reactive collection preserving per-tab state. → **water-rs/waterui#1194** (Important for a tabbed terminal)

4. **No way to intercept a window close request / confirm "process still running".** Terminals warn before closing a live shell. Wish: `Window` close-request callback. → **water-rs/waterui#1195** (Nice-to-have)

5. **Pointer events carry no click count.** Word/line selection needs double/triple-click info; `PointerButton` gives none — measured in the view by timing. → **water-rs/waterui#1196** (Nice-to-have)

6. **IME `CompositionUpdate` works; the view draws preedit inside its own pixels** — fine for terminals (we draw preedit at the caret ourselves). `ime_caret()` positions the candidate window correctly. (OK — informational)

7. **No resize event / presentation timestamp distinct from scene builds.** Terminal recomputes cols/rows from `build_scene`'s width/height each frame — works, at frame rather than event granularity. → **water-rs/waterui#1197** (Minor)

8. **Transparent windows aren't reachable on hydrolysis (winit + vello):** `WindowStyle`/`WindowBackground::Color` + alpha can't produce a translucent window. (Nice-to-have)

9. **No scroll "momentum"/natural-scroll phase data** — deltas arrive but no phase beyond `finished`. Fine in practice. → **water-rs/waterui#1198** (Minor)

10. **Scene content's a11y output is backend-sampled** — `accessibility_label/value` are read by the backend at frame pace, and on hydrolysis the GpuSurface-path emitted neither. A terminal's content changes at PTY pace; sampling is acceptable, non-emission was not. → **water-rs/hydrolysis#91** (Minor)

11. **winit runner: no command-line args/env plumbing for the app** — the App root can't easily get argv for `-e command`; `std::env::args` works. (Minor)

12. **Clipboard paste must be implemented app-side** — `TextInput` covers typed text; a paste event / `ClipboardRequest` on input-receiving surfaces would simplify bracketed-paste correctness. → **water-rs/waterui#1199** (Nice-to-have)

13. **`HStack<ForEach<...>>` is not itself a `View`.** `HStack::for_each(vec, …)` produces a specialization that doesn't satisfy `View`; the working path is `views.into_iter().collect::<HStack<(Vec<AnyView>,)>>()`, discoverable only in the impl block. Wish: make it return a `View`, or document the collect pattern. (Minor — workaround exists)

14. **No splitter/resizable-divider component.** Split panes distribute space equally via `HStack`/`VStack`; no draggable divider, per-child weight, or pointer-grab affordance — "resize a split" can't be built without a custom divider surface. Wish: `SplitPane`/`ResizableStack` over `Vec<(weight, View)>` with drag handles. (Important — every terminal, IDE, multiplexer needs it)

15. **hydrolysis winit runner spawns exactly one window** — `new_with_windows` accepts extras but only the main window is realized; no runtime "open another window" for Ctrl+Shift+N. Wish: `Window::show()` that spawns a winit window at runtime. (Important — multi-window is table stakes)

16. **No `Send`-able frame request for merged `SceneContent`.** `SceneInvalidator` is `Rc<dyn Fn()>` — correct for signal-driven content on the main thread, but data arriving on a background thread (our PTY parser) has no safe way to request a frame. Workaround that works today: an `async-channel` ping consumed by a `waterui::task::spawn_local` drain future that calls the invalidator on the main thread — relying on hydrolysis pumping the local executor through `PollLocalTasks`. Wish: a `Send + Sync` wake/`request_frame` handle handed to the content. (Important — every producer-thread scene hits this)

17. **`SceneContent::input` does not schedule a frame on delivery** — documented, but easy to miss: handlers that change visuals (selection, scroll, preedit) must call the invalidator themselves or the update waits for the next coincidental frame. We call it unconditionally at the end of `input()`. Worth a doc callout on `set_invalidator`. (Minor)

18. **parley/ICU4X logs `No segmentation model for complex script: Chinese/Japanese`** on every CJK run in the shared text stack (the packaged data set lacks the segmenter models). Glyphs still shape and render correctly — cell anchoring doesn't depend on line breaking — but each CJK frame writes error lines. (Minor)
