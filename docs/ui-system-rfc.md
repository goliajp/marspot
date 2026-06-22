# UI System RFC — P1+P2+P3

Status: in-progress on `feature/ui-system`
Branch policy: stay on branch, don't install, run sandboxed via `bin/run.sh`,
only `install-local` after all P1+P2+P3 done + tests green.

## Why this exists

`ViewPainter` is a thin shim over Metal pipeline-pushing.  Callers
choose between `fill_rect` (cells pipeline, no AA, hard edges)
and `fill_rounded_rect` (ui_rects pipeline, SDF AA) — a choice
that requires knowing:

1. Encode order:  cells pipeline draws BEFORE ui_rects in the
   overlay pass.  Picking `fill_rect` for a primitive that should
   sit ABOVE an SDF panel chrome silently puts it below.  This was
   the F3+12.x divider bug — 4 install cycles before root cause
   surfaced.
2. Pixel units:  all four args are physical pixels; callers
   `* scale` everywhere to get logical-pt semantics.  Easy to
   forget on retina vs non-retina.
3. Color is `[f32; 4]` raw RGBA, no hex, no semantic name, no alpha
   helper, no theme tokens.
4. No box model.  Every component computes (x, y, w, h) by hand.
   Padding, gap, centering, percentage-of-parent all open-coded.

User explicitly cited modern CSS:  predictable units (`1px = 1px`),
explicit alpha that always blends, percentage of parent, and an
abstraction that hides pipeline internals.

## Goals

1. **Caller code reads like CSS.**  `Pt(1.0)`, `Pct(0.5)`,
   `Color::rgba(255, 255, 255, 0.18)`, `Color::hex("#222")`.
2. **No exposed pipeline choice.**  One drawing API, internal
   dispatch.  Picking the wrong pipeline silently is impossible.
3. **Submission order is z-order.**  Drawing X then Y means Y is
   above X regardless of which pipeline each ends up on.
4. **Composable layout.**  `Stack`, `Pad`, `Sized` primitives so
   the ContextMenu / LayoutModal / ProcessPanel layout code drops
   ~60% of its hand-rolled math.
5. **No old/new coexistence.**  All existing UI components migrate
   in this branch.  `ViewPainter` is replaced wholesale.
6. **Tests prove pixel-accuracy.**  Snapshot test for each
   migrated component; layout unit tests for layout primitives.

## Non-goals (this RFC)

- Animation system (separate RFC).
- Theming (will get a token layer in P3 but not full theming).
- Cross-platform abstraction (marspot is macOS-only).
- Replacing the cells / ui_rects / glyph Metal pipelines.  We
  keep the GPU layer; we just put a CSS-shaped API in front of it.

## Phases

### P1 — Units + Color (foundation, no rendering change)

New types in `src/ui/core/units.rs`:

```rust
/// Logical point. 1 Pt = 1 device-independent unit.
/// On Retina (scale=2.0): Pt(1.0) renders as 2 physical px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pt(pub f64);

/// Percentage of parent's relevant dimension. Pct(0.5) = 50%.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pct(pub f64);

/// Resolvable length: either an absolute Pt or a Pct of parent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Length {
    Pt(f64),
    Pct(f64),
}

impl Length {
    pub fn resolve(self, parent: f64, scale: f64) -> f64 {
        match self {
            Length::Pt(p) => p * scale,
            Length::Pct(p) => parent * p,
        }
    }
}
```

New `Color` type in `src/ui/core/color.rs`:

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f64, // 0.0..1.0
}

impl Color {
    pub const TRANSPARENT: Self = Self { r: 0, g: 0, b: 0, a: 0.0 };
    pub const BLACK: Self       = Self { r: 0, g: 0, b: 0, a: 1.0 };
    pub const WHITE: Self       = Self { r: 255, g: 255, b: 255, a: 1.0 };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self { Self { r, g, b, a: 1.0 } }
    pub const fn rgba(r: u8, g: u8, b: u8, a: f64) -> Self { Self { r, g, b, a } }

    /// `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa` — CSS-aligned hex.
    pub fn hex(s: &str) -> Self;

    pub fn with_alpha(self, a: f64) -> Self { Self { a, ..self } }

    pub fn to_rgba_f32(self) -> [f32; 4];  // for shader uniforms
}
```

Tests: parse round-trips for every CSS hex shape; `with_alpha`
preserves RGB; `Pt(1) * 2.0 == 2 phys`; `Pct(0.5).resolve(100, _) == 50`.

### P2 — Canvas API + automatic pipeline + submission z-order

```rust
pub struct Canvas<'a> {
    // scale, parent_rect, font, atlas, ...
    // internal queue<Primitive> with monotonic submission_idx
}

impl<'a> Canvas<'a> {
    pub fn rect(&mut self) -> RectBuilder<'_>;
    pub fn line(&mut self, from: (Pt, Pt), to: (Pt, Pt)) -> LineBuilder<'_>;
    pub fn text(&mut self, x: Length, y: Length, s: &str) -> TextBuilder<'_>;
}

// Rect builder
canvas.rect()
    .at(Length::Pt(8), Length::Pt(12))       // x, y
    .size(Length::Pct(1.0), Length::Pt(1.0)) // w, h
    .fill(Color::rgba(255, 255, 255, 0.18))
    .radius(Pt(6))      // optional
    .border(Pt(1), Color::hex("#3a"))        // optional
    .shadow(Pt(16), (Pt(0), Pt(2)), Color::rgba(0,0,0,0.45)) // optional
    .draw();
```

Internals:
- Primitive enum: `Rect { fill, radius, border, shadow }`, `Line`,
  `Text { run, color, font_face }`.
- Each gets `submission_idx`.
- At flush:  primitives are bucketed by pipeline (cells / ui_rects /
  glyphs) AND by submission_idx.  Encoder walks the merged sorted
  list and emits a draw call when the pipeline changes.  Same
  pipeline runs across multiple sub-batches if z-order demands it.
- This costs ~3-5 extra encoders per overlay (cheap).
- Pixel snapping:  all integer phys coords round-half-to-even before
  encoder write so 1-Pt-tall fills land on whole pixels.
- Alpha:  always blends correctly because cells pipeline gets its
  blend state updated (currently OFF) to standard premultiplied.

Tests:
- Submission-order test:  push rect A red, then rect B blue at
  same coord.  Render to texture, assert pixel is blue (later
  submission wins) regardless of pipeline.
- Pixel-accuracy test:  draw `Pt(1)` rect at `Pt(10)` on
  scale=2.0; assert rendered band lies between phys 20..22.

### P3 — Layout primitives (Stack / Pad / Sized / Box model)

```rust
// Tree representation, immutable, computed in one pass.
pub struct VStack { gap: Pt, items: Vec<Box<dyn View>> }
pub struct HStack { gap: Pt, items: Vec<Box<dyn View>> }
pub struct Pad { all: Length, child: Box<dyn View> }
pub struct Sized { w: Option<Length>, h: Option<Length>, child: Box<dyn View> }

// Or builder-based, no trait objects:
pub struct VStackBuilder<...> { ... }

// Leaves
pub struct Filled { color: Color }
pub struct Hairline { color: Color, axis: Axis }
pub struct Text { s: String, color: Color, font: FontFace }
pub struct Hover { hovered: bool, on: Color, child: Box<dyn View> }
```

Layout pass:  measure (recursive size request → constraint
solver) → place (assign rects) → paint (Canvas calls).
Same Flexbox-ish discipline as CSS.

Example, ContextMenu reduced to declarative form:

```rust
fn build_menu(items: &[Item]) -> impl View {
    VStack::new(Pt(0.0))
        .pad(Pt(4.0))
        .children(items.iter().map(|it| match it {
            Item::Entry { label, hint, hovered } => {
                Pad::all(Pt(6.0)).child(
                    HStack::new(Pt(0.0))
                        .child(Text::new(label).color(LABEL_FG))
                        .child(Spacer::new())
                        .child(Text::new(hint).color(HINT_FG))
                )
                .background(if *hovered { HOVER_BG } else { TRANSPARENT })
            }
            Item::Divider => Hairline::horizontal(Pt(1.0), DIVIDER_COLOR).inset_x(Pt(12.0)),
        }))
}
```

Tests:
- Each layout primitive has a measure / place unit test against
  hand-calculated values.
- Each existing component (LayoutModal, ContextMenu,
  ProcessPanel) gets a snapshot test:  build the tree, render to
  a fixed-size texture, hash the result.  Hash changes = visual
  regression.

## Migration sweep (within branch)

Components to migrate to new API:
- `src/ui/components/context_menu.rs` — divider becomes Hairline.
- `src/ui/components/layout_modal.rs` — card grid becomes VStack
  of HStacks.
- `src/ui/components/table.rs` — column model maps directly.
- `src/ui/components/process_panel.rs` — master / detail split.
- All `push_*_via_view` painters in `src/render_metal.rs` —
  replaced by Canvas calls.

`ViewPainter` is deleted at end of this branch.  No deprecation
period.

## Test discipline (this branch's contract)

Per F3+11 reckoning + the "no fabricated technical claims"
feedback:  every assertion in this branch must be backed by a
test or a tool reading.  Specifically:

1. Unit test for every public primitive in `src/ui/core/`
   (units, color, primitive enum, layout pass).
2. Snapshot test for every migrated UI component, asserting
   pixel hash + per-region rect contents.
3. Headless-render scenario test for the encode-order invariant
   (rect A then B at same coord → B wins).
4. Bench `bin/bench-remote.sh` baseline on the branch before
   merge — UI primitives must not regress render p99 or RSS.

If a test can't be written for an effect, the effect doesn't
ship.

## Execution order

1. RFC committed (this file).
2. P1 — Units + Color types + unit tests.
3. P2 — Canvas + primitive queue + encode-order fix + tests.
4. P3 — Layout primitives + tests.
5. Migrate ContextMenu (smallest) → snapshot test → green.
6. Migrate LayoutModal → snapshot → green.
7. Migrate Table → ProcessPanel → snapshot → green.
8. Delete ViewPainter; verify build green; `bin/test.sh` green.
9. `bin/run.sh` sandboxed visual check on every screen mode.
10. `bin/bench-remote.sh` mini gate — must PASS.
11. Merge to `develop`, version bump, `install-local`.

User sees nothing until step 11.

## Open questions (for me, to answer with code not speculation)

1. Cells pipeline blend state — is it currently disabled?  Read
   `src/render_metal.rs::bg_pipeline` setup, find any
   `setBlendingEnabled` or `colorAttachment.blendingEnabled`
   call.  If disabled, P2's submission-order spec REQUIRES we
   enable it on cells too; verify no other call site depends on
   the no-blend behavior.
2. Submission-z-order interaction with existing extra-FG pass —
   glyphs currently encoded after both BG and UI passes.  P2's
   "submission order = z-order" needs glyphs to obey the same
   queue.  Verify text rendering doesn't break when text Y is
   below a later-submitted rect.
3. Layout primitives — Box vs typed builder.  Trait-object Box
   permits children of different concrete types; typed builder
   forbids it.  ContextMenu has homogeneous children; LayoutModal
   has heterogeneous (rows + apply button + label).  Lean to Box.

I will answer 1+2 by reading shader / pipeline setup BEFORE
writing P2.  Answer 3 by writing both and seeing which the
migration sweep prefers.

## Answers to open questions (read-the-source pass)

### Q1: cells pipeline blend state

ANSWER: **already enabled.**  `src/render_metal.rs::build_bg_pipeline`
sets the standard SrcAlpha / (1-SrcAlpha) blend:
- line 4341: `attachment.setBlendingEnabled(true);`
- line 4344-4347: `SourceAlpha` → `OneMinusSourceAlpha`.

My F3+12.5 debug note "cells doesn't blend, that's why red was
invisible" was the **fourth fabrication** in this branch.  Logged
in [[feedback-no-invented-technical-claims]].  The actual cause
the red didn't show was encode order:  overlay_cells encodes
BEFORE overlay_ui_rects, and the menu frame (ui_rects, opaque
SrcAlpha=1.0) covered the red regardless of cells-side blend.

P2 implication: cells pipeline already does what we need —
unified queue can route to cells without changing the pipeline
state.  Z-order is the only thing to fix.

### Q2: submission-z-order vs existing pass order

Main render pass order (`render_metal.rs` line 1346-1507):
1. BG cells (Clear)
2. Dots (Load)
3. UI rects SDF (Load)
4. Mono glyphs (Load)
5. Colour glyphs (Load)

Overlay pass order (line 1516-1594, repeated):
1. overlay_cells
2. overlay_ui_rects
3. overlay_mono_glyphs
4. overlay_colour_glyphs

So both main and overlay are "BG → UI → glyphs"; my divider
red rect went into overlay_cells = drawn before menu frame.

P2 strategy: SAME unified queue carries Primitive { z, payload }
for both main and overlay.  Encoder walks the queue in z-order,
switching pipelines on payload type change.  Roughly 4-8 extra
encoder objects per overlay frame (cheap).

For text-vs-rect interaction: a later-submitted rect WILL cover
earlier-submitted text in the same Z bracket — this is the
new contract, callers stack accordingly.  Existing components
all submit "frame first, then content" so the migration is a
direct mapping.

### Q3: layout primitive shape (Box vs typed builder)

Deferred to P3 prototype.  Will write both and pick.
