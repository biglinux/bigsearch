# Compact applets without hiding the task

Sound and Network are frequent, short interactions. Make the state and next action
clear without requiring knowledge of profiles or protocols. Compactness must not
remove functionality, shrink text or native targets, or clip keyboard focus.

## Sound: applications first, devices second

In a left-to-right interface, applications playing audio are on the **left** and
outputs/inputs on the **right**. Let GTK mirror logical order for RTL. This is not
a second tab. With no streams, hide the application column; do not reserve empty
space. A short hint explains that per-application volume appears when sound starts;
a device-only page in the Sound Centre must not promise controls it does not contain. When width is insufficient, the same widgets wrap below, not another model.

The private `row::companion_columns` helper accounts for either column being hidden.
Initial budgets are 340 + 340 logical pixels with a 12-pixel gap; these are Big's
reviewable choices, not universal GNOME requirements. Changing a number must not
rebuild the list or move the pointer target; structural changes can change width.

A device row has name/context, then slider/meter/value. Short identities and context
share one native text layout; long names wrap rather than hiding the distinguishing
part. Both external strings are escaped before adding Pango emphasis, and numeric
updates do not rebuild this text. This avoids inconsistent minimum/natural sizing
from nested wrapping containers on the supported toolkit.
Auxiliary device buttons span the two lines. Application routing is named **Play on…**
and wraps below the slider only when native controls/large text need the space.
Rows in a group share one surface and separators, not an individual pill each.
Preserve mute,
default-device selection, level meter, amplification, ports, routing and details.
Use the same row in the Sound Centre, with 4/8 vertical/horizontal spacing and no
fixed row height. Icon plates have no separate tint; native focus and hover remain.
Keep full names accessible and in plain-text tooltips. Test similar long prefixes,
large text and translations. Muting must not discard the stored volume setting.
The visible state is **Muted**, not an unexplained dash. Preserve the source identity:
a muted microphone is still a microphone and a muted application retains its icon.
Reserve the localized state width so toggling mute does not shift a slider during
a gesture. Secondary action labels and descriptions update after device renaming.

## Network: one column and passive traffic

Keep the current connection, Wi-Fi, networks, VPNs and recovery controls in one
column. Details expand **after** the everyday controls, not before Wi-Fi and
network selection. Closing details restores focus to their origin, or the surviving
connection control after a rescan. Use a single bounded
viewport, not nested scrolling or a second column. Addresses remain selectable and
wrapping. Icons have no separate plate colour; security/signal information remains.

A small **Receiving / Sending** line shows bytes per second on the active interface.
Disconnect remains named and neutral beside/below this line according to space.
Signal percentages stay in technical details and the full accessible description;
the first-level connection list uses a concise state without discarding the data.
It reuses the system monitor's bounded `/proc/net/dev` reader. It does **not** start
a speed test or contact a server. Existing explicit speed-test actions are separate.
This is total interface traffic, including local activity and protocol overhead;
it is neither a negotiated link speed nor guaranteed Internet throughput. VPN,
bond and multi-interface topologies require hardware/session validation.

Sampling is off the GTK thread and exists only for a mapped visit. There is no
unopened/closed-card timer, extra daemon or thread pool. The first reading, read
failure, disconnect, changed activation/interface or counter reset is unknown (—),
not an invented zero. Two comparable unchanged counters mean zero. Use monotonic
elapsed time, not an assumed exact timer interval. Accessible text names download
and upload; it is not a live region announcing every sample.

NetworkManager connectivity values must not confuse a captive portal (2) with
limited connectivity (3). Unknown is not proof of failure. Preserve connection,
authentication, permission and configuration owners rather than adding another
networking backend for presentation. A Wi-Fi request keeps the requested switch
position distinct from the confirmed state. Mark it busy, serialize requests and
show a nearby error/rollback on refusal. Use the existing bus connection, pin the
daemon owner and read its uncached property after the write; never report the
requested value as confirmation. Owner loss invalidates late completions.

### Saved network actions and ownership

**Forget this network** is a rare saved-profile action. It exists only after the
person explicitly opens **Connection details**, never beside first-level network
selection. A saved network that is not connected has a named **Details** disclosure;
opening it must not connect, request a password, or show the active network's IP
addresses as if they belonged to that profile. Both detail routes reuse one visit-owned confirmation,
show the exact network name as plain text and explain that saved credentials/settings
will be removed. Cancel receives focus, Escape dismisses the inner decision and
focus returns to its origin if it survives. A repeated press cannot switch the
pending network; a retained button cannot confirm twice or after closing.

Confirmation emits the existing operation; it does not create a second NetworkManager
backend or guarantee transactional profile identity during concurrent external edits.
Tests use an inactive/fake command receiver, not real removal of a user's profile.

Details are constructed on demand and released when closed. The detail owner also
owns the confirmation; callbacks use weak back-references. An unchanged service
snapshot preserves the existing controls and focus instead of rebuilding them.
A change of detail target, loss of the service, or a confirmed scan that reports
that the profile is no longer saved cancels an outstanding decision. A missing
access point alone does not prove deletion of a profile. These rules do not add
transactional profile identity to the existing network backend.

Callbacks owned by a row must not strongly capture its parent listing or password
entry. The QR-secret lookup is visit-bound and cancels on unmap/destruction;
late completion cannot write into another selection or keep a removed control alive.
The mapped confirmation scrolls into view after its first allocation, using the
native viewport API once. The pending frame handler is weak and disconnected on
cancel/close; it is not an ongoing timer. Test allocated button bounds against the
viewport, not just the mapped flag: a mapped button can still be clipped.
A confirmation clears focus only inside its subtree before it hides, not arbitrary
external focus. No forced GObject disposal or periodic purge is used.

## Popover ownership and focus

Build cards lazily and end the visit on ordinary close, panel removal or hiding.
GTK `unmap` is not equivalent to its `closed` signal. Inspect the popup’s own
`get_visible()` property when ending its visit: `is_visible()` also checks
ancestors and becomes false when the panel hides, even if its popup is still open. End visit-scoped sampling,
subscriptions and pending UI delivery even when the launcher is retained.

Do not unparent a popover inside its own native close stack. Defer detachment,
keep weak callback back-references, and reject old cleanup after replacement or
rapid reopening. Focus outside the closed subtree must not be stolen. A context
menu must be reachable with keyboard as well as pointer, and repeated activation
must reuse its existing popup rather than stack native grabs.

GTK 4.22.5 may hold a native popup in its deferred focus slot until the root
can move focus. Detach finished content on idle, even if that empty native shell
remains queued. Clear only focus within the closing/removed subtree. Never hide
a launcher synchronously inside its native active/closed notification: the update
applet did this and overwrote GTK's pending focus reference. Reconcile its current
badge/active state on idle; reopening or a new badge must win over stale cleanup.

Tests must prove the native popup actually mapped, verify immediate content release,
and then verify shell release after native focus dispatch. The 23-factory matrix
covers ordinary close, launcher removal and a hidden panel. Availability overrides
are fixtures, not claims of working hardware. A separate test covers hardware
vanishing while a card is still being built. A successfully compiled ignored test is not a
runtime test. Do not disable accessibility or swallow GTK criticals to pass.

## Application menu layouts

Review Grid, Categories, Classic, Full screen and Overview separately. All retain
their navigation and configured actions. Classic's field filters applications;
the other layouts use the shared computer search and must name that broader scope.
Overview hides its desktop/folder browsing while a query is shown and restores it
when cleared. Category names wrap naturally instead of using a twelve-character
ellipsis. Large text can create more rows; it must not abbreviate away their purpose. Desktop thumbnails scroll locally rather than forcing a wide menu.

Use real allocation and CSS backgrounds, never negative GtkWidget margins to
paint outside a menu: those margins can produce negative sizing constraints.
A good category-sidebar layout is not evidence that the other four were tested.

Home without favourites or recently installed applications offers a named **All
applications** button as well as the category sidebar. The action selects the
existing category; it neither launches a program nor creates artificial favourites.
Its callback retains only a weak reference to the category list. Reopening or
updating the menu must not keep an old widget tree alive through the empty state.

## Quality beyond compactness

Use [interface quality](interface-quality.md) for discovery, target sizes, semantic
colour, motion and user testing. Compactness alone is not clarity: muted audio needs
a visible state; technical details must not displace connection actions; an empty
application column must not erase discovery of per-application volume. Tests named
wide/narrow need different measured allocations. Extracted content does not validate
actual popover shadows or fullscreen placement. These are review requirements, not
claims that the R12 fixtures already pass.

## Review and contribution

| Area | Cases |
|---|---|
| Sound | No/many streams, several outputs/inputs, mute, amplification, routing, Sound Centre reuse. |
| Network | List/details, pending/failure, no radio, permissions, VPN, long addresses, changing active link and counter resets. |
| Layout | Light/dark, constrained width/height, enlarged text, translated/similar names and RTL. |
| Input | Keyboard activation, sliders, context menu, close/reopen, focus after refresh and disclosure. |
| Lifetime | Closed/hidden/removed owner, rapid replacement, retained children, no permanent sampling. |
| Menus | Each layout's search, clearing, navigation, popup/overlay lifecycle and resource release. |

Real Rust builder tests live in `applets/*layout*_tests.rs`, `applets/layout_review.rs`,
`applets/transient_tests.rs`, `clock/popover.rs` and `menu/layout_review.rs`. Execute
ignored GUI cases explicitly with `--ignored --exact` in a private display/session,
with one test thread and a finite timeout. `BIG_APPLET_REVIEW_OUTPUT` optionally
writes screenshots and measurements into a new directory. Photos use deterministic
service data; backend operations, Wayland, physical GPU and screen readers need
separate tests. The applet visit matrix opens real factories without performing
system-changing actions; absent devices must never be reported as hardware passes.

See [validation](validation.md) and [ownership](architecture.md). Repository
ownership remains separate from process composition: these components belong to
`big-desktop`; the framework and other products must not acquire applet dependencies.

## Required rendering checks for discovery changes

Run the actual Rust constructors, not a reconstruction in HTML/C. Existing tests in
`audio/card_layout_tests.rs`, `network_layout_tests.rs`, `network/forget.rs` and
`menu/layouts/overview.rs` cover visible state, exact names, routing/slider regions,
native focus, cancellation and release. Run ignored bodies explicitly, one per
process, with `--ignored --exact`.

For `anchored_network_remains_reachable_on_a_small_display`, provide an actual
800×600 display, not just a window named “small”. Other layout tests photograph
component content and record requested/allocated sizes. Enable `BIG_TEST_LOCALE_DIR`
only in tests to load the compiled catalog and use `LANGUAGE=pt_BR` with an installed
non-C UTF-8 locale for translated rendering. Set `BIG_TEST_EXPECTED_SOUND=Som` to
require proof that the catalog really loaded; `LANGUAGE` alone under `C.UTF-8`
does not provide that proof. An isolated test alias of the installed C UTF-8 locale
can exercise translated labels when locale data is missing, but it is not a test
of Brazilian date, number or collation formats. These hooks do not change
production locale policy. Record the actual locale and any such alias.

The native X11 popup test is not Wayland, a full desktop, a touch/Orca audit or a
physical network test. Source assertions, crops and callback simulations do not prove
real pointer gestures by themselves. Preserve failed attempts next to the final
results instead of hiding a clipping/retention failure behind a new test name.

## Primary references

- [GTK Popover](https://docs.gtk.org/gtk4/class.Popover.html)
- [GTK closed signal](https://docs.gtk.org/gtk4/signal.Popover.closed.html)
- [GNOME popovers](https://developer.gnome.org/hig/patterns/containers/popovers.html)
- [GNOME menus](https://developer.gnome.org/hig/patterns/controls/menus.html)
- [libadwaita WrapBox](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/1-latest/class.WrapBox.html)
- [NetworkManager states](https://networkmanager.dev/docs/api/latest/nm-dbus-types.html)
- [Linux interface statistics](https://docs.kernel.org/networking/statistics.html)

Online documentation can describe a newer release. Validate implementation details
against the supported GTK/libadwaita sources and the actual driver/session too.

## Weather: unavailable is not zero

The details window's hourly charts retain missing observations as gaps. Neither
line nor area fill crosses a missing time slot. An isolated observation is a
point, not an invented interval. A genuine 0% rain forecast remains drawable;
when no valid observations exist, show “Forecast data unavailable” instead of an
empty chart or a dry-weather claim. Partial data has a visible explanation.

The chart's accessible name identifies the measurement; its description lists
available hours and values in the selected units and explains any gaps. This is
not a live region. Geometry is prepared once with the snapshot; repainting does
not fetch, allocate series or install a timer. The existing forecast service,
cache and notification policies are not replaced.

The details page wraps the same hourly cells used in the compact popover, adding
rain values without a nested horizontal scroller. Each hour remains fully readable
and the page stays within its actual requested width with enlarged text.
Do not rename a screenshot “narrow” without asserting its real allocation.

Regression tests live in `weather/ui/chart_data.rs` (availability/positions) and
`weather/ui/details_tests.rs` (Cairo raster and opt-in real GTK page/lifetime).
The ignored test must actually run in a private display; its component captures
do not prove the external popover, Wayland, live forecasts or an Orca session.

## Session and power: names before symbols

New launcher configurations use **Session and power** in Grid, Categories, Classic,
Full screen and Overview. It opens a native, visit-owned menu with complete action
names. Lock and Suspend are grouped separately from Log out, Restart and Shut down;
Hibernate is added only after the existing logind capability query allows it.
The query is cancellable and finite. A late capability answer modifies its section,
not the whole action model. Browsing or focusing an item never executes it.

This is the `session-menu` value of `menu.power_button_style`, not the global
**With names** profile. Explicit `icon-only`, `icon-text` and `text-only` choices
remain supported. The menu configurator offers all four presentations in every
layout, including Classic. Favorites, menu layout, search and panel arrangement
are not rewritten when the presentation changes. All variants use the same action
catalog and session controller; there is no new session daemon or Rust ABI.

Escape dismisses the session submenu first and restores focus to its named entry.
The next Escape closes the launcher. A mouse dismissal does not steal another
control's focus. Selecting an action closes the menus before entering the existing
session controller. Its actions for leaving the session retain explicit confirmation
with Cancel as the default; no countdown ever authorizes abandoning open documents.

One question is one operation: another Log out/Restart/Shut down request cannot
replace its target or open a second question. Cancel, destruction of the native
host and stale callbacks invalidate that question. GTK host unrealization matters
here: `GtkWindow.destroy` drops GTK's toplevel reference and can leave other owners
alive. Retire native dialogs outside their response/unrealize stack.

After confirmation, the existing taskbar receives close requests once. Keep waiting
continues observation without sending Close again to an editor already asking about
unsaved work. Each polling source belongs to that particular pending operation and
is removed when it ends; an old source must never adopt a replacement operation.
This does not turn the taskbar protocol into a transactional session manager or
prove shutdown/recovery in a real compositor session.

### Focused verification

`menu/session_controls/tests.rs` covers action membership, saved styles, native
constructors, stale/duplicate activation, and the text/regions of mapped menus.
`session_end/tests.rs` exercises real alert buttons with a private, non-executing
taskbar receiver: it never asks the machine to suspend, log out or power off.
Run the ignored GTK cases explicitly, one per process, in a private D-Bus/display
under a finite supervisor. The X11 keyboard case uses the local `keys.py` fixture
and libXtst to send Space, arrows and Escape. A callback test is not a keyboard test.

Photographs of each menu layout validate component content, not fullscreen behavior
on a real monitor. The separate mapped-popup scene records its native surface
bounds and text regions in light/dark, enlarged text and mirrored layout. Mirrored
English or Portuguese is a geometry check, not validation of an Arabic translation.
GTK/AT-SPI exposure is not an Orca or participant study.

Primary references: [native menu semantics](https://docs.gtk.org/gtk4/class.PopoverMenu.html),
[window lifetime](https://docs.gtk.org/gtk4/method.Window.destroy.html), and
[visible labels](https://www.w3.org/WAI/WCAG2/supplemental/patterns/o4p06-clear-labels/).
