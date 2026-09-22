# WATERUI_FEEDBACK — hydroterm dogfooding notes

Real gaps/wishes found while building a Ghostty-class terminal on hydrolysis. Each item notes severity for a terminal workload.

## Findings

1. **Keyboard focus is click-only.** A `GpuSurface` with `wants_input_events()` only receives `Focus(true)` after a pointer press. A terminal must accept keyboard input at app launch / when a tab becomes active without requiring a click. Wish: a way to request focus programmatically (e.g. `GpuView::request_focus()` or a `.focused(&binding)` view modifier), or auto-focus of the first input surface. (Critical)

2. **No cursor-shape control from a GpuView.** A terminal wants I-beam over text, arrow over padding, pointer over a Ctrl-hovered OSC8 link. `wants_input_events` views can't communicate the cursor to the host. Wish: `GpuView::cursor_style() -> Option<CursorStyle>` or a cursor set through the env/frame. (Nice-to-have)

3. **`Tabs` requires ≥1 tab and has no dynamic collection API.** `hydrolysis::TabsRenderState::from_tabs` asserts non-empty, and `Tabs::new(selection, Vec<Tab>)` is a fixed set — adding/closing tabs means rebuilding the whole `Tabs` view (all native tab state rebuilt, tab contents' GpuSurfaces re-set-up). Wish: `Tabs` over a reactive collection (`SignalCollection`/`ReactiveList`), preserving per-tab state. (Important for a tabbed terminal)

4. **No way to intercept a window close request / confirm "process still running".** Terminals warn before closing a live shell. Wish: `Window` close-request callback. (Nice-to-have)

5. **`GpuView::input` doesn't expose double/triple click semantics.** Word/line selection needs click-count info (like winit's `click_count`). `PointerButton` gives no count; must be measured in the view by timing. (Nice-to-have)

6. **IME `CompositionUpdate` works but the view cannot show preedit outside its own pixels** — that's fine for terminals (we draw preedit at the caret ourselves). Noting for completeness; `ime_caret()` positions the candidate window correctly. (OK)

7. **`GpuFrame` has no vsync/presentation timestamp or resize event distinct from render.** Terminal needs "surface resized" to recompute cols/rows — currently inferred by comparing `frame.width/height` each `render()`, which works but runs at frame granularity rather than event granularity. (Minor)

8. **Window `fullscreen()` exists on `WindowHandle`, but `WindowStyle`/`WindowBackground::Color` + alpha can't produce a transparent window on hydrolysis (winit + vello).** Ghostty's translucent background isn't possible through `WindowBackground`; unclear if a GpuSurface alpha channel is ever honored on the swapchain. (Nice-to-have)

9. **No scroll "momentum"/natural scrolling direction info beyond `Scroll` deltas** — deltas arrive but no phase/momentum data beyond `finished`. Fine in practice. (Minor)

10. **`GpuView` cannot emit a11y tree updates on its own cadence** — `accessibility_label/value` are sampled by the backend; a terminal's screen content changes at PTY pace, sampled at frame pace — acceptable. (OK)

11. **winit runner: no command-line args/env plumbing for the app** — the App root can't easily get argv for `-e command`. We use `std::env::args` directly — fine, just noting. (Minor)

12. **Pasting via IME/TextInput**: `TextInput` covers typed text; clipboard paste must be implemented app-side (clipboard API via waterkit) — a `GpuView` `paste` hook or ClipboardRequest event would simplify bracketed-paste correctness. (Nice-to-have)
13. **`nami::Signal` is implemented for `Vec<T>`, shadowing `Vec::get(usize)`.** On a `Vec` (or `MutexGuard<Vec>`) the expression `v.get(i)` resolves to `Signal::get()` (0-arg), not slice `get`, so element access needs `v.as_slice().get(i)` — and `clippy::iter_nth` actively suggests the broken `.get(i)`. Wish: rename the signal method or scope it so `Vec`'s inherent API is unreachable only when intended. (Minor, but surprising)

14. **`nami::Binding` is not `Send + Sync`.** Fine for single-thread UI confinement, but it propagates: any struct holding a `Binding` becomes `!Send + !Sync`, tripping `clippy::arc_with_non_send_sync` on every `Arc` that wraps it (needs `#[allow]` + justification). If bindings are meant to be UI-thread-only this is correct — but a doc note, or a Send+Sync variant for genuinely cross-thread state, would help. (Minor)

15. **Focus confirmed in a second spot**: a newly created tab's `GpuSurface` gets no `Focus` event until clicked — so a freshly spawned shell can't be typed into until the user clicks. Same root cause as #1; reiterating because a tabbed app hits it on every new tab. (Critical — duplicates #1)
