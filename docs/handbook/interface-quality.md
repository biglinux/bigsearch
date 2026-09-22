# Interface quality: discover, act, understand, recover

This is a design and review contract, not a claim that the current interfaces pass.
It applies to applets, application menus and their configurators. Keep the ownership,
state and lifecycle rules in [architecture](architecture.md) and the specific layout
in [applet layout](applet-layout.md). A Git boundary does not require another process,
widget implementation or copy of the design system.

The [Big Design System 2026](big-experience-2026.md) is the product-direction
companion to this testable contract. It uses current web practice rather than
Fluent as an organising style, and defines the planned suite-wide labelled profile,
BigSearch context and common configurator behaviour. Existing native tokens remain
the implementation baseline until a measured migration replaces them.

## 1. Discovery is part of completion

Review a task from the closed desktop, not only from the correct open dialog. Define
what the person wants, where they would first look, the visible cue, the action,
the confirmed result and recovery. A help page, tooltip, context menu or search term
they do not know is not a substitute for an intelligible entry point.

Sound keeps application volumes visible on the left and devices on the right in LTR,
with logical order mirrored in RTL. Do not replace application controls with a tab.
When no program is playing, consider a short explanation of where its volume will
appear, rather than an empty second column. Changing streams must not move a control
under a pointer that is dragging it. Network remains one column with a small labelled
traffic reading. Preserve advanced capabilities through identifiable entry points.

Each visible action must pass two separate questions: can people find it unaided,
and can they understand/use it once found? Test both. Familiarity of the author with
ellipsis icons, profile names or categories is not evidence of discoverability.

## 2. One coherent visual language

Use existing tokens and controls before adding any. Values below are starting rules
for design review, not newly applied CSS or measured optimal sizes.

| Layer | Big rule |
|---|---|
| Spacing | Start with the existing 4/8/12/24 logical-unit rhythm. Closer spacing means stronger relationship. Add a missing role centrally only after comparing consumers; do not scatter a new magic number. |
| Corners | Existing small/card radii are 6/12. Map by role, not per applet. Pill shapes communicate a deliberate control/badge, not generic decoration. Nested edges must not produce inconsistent tangencies. |
| Elevation | Use a single popup boundary; children on the same plane do not each need shadows. Theme/high-contrast boundaries may use a contrasting rim instead. Inspect the actual anchored surface, not an extracted child. |
| Colour | Semantic surface/text/action/selected/error roles; theme and high contrast override values. Accent means an action or state, not decoration everywhere. Colour never carries the only distinction. |
| Transparency | Start with opaque content behind text and controls. A translucent outer shell is optional and requires contrast across wallpapers plus a performance budget. Do not apply opacity to a parent containing text. |
| Type | Inherit the user's UI font/size. Differentiate roles by scale and weight, not tiny low-opacity captions. As an initial optical reference, normal labels around 14–16 logical units at ordinary desktop settings; this is not a forced replacement of system fonts. |
| Icons | Use one symbolic vocabulary for actions. Typical artwork is 16/20/24 logical units; its hit area is larger. Use actual application identity icons, with a guaranteed meaningful fallback. No separate coloured plates behind audio/network icons. |
| Density | Obtain compact rows by removing duplicate frames, repeated labels and excessive padding, not shrinking targets or essential text. Let long/large-text rows grow. Never clip a focus outline to meet a height target. |
| Numbers | Align volume/traffic readings and reserve sensible width. Tabular numerals where supported. A new number must not cause layout oscillation. Missing, zero, muted and pending are different states. |
| Motion | Existing tokens include 150/250 ms. Use short productive transitions to explain change, not to delay it. Avoid bounce, zoom theatrics, continuously blurred backgrounds and arbitrary animations on every row. |

No blur or shadow radius is universally correct. Judge the actual theme, background,
scale, composition and cost. Do not import Fluent, Carbon or another product's entire
appearance. The dated 2025/2026 web sources in the companion guide inform Big's
own visual and interaction vocabulary; they do not imply copying expert-only density.

## 3. Every interactive component has a state contract

| State/event | Required observable behaviour |
|---|---|
| Rest | Label and structure identify the action before hovering. |
| Hover | Subtle local response without shifting geometry. Essential actions were already present. |
| Keyboard focus | Clearly distinguishable from hover and selection; not clipped or hidden. No task depends on hover. |
| Press | Immediate local feedback; ordinary actions commit on release, with cancellation when leaving the target where appropriate. No button executes a destructive action just on focus. |
| Selected/checked | Persistent shape/check/text plus colour; distinct from focus and temporary pressed state. |
| Pending | Requested state is not falsely reported as confirmed. Local feedback, bounded work and error recovery; do not blank the whole card. |
| Error | Explain what failed and a feasible next action, near its origin. Preserve input and successful unrelated work. |
| Disabled/unavailable | Give a discoverable reason where useful; avoid invisible controls that hide available functionality. |
| Updated remotely | Do not steal focus, jump the pointer target, reorder the active row, or announce rapidly changing meters. |
| Closed/hidden | End visit-scoped tasks/subscriptions; preserve the R12 close/reopen ownership invariants. |

Tooltips supplement labels and details; they do not contain essential instructions,
connection errors, the only difference between similarly named devices, or the only
way to discover a feature. Dialogs restore focus to a valid origin; Escape dismisses
the inner transient first. Avoid overlapping destructive and high-frequency targets.

## 4. Accessibility is not a colour palette

WCAG 2.2 is a web reference; WCAG2ICT is informative guidance for non-web software,
principally for A/AA criteria. Neither a screenshot nor a heuristic review certifies
this desktop. “Premium” and “AAA+” are not accessibility conformance levels.

Use normal text contrast of at least 4.5:1 and meaningful non-text controls/states of
3:1 as explicit review thresholds. Large-text exceptions must meet their actual size
and weight definitions. A 7:1 normal-text aspiration is the enhanced contrast target,
not permission to claim complete AAA. Decorative dividers do not all require 3:1.
Compute contrast from effective foreground/background colours including compositing;
do not measure antialiased glyph-edge pixels and call that normative text contrast.

Prefer a 44×44 logical-unit target for frequent/critical controls and touch. Smaller
desktop targets require measured boundaries and spacing; a 24×24 web AA floor has
specified exceptions and is not a general ideal. Do not equate raw screenshot pixels
with GTK logical units without recording scale. The existing 32-pixel desktop floor
is not evidence of enhanced 44-pixel target compliance.

Verify 200% text scaling (not just doubling image pixels), high contrast, reduced
motion, keyboard and assistive-technology name/role/value/action. The enhanced focus
appearance reference includes area equivalent to a 2-pixel perimeter and 3:1 change
contrast, with exceptions; use it as a stated design target, not an implicit claim.
Offer a single-pointer alternative to dragging. Retain native GtkScale semantics,
keyboard arrows/Home/End and an intelligible name, unit and muted state.

Test a real RTL translation and mixed-direction identifiers. Ellipsis must not hide
the distinguishing suffix of similar device/network names. Wrap or allocate identity
space before abbreviating essential names. Screen-reader text complements rather
than repairs unreadable visible labels. Do not apply multiple opacity reductions to
captions before measuring them in all themes.

## 5. Product-specific review

### Sound

Application identity, volume and mute remain in the first layer. Output selection
must make the active destination unambiguous; microphone controls must look and read
as microphone controls. Show a muted state explicitly while retaining its stored
volume. Reserve an intelligible route entry such as the destination name; keep ports,
profiles, amplification and advanced routing accessible without showing every option
at once. Use the same row/state owner in the applet and Sound Centre.

Check zero/one/many streams, identical prefixes, hotplug, active recording, silent
streams and destination disappearance during interaction. Keep application and device
volume differences comprehensible; moving a stream is not changing the system default.

### Network

Name the active connection and actual connectivity. Healthy, captive portal, limited,
blocked radio, permission refusal and disconnected are distinct. Give diagnostic
recovery visual priority when it is relevant, not permanently over normal actions.
Keep Disconnect and Forget distinct. Forgetting credentials should have an explicit
network-named action and a proportionate confirmation; never depend on a trash glyph.

Use a concise, labelled receiving/sending pair with units. It represents interface
traffic, not a bandwidth promise or speed test. Put addresses, protocol details and
signal percentages behind an identifiable disclosure without pushing Wi-Fi and
connection actions out of reach. Preserve password, VPN, hotspot and advanced pages.

### Menus and configurators

Grade/Grid, Categories, Classic, Full screen and Overview may differ in geometry but
must share identity, search semantics, activation, context actions and focus rules.
A fullscreen layout inside a test window is not a fullscreen session test. Classic
may retain its conventional search position; first focus and keyboard entry must
still be obvious. Configured layouts/features must not be silently removed.

Design first use and empty favourites deliberately: name the collection, offer an
obvious way to view/pin apps, and keep a valid empty collection distinct from failed
index/catalogue loading. A missing icon never leaves an unidentified blank target.
Expose session actions through names, not a row of ambiguous power symbols alone.
Settings must be reachable from the relevant applet and found by task-oriented search
terms. Do not add a second settings backend/store just to present simpler controls.

## 6. Rendering and performance

GTK CSS is not the browser DOM/layout engine. Use GtkBox/Grid/list models and the
existing adaptive helpers for geometry. Validate selectors/properties against the
supported GTK 4.22.5 and libadwaita 1.9.4, even when online docs are newer. GTK's
focus-visible propagation includes ancestors; test the actual focus target rather
than copying a global web outline rule onto every container.

Use GSK/Cairo for justified visual primitives, not a replacement for accessible
labels, buttons and sliders. Preserve native interaction and explicit ownership.
Paint only changed areas; reuse rows, models, icons and service observations. Draw a
meter only while visible and changing. Do not add a permanent timer/worker for polish.
Respect gtk-enable-animations; reduced motion and high contrast may intentionally
snap. There is no requirement to animate every hover or focus transition.

Measure input latency, frame-time distribution, cold/warm opening, idle activity,
PSS and resource release. A 60 Hz frame offers about 16.7 ms end-to-end; this is a
reference budget, not an observed Big result or a licence to spend all of it on UI
work. Preserve an OpenGL path suitable for the reference i3/4 GiB/HDD and a readable
software fallback. Do not require Vulkan, animated blur or more processes for polish.

## 7. Evidence and human testing

Record revision, builder/product, fixture or real service, locale, theme, font,
text scale, window allocation, monitor/work area, renderer and capture method. Keep
widget screenshots, anchored desktop captures, videos, input tests and user studies
separate. An unavailable field is unknown, never inferred from a filename.

At minimum, cover the affected states in both themes, actual narrow allocation,
large text, high contrast and the real anchored popup over light/dark/busy backgrounds.
Capture pointer/focus states, empty/loading/error and long/similar labels. Run all
five menu layouts; reuse scenario data but not a duplicate implementation of the UI.
A lifecycle test across 23 constructors does not prove 23 complete UX audits.

Start usability tasks from the closed desktop. Example: “Keep the music at its
current level, but make the video quieter.” Do not tell the participant to open the
sound applet or which column to use. Observe first choice, unassisted completion,
wrong turns, recovery, retained settings and explanation of the result. Use consenting
participants, including people with disabilities and people unfamiliar with Linux;
store only necessary redacted evidence. Counterbalance layouts to limit learning bias.

An initial qualitative round of 6–8 people is a proposed iteration size, not proof
of universal usability. Release claims need an appropriate larger evaluation with
raw counts, denominators, participant/task descriptions and uncertainty. Never hide a
critical failure in an average score. A performance/visual regression, unsafe action
or undiscoverable everyday task blocks promotion independently of aesthetic preference.

Keep outcomes in dated evidence, policies here, and unfinished changes in the issue
backlog. The maintained beauty gate defines required evidence; historical PASS counts
must not be copied into it as permanent guarantees.

## Primary references

- [WCAG 2.2](https://www.w3.org/TR/WCAG22/) and [WCAG2ICT](https://www.w3.org/TR/wcag2ict-22/).
- [Linear UI refresh, March 2026](https://linear.app/changelog/2026-03-12-ui-refresh).
- [Atlassian visual refresh, April 2025](https://atlassian.design/whats-new/atlassian-ui-refresh-updates).
- [Spectrum 2 implementation stable, December 2025](https://react-spectrum.adobe.com/releases/v1-0-0).
- [Base UI releases 2025/2026](https://base-ui.com/react/overview/releases).
- [DTCG stable reports 2025.10](https://www.designtokens.org/tr/2025.10/).
- [WAI cognitive guidance: visible labels](https://www.w3.org/WAI/WCAG2/supplemental/patterns/o4p06-clear-labels/).
- [Carbon motion](https://carbondesignsystem.com/elements/motion/overview/).
- [Atlassian design tokens](https://atlassian.design/tokens/design-tokens/).
- [WAI slider interaction](https://www.w3.org/WAI/ARIA/apg/patterns/slider/).
- [GTK CSS](https://docs.gtk.org/gtk4/css-overview.html) and [supported properties](https://docs.gtk.org/gtk4/css-properties.html).
- [GOV.UK moderated usability testing](https://www.gov.uk/service-manual/user-research/using-moderated-usability-testing).

Consulted 2026-09-21. Web patterns inform the reasoning; they do not replace native
GTK/AT-SPI contracts, user research or measurements on the supported desktop.
