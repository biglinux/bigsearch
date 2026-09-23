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

use std::rc::Rc;

use adw::prelude::*;
use big_search_config::{ContentIndexMode, Preset};

use crate::{SearchSettings, msgid, tr};

/// The heading of this group, and what the settings search calls it.
pub(crate) const TITLE: &str = msgid("How hard the computer works");

/// The whole section: the four choices, then the advanced part folded away.
pub(crate) fn group(settings: &Rc<SearchSettings>) -> adw::PreferencesGroup {
    let config = big_search_config::read();
    let chosen = big_search_config::preset_of(&config.defaults);

    let group = adw::PreferencesGroup::builder().title(tr(TITLE)).build();
    if chosen.is_none() {
        group.set_description(Some(&tr(
            "Adjusted by hand in the configuration file. Choosing one of these replaces that.",
        )));
    }

    let mut first: Option<gtk::CheckButton> = None;
    for preset in Preset::all() {
        let choice = gtk::CheckButton::new();
        match &first {
            Some(leader) => choice.set_group(Some(leader)),
            None => first = Some(choice.clone()),
        }
        choice.set_active(chosen == Some(preset));
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
        {
            let settings = Rc::clone(settings);
            let group = group.clone();
            choice.connect_toggled(move |choice| {
                if !choice.is_active() {
                    return;
                }
                // Written as the whole set, the same values the command line
                // writes for the same name.
                for (key, literal) in preset_values(preset) {
                    settings.set_default(key, &literal);
                }
                group.set_description(None);
            });
        }
        group.add(&row);
    }

    group.add(&advanced(settings, &config.defaults));
    group
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

/// The numbers, folded away.
///
/// Nobody comes here to set a reading limit in megabytes; the people who do know
/// what these mean can find them, and everybody else never opens the row.
fn advanced(
    settings: &Rc<SearchSettings>,
    defaults: &big_search_config::Defaults,
) -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder()
        .title(tr("Advanced options"))
        .subtitle(tr("Changing these leaves the choice above"))
        .build();

    let modes = [
        (ContentIndexMode::Auto, tr("Decided by the computer")),
        (ContentIndexMode::Basic, tr("Smallest list")),
        (ContentIndexMode::Freqs, tr("Better ordering of results")),
    ];
    let labels: Vec<&str> = modes.iter().map(|(_, label)| label.as_str()).collect();
    let mode_row = adw::ComboRow::builder()
        .title(tr("Detail kept about each word"))
        .model(&gtk::StringList::new(&labels))
        .selected(
            modes
                .iter()
                .position(|(mode, _)| *mode == defaults.content_index_mode)
                .unwrap_or(0) as u32,
        )
        .build();
    {
        let settings = Rc::clone(settings);
        mode_row.connect_selected_notify(move |row| {
            let mode = match row.selected() {
                1 => ContentIndexMode::Basic,
                2 => ContentIndexMode::Freqs,
                _ => ContentIndexMode::Auto,
            };
            settings.set_default(
                "content_index_mode",
                &big_search_config::string_literal(mode.as_str()),
            );
        });
    }
    expander.add_row(&mode_row);

    expander.add_row(&number_row(
        settings,
        "extract_max_mb",
        &tr("Most text read from one file"),
        &tr("0 lets the computer decide"),
        defaults.extract_max_mb,
        0.0,
        512.0,
    ));
    expander.add_row(&number_row(
        settings,
        "pdf_timeout_secs",
        &tr("Longest time spent on one PDF"),
        &tr("In seconds"),
        defaults.pdf_timeout_secs,
        1.0,
        120.0,
    ));
    expander.add_row(&number_row(
        settings,
        "content_cooldown_secs",
        &tr("Wait before reading a changed file again"),
        &tr("In seconds. Keeps a document being edited from being read on every save."),
        defaults.content_cooldown_secs,
        0.0,
        7200.0,
    ));
    expander
}

fn number_row(
    settings: &Rc<SearchSettings>,
    key: &'static str,
    title: &str,
    subtitle: &str,
    value: u64,
    lowest: f64,
    highest: f64,
) -> adw::SpinRow {
    let row = adw::SpinRow::builder()
        .title(title)
        .subtitle(subtitle)
        .adjustment(&gtk::Adjustment::new(
            value as f64,
            lowest,
            highest,
            1.0,
            10.0,
            0.0,
        ))
        .build();
    let settings = Rc::clone(settings);
    row.connect_value_notify(move |row| {
        settings.set_default(key, &(row.value() as u64).to_string());
    });
    row
}
