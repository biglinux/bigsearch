// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! The window every BigLinux program opens to configure the system search.
//!
//! The search service catalogues the person's files and answers the file
//! manager, the menu and anything else that asks. Until now it could only be
//! configured by editing a text file, which for the people this desktop is built
//! for means it could not be configured at all.
//!
//! One window, opened in whichever program the person is already in:
//!
//! ```no_run
//! big_search_settings::present(None, "bigfiles");
//! ```
//!
//! ## Why a library and not a program of its own
//!
//! A separate application would need its own package, icon, desktop entry and
//! translation catalogue, and every program wanting to offer the setting would
//! have to launch it and hope it is installed. As a library, the file manager
//! and the shell open the same window in their own process, and the next
//! program to want it adds one line.
//!
//! ## What it changes, and when
//!
//! Every control writes `~/.config/big-search/config.toml` through
//! [`big_search_config`] two seconds after the last change. The service reads
//! that file once when it starts, so a change only takes effect on a restart —
//! and a restart re-reads every catalogued folder and drops the socket for a
//! moment, which is why this window restarts it **once**, when it is closed or
//! after ten seconds of quiet, and only when a setting the service reads
//! actually changed.

mod effort;
mod folders;
mod privacy;
mod status;
mod versions;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

/// Space between groups when the controls are embedded in another window, which
/// is what `AdwPreferencesPage` puts between its own.
const GROUP_SPACING: i32 = 24;

/// One row of this page, as another program's settings search needs it.
///
/// The words come from here because the groups are built here: the file manager
/// used to retype the four titles, and renaming a group left its settings search
/// advertising a heading that no longer existed.
pub struct SearchEntry {
    /// The widget name to scroll to, one of the `ANCHOR_*` values.
    pub anchor: &'static str,
    /// The group's heading, as it reads on the page.
    pub title: String,
    /// One line saying what the group decides.
    pub subtitle: String,
    /// Extra words somebody might type looking for this, already translated.
    pub keywords: String,
}

/// What a settings window should offer when somebody searches its own settings.
///
/// Takes the text domain like [`page`] does, and for the same reason: the list
/// is built before the page exists, so nothing has told this crate which
/// catalogue to read yet.
#[must_use]
pub fn search_entries(text_domain: &str) -> Vec<SearchEntry> {
    set_text_domain(text_domain);
    vec![
        SearchEntry {
            anchor: ANCHOR_FOLDERS,
            title: tr(folders::TITLE),
            subtitle: tr("Which folders the computer keeps a ready list of"),
            keywords: tr("folders search index catalogue"),
        },
        SearchEntry {
            anchor: ANCHOR_EFFORT,
            title: tr(effort::TITLE),
            subtitle: tr("Names only, or also what is written inside the files"),
            keywords: tr("memory effort content index"),
        },
        SearchEntry {
            anchor: ANCHOR_ORIGIN,
            title: tr(privacy::TITLE),
            subtitle: tr("Remembering how a file arrived, and reading the browser's download list"),
            keywords: tr("origin downloads browser privacy"),
        },
        SearchEntry {
            anchor: ANCHOR_VERSIONS,
            title: tr(versions::TITLE),
            subtitle: tr("Keeping what a document used to be, so you can go back"),
            keywords: tr("versions history undo documents git"),
        },
    ]
}

/// Names the embedding window can point its own search results at.
///
/// A settings window that answers "no matching setting" while the section is
/// sitting in its sidebar is worse than one with no search: the person
/// concludes the setting does not exist. The host declares what it wants found
/// and each name here is on the group it should scroll to.
pub const ANCHOR_FOLDERS: &str = "big-search-folders";
/// The group about how much work the computer does.
pub const ANCHOR_EFFORT: &str = "big-search-effort";
/// The group about where files came from, and the browser download list.
pub const ANCHOR_ORIGIN: &str = "big-search-origin";
/// The group about keeping earlier versions of documents.
pub const ANCHOR_VERSIONS: &str = "big-search-versions";
/// How long after the last change the file is written.
const WRITE_AFTER: Duration = Duration::from_secs(2);
/// How long a written change waits for the person to finish before the service
/// is restarted, when they leave the window open.
const RESTART_AFTER: Duration = Duration::from_secs(10);
/// How often the window asks the service how it is doing.
const POLL_EVERY: Duration = Duration::from_secs(2);

thread_local! {
    /// The window this process has open, so a second request raises it instead
    /// of stacking a copy on top.
    static OPEN_WINDOW: RefCell<Option<glib::WeakRef<adw::Window>>> =
        const { RefCell::new(None) };
    static TEXT_DOMAIN: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Read this crate's visible strings from the calling product's catalogue.
///
/// `textdomain` is process-global and belongs to whoever booted the process, so
/// in a host that runs several products a plain `gettext` here answers from the
/// wrong catalogue — every row in English inside a translated window. The domain
/// is an argument of [`present`] rather than a separate call somebody has to
/// remember to make: a window opened with the wrong words is exactly what a
/// forgettable setup call produces, and it went unnoticed until the window was
/// opened from the installed multicall host.
fn set_text_domain(domain: &str) {
    TEXT_DOMAIN.with_borrow_mut(|slot| *slot = Some(domain.to_owned()));
}

/// Mark a literal for extraction without translating it here.
///
/// The group titles are constants, used both to draw the heading and to tell the
/// embedding window what to call the group — and `xgettext` only ever sees
/// literals. Without this the four titles reach no catalogue at all, and the
/// settings search shows them in English inside a translated window.
pub(crate) const fn msgid(text: &'static str) -> &'static str {
    text
}

/// Translate one of this crate's strings.
pub(crate) fn tr(msgid: &str) -> String {
    if msgid.is_empty() {
        return String::new();
    }
    TEXT_DOMAIN.with_borrow(|domain| match domain {
        Some(domain) => gettextrs::dgettext(domain, msgid),
        None => gettextrs::gettext(msgid),
    })
}

/// Open the search settings, or raise the window this process already has open.
///
/// `parent` is the window the person is in, so the settings sit over it and
/// close with it. `text_domain` is the calling product's gettext domain, which
/// is where this window reads its own words from.
pub fn present(parent: Option<&gtk::Window>, text_domain: &str) {
    set_text_domain(text_domain);
    if let Some(open) =
        OPEN_WINDOW.with_borrow(|slot| slot.as_ref().and_then(glib::WeakRef::upgrade))
    {
        open.present();
        return;
    }
    let window = build(parent);
    OPEN_WINDOW.with_borrow_mut(|slot| *slot = Some(window.downgrade()));
    window.present();
}

/// Everything the window's parts share.
pub(crate) struct SearchSettings {
    /// Which settings have been changed but not yet written.
    pending_write: RefCell<Vec<(String, String)>>,
    /// Whether a change the service actually reads is waiting for a restart.
    restart_wanted: Cell<bool>,
    write_timer: RefCell<Option<glib::SourceId>>,
    restart_timer: RefCell<Option<glib::SourceId>>,
    /// The status card, which is also where "applying…" is said.
    status: status::StatusCard,
    /// The folder list, held here for as long as the window lives. Its rows are
    /// rebuilt from the file after every change, and the widgets alone would not
    /// keep the thing that rebuilds them alive.
    folders: RefCell<Option<Rc<folders::Folders>>>,
}

impl SearchSettings {
    /// Ask for `key = literal` in `[defaults]`, written after a short pause.
    pub(crate) fn set_default(self: &Rc<Self>, key: &str, literal: &str) {
        self.queue(key.to_owned(), literal.to_owned());
    }

    /// Ask for `key = literal` in `[origin]`, written after a short pause.
    pub(crate) fn set_origin(self: &Rc<Self>, key: &str, literal: &str) {
        self.queue(format!("origin.{key}"), literal.to_owned());
    }

    /// Ask for `key = literal` in `[history]`, written after a short pause.
    pub(crate) fn set_history(self: &Rc<Self>, key: &str, literal: &str) {
        self.queue(format!("history.{key}"), literal.to_owned());
    }

    /// Say that the catalogued folders changed, so the service has to be
    /// restarted even though this window wrote no `[defaults]` key.
    pub(crate) fn folders_changed(self: &Rc<Self>) {
        self.restart_wanted.set(true);
        self.status.show_pending_restart();
        self.arm_restart();
    }

    fn queue(self: &Rc<Self>, key: String, literal: String) {
        {
            let mut pending = self.pending_write.borrow_mut();
            pending.retain(|(kept, _)| *kept != key);
            pending.push((key, literal));
        }
        if let Some(timer) = self.write_timer.borrow_mut().take() {
            timer.remove();
        }
        let settings = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(WRITE_AFTER, move || {
            if let Some(settings) = settings.upgrade() {
                settings.write_timer.borrow_mut().take();
                settings.write_now();
            }
        });
        *self.write_timer.borrow_mut() = Some(timer);
    }

    /// Write everything waiting, and remember that the service is now behind.
    fn write_now(self: &Rc<Self>) {
        let pending = std::mem::take(&mut *self.pending_write.borrow_mut());
        if pending.is_empty() {
            return;
        }
        let (blocks, defaults): (Vec<_>, Vec<_>) = pending
            .iter()
            .partition(|(key, _)| key.starts_with("origin.") || key.starts_with("history."));
        let (origin, history): (Vec<_>, Vec<_>) = blocks
            .into_iter()
            .partition(|(key, _)| key.starts_with("origin."));
        let defaults: Vec<(&str, &str)> = defaults
            .iter()
            .map(|(key, literal)| (key.as_str(), literal.as_str()))
            .collect();
        let origin: Vec<(&str, &str)> = origin
            .iter()
            .map(|(key, literal)| (key.strip_prefix("origin.").unwrap_or(key), literal.as_str()))
            .collect();
        let history: Vec<(&str, &str)> = history
            .iter()
            .map(|(key, literal)| {
                (
                    key.strip_prefix("history.").unwrap_or(key),
                    literal.as_str(),
                )
            })
            .collect();
        if !defaults.is_empty()
            && let Err(error) = big_search_config::set_defaults(&defaults)
        {
            log::warn!("search settings could not be written: {error}");
            self.status.show_write_failed();
            return;
        }
        if !origin.is_empty()
            && let Err(error) = big_search_config::set_origin(&origin)
        {
            log::warn!("search settings could not be written: {error}");
            self.status.show_write_failed();
            return;
        }
        if !history.is_empty()
            && let Err(error) = big_search_config::set_history(&history)
        {
            log::warn!("search settings could not be written: {error}");
            self.status.show_write_failed();
            return;
        }
        self.restart_wanted.set(true);
        self.status.show_pending_restart();
        self.arm_restart();
    }

    /// Restart the service after a quiet spell, so a person changing four
    /// things in a row costs one restart rather than four.
    fn arm_restart(self: &Rc<Self>) {
        if let Some(timer) = self.restart_timer.borrow_mut().take() {
            timer.remove();
        }
        let settings = Rc::downgrade(self);
        let timer = glib::timeout_add_local_once(RESTART_AFTER, move || {
            if let Some(settings) = settings.upgrade() {
                settings.restart_timer.borrow_mut().take();
                settings.apply_now();
            }
        });
        *self.restart_timer.borrow_mut() = Some(timer);
    }

    /// Write anything still waiting and restart the service, now.
    pub(crate) fn apply_now(self: &Rc<Self>) {
        if let Some(timer) = self.write_timer.borrow_mut().take() {
            timer.remove();
        }
        self.write_now();
        if let Some(timer) = self.restart_timer.borrow_mut().take() {
            timer.remove();
        }
        if !self.restart_wanted.replace(false) {
            return;
        }
        self.status.show_applying();
        restart_service();
    }
}

/// Ask systemd to restart the search service, if it is running at all.
///
/// `try-restart` rather than `restart`: somebody who has the service switched
/// off asked for a setting to be saved, not for the service to be started behind
/// their back. Starting it is its own button on the status card.
fn restart_service() {
    if let Err(error) = systemctl(&["try-restart", "big-search.service"]) {
        log::warn!("the search service could not be restarted: {error}");
    }
}

/// Run `systemctl --user` with a fixed argv, without waiting for it: GIO reaps
/// the child from the main loop, so nothing blocks and nothing is left behind.
pub(crate) fn systemctl(args: &[&str]) -> Result<(), glib::Error> {
    let argv: Vec<&std::ffi::OsStr> = ["systemctl", "--user"]
        .iter()
        .chain(args)
        .map(std::ffi::OsStr::new)
        .collect();
    gtk::gio::Subprocess::newv(&argv, gtk::gio::SubprocessFlags::NONE).map(drop)
}

/// The same controls as [`present`], as a page to put inside another program's
/// settings window.
///
/// A window of its own is right when the person is in the middle of searching
/// and asks "why is this folder slow?"; it is the wrong answer for the person
/// who opened Preferences looking for the search. Both exist, and both write the
/// same file. Everything pending is written when the page leaves the screen, so
/// closing the settings window that holds it loses nothing.
pub fn page(text_domain: &str) -> gtk::Widget {
    set_text_domain(text_domain);
    // A plain box, not an `AdwPreferencesPage`: that widget brings its own
    // scrolling, and inside the settings window's scroller it is squeezed into
    // one screenful with everything below it — privacy, kept versions — out of
    // reach and no way to scroll to them.
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(GROUP_SPACING)
        .build();
    let (groups, settings) = build_groups();
    for group in &groups {
        page.append(group);
    }
    // Off the screen — the window closed, or another section was chosen — is
    // where pending changes are written and the service is restarted, and the
    // moment to stop asking the service how it is doing. Nothing is restarted
    // unless a setting the service reads actually changed.
    {
        let settings = Rc::clone(&settings);
        page.connect_unmap(move |_| {
            settings.apply_now();
            settings.status.stop_polling();
        });
    }
    page.connect_map(move |_| settings.status.start_polling());
    page.upcast()
}

fn named(group: adw::PreferencesGroup, name: &str) -> adw::PreferencesGroup {
    group.set_widget_name(name);
    group
}

/// The controls, in the order they are read, and the state they share.
fn build_groups() -> (Vec<adw::PreferencesGroup>, Rc<SearchSettings>) {
    let status = status::StatusCard::new();

    let settings = Rc::new(SearchSettings {
        pending_write: RefCell::new(Vec::new()),
        restart_wanted: Cell::new(false),
        write_timer: RefCell::new(None),
        restart_timer: RefCell::new(None),
        status,
        folders: RefCell::new(None),
    });

    let folders = folders::Folders::new(&settings);
    *settings.folders.borrow_mut() = Some(Rc::clone(&folders));

    let groups = vec![
        settings.status.group().clone(),
        named(folders.group().clone(), ANCHOR_FOLDERS),
        named(effort::group(&settings), ANCHOR_EFFORT),
        named(privacy::group(&settings), ANCHOR_ORIGIN),
        named(versions::group(&settings), ANCHOR_VERSIONS),
    ];
    (groups, settings)
}

fn build(parent: Option<&gtk::Window>) -> adw::Window {
    let (groups, settings) = build_groups();
    let page = adw::PreferencesPage::new();
    for group in &groups {
        page.add(group);
    }
    let status = settings.status.clone();

    let header = adw::HeaderBar::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&page));

    let window = adw::Window::builder()
        .title(tr("System search"))
        .default_width(600)
        .default_height(760)
        .content(&view)
        .build();
    if let Some(parent) = parent {
        window.set_transient_for(Some(parent));
    }

    // Everything pending is written and applied when the window closes: a person
    // who changes a setting and shuts the window immediately must not lose it,
    // and the service is restarted once rather than after every switch.
    {
        let settings = Rc::clone(&settings);
        window.connect_close_request(move |_| {
            settings.apply_now();
            settings.status.stop_polling();
            OPEN_WINDOW.with_borrow_mut(|slot| *slot = None);
            glib::Propagation::Proceed
        });
    }

    status.start_polling();
    window
}
