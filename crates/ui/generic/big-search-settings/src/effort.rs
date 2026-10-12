// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! How much of the machine the search is allowed to use.
//!
//! Four named amounts of work rather than the eight numbers underneath them.
//! The numbers only make sense together — an index that keeps word frequencies
//! next to a four-megabyte reading limit is a contradiction nobody would pick on
//! purpose — so they are chosen as sets, and each set says what it costs and
//! what it buys.
//!
//! ## When the file is somebody's own mixture
//!
//! Somebody who edited the file by hand, or who opened the advanced section, has
//! values that are not any of the four. Then **no** option is selected and a line
//! says why. A fifth option called "Custom" that cannot be clicked is a control
//! that lies about being one.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use big_search_config::{ContentIndexMode, Defaults, Preset};

use crate::{SearchSettings, msgid, tr};

/// The heading of this group, and what the settings search calls it.
pub(crate) const TITLE: &str = msgid("How hard the computer works");

/// The order of the "Detail kept about each word" choices.
const MODES: [ContentIndexMode; 3] = [
    ContentIndexMode::Auto,
    ContentIndexMode::Basic,
    ContentIndexMode::Freqs,
];

/// The whole section: the four choices, then the advanced part folded away.
///
/// The two halves show the same values: choosing a preset moves the advanced
/// rows to it, and moving a row selects whichever preset the values now are,
/// if any.
pub(crate) struct Effort {
    group: adw::PreferencesGroup,
    choices: Vec<(Preset, gtk::CheckButton)>,
    mode_row: adw::ComboRow,
    extract_row: adw::SpinRow,
    pdf_row: adw::SpinRow,
    /// What the file says once the queued writes reach it.
    defaults: RefCell<Defaults>,
    /// Set while this code moves its own controls, so their handlers do not
    /// take that for the person's choice.
    syncing: Cell<bool>,
}

impl Effort {
    pub(crate) fn new(settings: &Rc<SearchSettings>) -> Rc<Self> {
        let defaults = big_search_config::read().defaults;
        let group = adw::PreferencesGroup::builder().title(tr(TITLE)).build();

        let mut choices = Vec::new();
        for preset in Preset::all() {
            let choice = gtk::CheckButton::new();
            if let Some((_, leader)) = choices.first() {
                choice.set_group(Some(leader));
            }
            let row = adw::ActionRow::builder()
                .title(title_of(preset))
                .subtitle(subtitle_of(preset))
                .activatable_widget(&choice)
                .build();
            row.add_prefix(&choice);
            if preset == Preset::Balanced {
                let recommended = gtk::Label::builder()
                    .label(tr("recommended"))
                    .css_classes(["dim-label", "caption"])
                    .build();
                row.add_suffix(&recommended);
            }
            group.add(&row);
            choices.push((preset, choice));
        }

        // The numbers, folded away. Nobody comes here to set a reading limit in
        // megabytes; the people who do know what these mean can find them, and
        // everybody else never opens the row.
        let expander = adw::ExpanderRow::builder()
            .title(tr("Advanced options"))
            .subtitle(tr("Changing these leaves the choice above"))
            .build();
        let labels = [
            tr("Decided by the computer"),
            tr("Smallest list"),
            tr("Better ordering of results"),
        ];
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mode_row = adw::ComboRow::builder()
            .title(tr("Detail kept about each word"))
            .model(&gtk::StringList::new(&labels))
            .build();
        let extract_row = spin_row(
            &tr("Most text read from one file"),
            &tr("0 lets the computer decide"),
            0.0,
            512.0,
        );
        let pdf_row = spin_row(
            &tr("Longest time spent on one PDF"),
            &tr("In seconds"),
            1.0,
            120.0,
        );
        let cooldown_row = spin_row(
            &tr("Wait before reading a changed file again"),
            &tr("In seconds. Keeps a document being edited from being read on every save."),
            0.0,
            7200.0,
        );
        cooldown_row.set_value(defaults.content_cooldown_secs as f64);
        for row in [
            mode_row.upcast_ref::<adw::PreferencesRow>(),
            extract_row.upcast_ref(),
            pdf_row.upcast_ref(),
            cooldown_row.upcast_ref(),
        ] {
            expander.add_row(row);
        }
        group.add(&expander);

        let effort = Rc::new(Self {
            group,
            choices,
            mode_row,
            extract_row,
            pdf_row,
            defaults: RefCell::new(defaults),
            syncing: Cell::new(false),
        });
        effort.show_rows();
        effort.show_choice();

        for (preset, choice) in &effort.choices {
            let (effort, settings, preset) = (Rc::downgrade(&effort), Rc::clone(settings), *preset);
            choice.connect_toggled(move |choice| {
                let Some(effort) = effort.upgrade() else {
                    return;
                };
                if effort.syncing.get() || !choice.is_active() {
                    return;
                }
                // Written as the whole set, the same values the command line
                // writes for the same name.
                for (key, literal) in preset_values(preset) {
                    settings.set_default(key, &literal);
                }
                big_search_config::apply_preset_in_memory(
                    &mut effort.defaults.borrow_mut(),
                    preset,
                );
                effort.show_rows();
                effort.group.set_description(None);
            });
        }
        {
            let (weak, settings) = (Rc::downgrade(&effort), Rc::clone(settings));
            effort.mode_row.connect_selected_notify(move |row| {
                let Some(effort) = weak.upgrade().filter(|effort| !effort.syncing.get()) else {
                    return;
                };
                let mode = MODES
                    .get(row.selected() as usize)
                    .copied()
                    .unwrap_or_default();
                settings.set_default(
                    "content_index_mode",
                    &big_search_config::string_literal(mode.as_str()),
                );
                effort.defaults.borrow_mut().content_index_mode = mode;
                effort.show_choice();
            });
        }
        connect_number(
            &effort,
            &effort.extract_row,
            settings,
            "extract_max_mb",
            |d| &mut d.extract_max_mb,
        );
        connect_number(
            &effort,
            &effort.pdf_row,
            settings,
            "pdf_timeout_secs",
            |d| &mut d.pdf_timeout_secs,
        );
        connect_number(
            &effort,
            &cooldown_row,
            settings,
            "content_cooldown_secs",
            |d| &mut d.content_cooldown_secs,
        );
        effort
    }

    pub(crate) fn group(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    /// Put the advanced rows at the values the file is getting.
    fn show_rows(&self) {
        let (mode, extract, pdf) = {
            let defaults = self.defaults.borrow();
            (
                defaults.content_index_mode,
                defaults.extract_max_mb,
                defaults.pdf_timeout_secs,
            )
        };
        self.syncing.set(true);
        self.mode_row
            .set_selected(MODES.iter().position(|kept| *kept == mode).unwrap_or(0) as u32);
        self.extract_row.set_value(extract as f64);
        self.pdf_row.set_value(pdf as f64);
        self.syncing.set(false);
    }

    /// Select the preset the values are, or none and a line saying why.
    fn show_choice(&self) {
        let chosen = big_search_config::preset_of(&self.defaults.borrow());
        self.syncing.set(true);
        for (preset, choice) in &self.choices {
            choice.set_active(chosen == Some(*preset));
        }
        self.syncing.set(false);
        self.group.set_description(
            chosen
                .is_none()
                .then(|| tr("Adjusted by hand. Choosing one of these replaces that."))
                .as_deref(),
        );
    }
}

/// The values a preset writes, taken from the shared description so the window
/// and the command line cannot drift.
fn preset_values(preset: Preset) -> Vec<(&'static str, String)> {
    let mut defaults = big_search_config::Defaults::default();
    big_search_config::apply_preset_in_memory(&mut defaults, preset);
    vec![
        ("names_only", bool_literal(defaults.names_only)),
        ("content", bool_literal(defaults.content)),
        ("metadata", bool_literal(defaults.metadata)),
        (
            "content_index_mode",
            big_search_config::string_literal(defaults.content_index_mode.as_str()),
        ),
        ("extract_max_mb", defaults.extract_max_mb.to_string()),
        ("text_max_mb", defaults.text_max_mb.to_string()),
        ("office_max_mb", defaults.office_max_mb.to_string()),
        ("pdf_max_mb", defaults.pdf_max_mb.to_string()),
        ("pdf_timeout_secs", defaults.pdf_timeout_secs.to_string()),
    ]
}

fn bool_literal(value: bool) -> String {
    if value {
        "true".to_owned()
    } else {
        "false".to_owned()
    }
}

fn title_of(preset: Preset) -> String {
    match preset {
        Preset::NamesOnly => tr("Only the names of files"),
        Preset::Light => tr("Light — for a computer with little memory"),
        Preset::Balanced => tr("Balanced"),
        Preset::Complete => tr("Complete — finds more, uses more memory"),
    }
}

fn subtitle_of(preset: Preset) -> String {
    match preset {
        Preset::NamesOnly => {
            tr("Finds a file by its name. It will not find a word written inside a document.")
        }
        Preset::Light => tr("Reads inside files too, keeping the smallest possible list."),
        Preset::Balanced => {
            tr("Reads inside files, and decides the detail by the size of the computer.")
        }
        Preset::Complete => tr("Reads more of each file and remembers more about it."),
    }
}

fn spin_row(title: &str, subtitle: &str, lowest: f64, highest: f64) -> adw::SpinRow {
    adw::SpinRow::builder()
        .title(title)
        .subtitle(subtitle)
        .adjustment(&gtk::Adjustment::new(
            lowest, lowest, highest, 1.0, 10.0, 0.0,
        ))
        .build()
}

/// Write `key` when the person moves `row`, and keep the preset choice in step.
fn connect_number(
    effort: &Rc<Effort>,
    row: &adw::SpinRow,
    settings: &Rc<SearchSettings>,
    key: &'static str,
    field: fn(&mut Defaults) -> &mut u64,
) {
    let (effort, settings): (Weak<Effort>, _) = (Rc::downgrade(effort), Rc::clone(settings));
    row.connect_value_notify(move |row| {
        let Some(effort) = effort.upgrade().filter(|effort| !effort.syncing.get()) else {
            return;
        };
        let value = row.value() as u64;
        settings.set_default(key, &value.to_string());
        *field(&mut effort.defaults.borrow_mut()) = value;
        effort.show_choice();
    });
}
