# Client keyboard, focus, responsive, and accessibility audit

Bead `winwincode-mrt` (logical ID UI-604, parent UI-600). Scope: the Shell, Chat,
StrongFlow, Diff, Graph, Attention, Settings, and Enterprise surfaces of
`apps/client`.

Status: findings A1–A8 below are fixed and pinned by tests. Residual risks are
listed at the end.

## How this audit was produced

Two passes over the same surfaces:

1. **Markup and behaviour review** of every mounted view in `apps/client/src`
   and of the token/CSS layer in `apps/client/src/styles`.
2. **Real-Chrome walkthrough** with DevTools Protocol, driving the built client
   through each surface at desktop width, at a 640 px window (the 200 % zoom
   case for the 1280 px design), and at 360 px. Focus order, focus visibility,
   landmark roles, and the live-region set are read back from the page rather
   than asserted from source.

The automated checks live in two suites:

| Suite | Covers |
| --- | --- |
| `tests/ui604-shell-a11y-browser.test.mjs` | Landmarks, heading outline, live-region allow-list, skip link, 200 % zoom and narrow-viewport overflow, for Chat, Settings, Attention, Local Operations, and session decisions |
| `tests/ui604-a11y-audit.test.mjs` | Kanban keyboard advance, Chat conversion dialog semantics, panel heading nesting, Diff table caption and column headers |
| `tests/chat-page.test.mjs`, `tests/strongflow-delivery-list-page.test.mjs`, `tests/strongflow-diff-viewer.test.mjs`, `tests/enterprise-operations-page.test.mjs`, `tests/enterprise-resource-page.test.mjs` | The same contracts asserted next to the behaviour they belong to |

## What was already correct

Recorded so it is not "fixed" again by accident.

- Focus is always visible: `styles/base.css` gives every button, link, field, and
  `[tabindex]` a `:focus-visible` outline built from `--wwc-color-focus`,
  `--wwc-focus-width`, and `--wwc-focus-offset`.
- Colour is never the only state signal. `mountStatusBadge` pairs a tone with a
  text glyph and a text label; Diff rows carry a `+`/`-`/space marker span next
  to their tint; StrongFlow resize handles, tabs, and toolbars expose
  `aria-valuenow`, `aria-selected`, and `aria-pressed`.
- Graph components have a list alternative. `strongflow-diagram-graph.ts` offers
  a graph/list toggle (`aria-pressed` on both options), an edges `role="list"`,
  per-node `aria-label`, and roving `tabindex` across the viewport.
- Roving `tabindex` is used consistently for tree-like collections: candidate
  files, history navigation, Diff rows, graph nodes, and the tabs/toolbar
  primitives.
- The Drawer component already has `role="dialog"`, Escape handling, focus on
  open, and focus restoration on close; the StrongFlow narrow drawers reuse it.
- Split panes expose named `role="region"` children and keyboard resize handles
  (`aria-orientation`, `aria-controls`, `aria-valuenow`, Arrow keys).
- Every linear dimension is `rem`/token based; the only `px` values are the
  border and focus outline widths, so the layout scales with text zoom.
- `prefers-reduced-motion` collapses transitions and scroll behaviour.

## Findings and fixes

Severity: **blocker** = a primary flow cannot be completed as-is; **major** = a
whole class of users gets a broken or unusable experience; **minor** = correct
but degrading.

### A1 — Blocker — Shell turned every page into a live region

`application.ts` set `aria-live="polite"` on `section.wwc-surface-slot`, the
container that holds the entire mounted page. Every realtime DOM change anywhere
on any surface was therefore queued for announcement, and because pages also
mark their own status lines live the announcements nested and repeated.

*Fix*: the slot is no longer a live region. Status announcements stay on the
page's own single polite status line and on the shell's connection badge.

### A2 — Major — Collection containers announced their own re-renders

Ten collection containers were `aria-live="polite"`: the Attention Center card
list, the Settings Credential list, the Local Worker list, the three
Local-decisions lists (inputs, approvals, Attention), and the Enterprise
Policy, Fleet, Usage, Audit, Integration, Organization, Member, Role, Project,
and Repository lists. A keyed collection replaces nodes on every realtime tick,
so a background refresh re-read the whole list.

*Fix*: `aria-live` is removed from every collection container. The dedicated
`*-status` line next to each list stays live, so a change is announced once as a
sentence instead of as a re-read of the list.

### A3 — Blocker — Delivery Kanban advance was drag-only

In `strongflow-delivery-list-page.ts` the Kanban card move was HTML5 drag and
drop only (`draggable` + column `drop`). A pointer-free user could not advance a
Delivery from the Kanban view at all, which fails the "primary flows are
keyboard-completable" acceptance criterion (WCAG 2.1.1 Keyboard, 2.5.7 Dragging
Movements).

*Fix*: each card carries a `wwc-delivery-kanban-advance` button that calls the
same `advanceDelivery(deliveryId, expectedRevision)` the drop handler calls. It
is labelled `Advance <Delivery title>` so a screen reader names the target, and
it is hidden and disabled while the surface is read-only. The drop path is
unchanged.

### A4 — Major — Chat conversion confirmation was an unlabelled, focus-less panel

The "Convert to StrongFlow" confirmation was a plain `section` with an `h3`.
It had no dialog role, was not named by its heading, did not move focus when it
opened, could not be dismissed with Escape, and left focus on the trigger when
it closed.

*Fix*: `role="dialog"` with `aria-modal="false"`, `aria-labelledby` pointing at
its heading, `aria-controls`/`aria-expanded` on the trigger, focus moves to the
first field on open, Escape closes it, and focus returns to the trigger on
close by any path (Escape, Cancel, or the Chat session changing underneath).

### A5 — Major — No bypass for the repeated header navigation

The product-area navigation is repeated on every surface and sat before the page
content in the tab order, so every page change cost a full pass through the
header (WCAG 2.4.1 Bypass Blocks).

*Fix*: a `Skip to main content` anchor is the first focusable element in the
header. It is clipped out of the visual order until focused, and activating it
calls `preventDefault()` — so the hash router never sees it — then focuses
`main`, which is now `tabindex="-1"`.

### A6 — Major — Two `<main>` landmarks and two page headings per surface

Six feature pages (Settings, Attention Center, Local Operations, Local
decisions, Enterprise operations, Enterprise resources) built their layout as a
`<main>` inside the Shell's `<main>`, and also mounted a page header at
`headingLevel: 1` next to the Shell's `<h1>` surface title. The result was two
`main` landmarks — nested `main` is invalid, and the landmark map a screen
reader builds for the page was wrong — plus two first-level headings.

*Fix*: a page is a `<section>` inside the Shell's one `<main>`; the Shell owns
the single `<h1>`; feature page headers moved to level 2; the panels inside them
moved to level 3, and the error/empty states nested in those panels follow with
a `headingLevel` option instead of hard-coding level 2.

### A7 — Minor — Diff table had no caption or column headers

The Diff body is a real `<table>`, but with no `<caption>` and no `<thead>` a
screen reader reads `1 1 const one = 1` with no way to tell an old line number
from the changed text.

*Fix*: the table carries a `<caption>` naming the selected file and a
`<thead>` row of `scope="col"` headers. The header set follows the active
layout: `Old line / New line / Line content` unified, `Old line / Removed
content / New line / Added content` side by side.

### A8 — Blocker — Nested `<main>` broke the landmark map

Recorded separately from A6 because it is the structural half: `main` must not
have a `main` descendant. Before the fix, VoiceOver and NVDA landmark navigation
listed two main regions per surface and the inner one swallowed the page
content. Fixed together with A6; the browser suite asserts
`document.querySelectorAll('main').length === 1` on every surface.

## Manual test checklist

Run this by hand before calling a release accessible. Automation above covers
the structural parts; the items below need a person.

### Keyboard only (no pointer)

1. Load `#/chat`. Press `Tab` once: the `Skip to main content` link appears
   inside the header. Press `Enter`: focus lands on the page content, the URL is
   unchanged.
2. Tab through the whole Chat surface. Confirm every control is reachable in
   reading order, nothing is reachable twice, and focus is always visible.
3. Submit a message with `Enter`; insert a line break with `Shift+Enter`.
4. Open `Convert to StrongFlow`. Confirm focus jumps to `Delivery title`,
   `Escape` closes the panel, and focus is back on the trigger.
5. Move to StrongFlow. Confirm the delivery list search, status filter,
   attention filter, order, view toggle, refresh, and load-more are all
   operable.
6. Switch to the Kanban view. Tab to a card, reach `Advance`, press `Enter`, and
   confirm the Delivery advances and the card stays in its server-owned column
   until the projection changes.
7. Open a Delivery. Operate the artifact tabs, candidate file list, Diff search
   (`Enter` next match, `Shift+Enter` previous), `j`/`n` and `k`/`p` hunk
   navigation, `u`/`s` layout switch, and the hunk collapse toggles.
8. Operate the graph with the keyboard: node roving tabindex, zoom in/out/fit,
   trust-boundary headers and chips, and the graph/list toggle.
9. Open the Attention Center. Change the type and order selects, reach each
   card's context link, and confirm disabled cards are skipped without trapping.
10. Open Settings and Enterprise. Change every select and submit every form with
    the keyboard only, and confirm validation errors are announced.
11. Narrow the window until the StrongFlow drawers appear. Open each with
    `Enter`, close with `Escape`, and confirm focus returns to the button that
    opened the drawer.
12. Confirm Escape closes every overlay and returns focus to where it came from.

### Screen reader

Run once with VoiceOver (macOS) and once with NVDA (Windows), against the
production build.

1. The landmark list shows one banner, one navigation (`Product areas`), and one
   main region. No nested main.
2. The heading outline is `h1` page title → `h2` sections → `h3` panels, with no
   skipped level on any surface.
3. The Chat status line, the StrongFlow header status line, and each page's
   status line announce once per state change — not once per re-render.
4. A background realtime update on the StrongFlow workbench produces no
   announcement at all; a state change produces exactly one.
5. Every icon-only control (drawer close, graph zoom, copy diagnostic) announces
   a name.
6. Diff rows read as `Old line 3, New line 4, Line content …`.
7. The Attention Center cards read kind, urgency, deadline, and binding target
   before the action link.
8. Connection loss announces once as `Offline`, and recovery announces once.

### Zoom and responsive

1. 200 % browser zoom at every surface: no content is clipped or lost, no
   horizontal scrolling appears, and every control stays at least 24 px wide.
2. 360 px viewport: the StrongFlow navigation and context move into drawers,
   resize handles disappear, and the delivery list, evidence, and approval
   actions remain reachable.
3. 320 px viewport: the header navigation wraps instead of clipping, and no
   surface scrolls sideways.
4. High-density desktop (two 1440 px columns side by side): the StrongFlow three
   column split keeps the candidate Diff readable and does not push the context
   column off screen.
5. 400 % zoom on a single control (WCAG 1.4.4 reflow at 320 px): text is not cut
   and no control overlaps another.

### High-density desktop

1. With the StrongFlow workbench at its maximum navigation and context widths,
   the main region still keeps at least 40 characters of Diff line visible.
2. Resizing with the keyboard handles matches resizing by pointer, and the
   stored layout preference survives a reload.

## Residual risks

- **Streaming chat announcements.** `wwc-chat-messages` is a polite live region
  with `aria-relevant="additions text"`, which is the right shape for a
  transcript, but a streaming reply mutates the message text in place and may be
  announced repeatedly by some screen readers. The chat fixture models a
  completed message, so this needs a live streaming run to measure.
- **Enterprise surface is covered by the node suites, not the browser suite.**
  The browser audit walks Chat, Settings, Attention, Local Operations, and
  session decisions. The Enterprise Policy, Fleet, Usage, Audit, Integration,
  Organization, Member, Role, Project, and Repository lists are covered by the
  aria-live assertions in `tests/enterprise-operations-page.test.mjs` and
  `tests/enterprise-resource-page.test.mjs`; their landmarks and headings
  inherit the same Shell and page-header components the browser suite checks.
- **Screen-reader coverage is manual by nature.** The automated suites assert
  roles, names, levels, and the live-region set, not how a specific screen
  reader renders them. Use the checklist above.
- **Visual regression is out of scope here** and is tracked by UI-608, which
  this bead blocks.
