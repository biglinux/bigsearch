# Names on panels

The desktop preference **Show names on panels**, at the top of **Comfort**, adds visible
names to the menu button and inherited panel controls. It is one presentation
setting, not a Look or a replacement of the user's layout.

## Using it

Open **Comfort** from the settings search using “show names”, “icon names”,
“mostrar nomes” or “ícones com texto”. The Accessibility applet also names this
destination. The preview reuses the panel naming adapter and launcher content;
its sample controls cannot change a device. The preview is not a live connection to hardware.

Names appear beside inherited controls on horizontal panels and below them on
vertical panels. Explicit beside/below/text-only choices of a widget remain
unchanged. The panel's own settings offer **Follow the desktop**, **Show names**
and **Do not add names**. Explicit widget text layouts remain visible even when no names are added.
These are per-panel exceptions addressed by stable panel ID,
not by index.

Turning the desktop preference off returns each panel to its own settings; it
does not force every control to hide its name. Legacy beside-icon labels also keep
their original layout on a vertical bar until the new overlay or local override is chosen. A panel that was already labelled
remains labelled. That old widget-only choice does not add a launcher name until
the new desktop option or local panel override is chosen. The usual session-scoped Undo swaps the configuration; there
is no new undo stack, timeout or notification. The native switch and panel exception follow applied
snapshots, so Undo or a reload does not write its readback as a new edit.

This increment does **not** coordinate the separate presentation of application
toolbars, taskbar window titles, third-party tray items, the clock's own date/time
format or the launcher catalog.
Those products retain their existing presentation settings. Desktop file icons
and menu entries already have their own labels. Do not claim that this increment
completes the suite-wide “Com nomes” profile described in
[Big experience 2026](big-experience-2026.md).

## Ownership and persistence

`big-shell-core::config::names` owns the pure resolution rules. The stored
configuration is never replaced with an effective/rendered configuration.
`comfort.show_control_names` defaults to false for existing documents;
`panels[].widget_names_override` defaults to `null` (inherit). The legacy
`show_widget_names` value is preserved as the panel's configured default.

Resolution order is:
1. Explicit widget text layout, or an explicit named launcher layout.
2. The panel's explicit names override, when one is configured.
3. Desktop naming overlay, otherwise the panel's existing saved preference.

The helpers borrow the launcher when no projection is needed. They do not alter
favourites, places, panel order, custom applets, keyboard shortcuts, themes,
density or documents. A newly added panel inherits the overlay without having
its defaults rewritten. Comfort dials and a Look preserve this accessibility
choice. The compositor-facing panel rebuild remains responsible for closing
popovers before rebuilding; this preference creates no additional process.

The structural change must not enter the stylesheet-only fast path.
`take_the_sheet` copies only known dial fields, not all of `ComfortDials`;
otherwise the preference would be saved with no label appearing.

## Names and live status are different

Updates and printers can carry a numeric badge next to their icon. That badge is
not their name. The panel adapter retains the entire original status content and
adds a separate label. Its private widget identity prevents double decoration.
Live icon updates find the existing image inside composite content instead of
replacing the button's child and losing its name/count.

Below-icon labels, including the launcher, wrap rather than abbreviating the distinguishing text. The
same naming adapter handles previews and the live panel. Beside-icon labels still
use the existing compact behavior; crowded bars and every possible custom text
need their own review. This is not a guarantee that arbitrary text fits any width.

## Verification

Run the pure `big-shell-core` `config::names` tests first. Then check/lint the
desktop, and execute the ignored native cases in `settings::comfort::names::tests`
one per process under a private display and a finite timeout. The rendering case
records real requested/allocated dimensions and reuses
`applets::layout_review`; optional `BIG_APPLET_REVIEW_OUTPUT` writes PNGs/JSON.

Tests of the native switch use controlled configuration notifications. They
verify actual row activation, readback without writes, and disposal after the
owner is gone. They are not proof of Wayland panel placement or a complete
screen-reader/hardware test. A rendered preview is not the running full panel;
record that distinction with the screenshots. Panel tests install the actual
generated stylesheet and include floating/shadow margins in the surface width;
the configured strip thickness alone is not the width of its entire window.

The large-text panel test caught an actual width requirement from the launcher
label (119 pixels for a 93-pixel surface). Wrapping fixed the width, but pixel
inspection revealed overlap with the next control. The stricter test now checks
each label inside its own button, not only inside the panel.

GTK 4.22.5 MenuButton reports ConstantSize even for wrapping public content.
`taskbar_panel::named_menu` adds a private height-for-width layout around only
those buttons. It measures the public content and native button envelope; the
original MenuButton retains popup allocation, actions, input and accessibility.
No private GTK child, new timer or GType identity crosses a module interface.
An icon-only or plain button keeps the original path. The bridge follows the
native button's visibility; it is not a second control or name.

The preference section itself can grow vertically; its existing settings-page
scroll container remains necessary. Neither component screenshots nor the X11
panel-content fixture validate Wayland exclusive zones, all custom icons, or
every monitor topology. Enlarge text without shrinking fonts or accepting
overlap just to keep a predetermined row height.

## Primary references

- [Clear visible labels, WAI cognitive guidance](https://www.w3.org/WAI/WCAG2/supplemental/patterns/o4p06-clear-labels/).
- [libadwaita SwitchRow](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/1-latest/class.SwitchRow.html).
- [GTK FlowBox](https://docs.gtk.org/gtk4/class.FlowBox.html).
- [GTK native size request modes](https://docs.gtk.org/gtk4/method.Widget.get_request_mode.html).

The native popup test places and verifies its private X11 toplevel at (0,0).
Without a WM, GTK's RTL right-gravity can grow that window partly offscreen, causing
GDK to clip the popup below its minimum size. The failed fixture and readonly trace
are preserved; no production popup-sizing workaround was retained. Keyboard, popup
mapping and native surface bounds remain explicit assertions.
