// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! Which folders the computer looks in.
//!
//! The one setting people actually come here for: "why doesn't it find the
//! photos on my external disk?". Each folder is a row with a switch for reading
//! the text inside its files, and a menu to take it out again.
//!
//! ## The empty file means the home folder
//!
//! A configuration with no folders in it means "my home folder", and the list
//! says so rather than showing nothing. Writing the first folder writes the home
//! folder with it — [`big_search_config::add_source`] does that, so adding an
//! external disk cannot quietly remove the person's own files from the search.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{SearchSettings, msgid, tr};

/// The heading of this group, and what the settings search calls it.
pub(crate) const TITLE: &str = msgid("Where to look");

/// The list of catalogued folders.
pub(crate) struct Folders {
    group: adw::PreferencesGroup,
    rows: RefCell<Vec<adw::ActionRow>>,
    settings: RefCell<Option<std::rc::Weak<SearchSettings>>>,
    /// Which folders the service says it is actually catalogueing right now, so
    /// a disk that is unplugged can be shown as unplugged.
    available: RefCell<Vec<PathBuf>>,
}

impl Folders {
    pub(crate) fn new(settings: &Rc<SearchSettings>) -> Rc<Self> {
        let group = adw::PreferencesGroup::builder()
            .title(tr(TITLE))
            .description(tr(
                "The computer keeps a ready list of what is in these folders, so searching them is instant.",
            ))
            .build();
        // A row of its own at the end of the list, not a button in the heading:
        // beside a two-line explanation the heading suffix sits on top of the
        // words, and the person reads a button through a sentence.
        let add = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("list-add-symbolic")
                    .label(tr("Add a folder"))
                    .halign(gtk::Align::Center)
                    .build(),
            )
            .margin_top(6)
            .build();

        let folders = Rc::new(Self {
            group,
            rows: RefCell::new(Vec::new()),
            settings: RefCell::new(Some(Rc::downgrade(settings))),
            available: RefCell::new(Vec::new()),
        });

        {
            let folders = Rc::clone(&folders);
            add.connect_clicked(move |button| folders.choose_folder(button));
        }
        folders.reload();
        folders.group.add(&add);
        folders.refresh_availability();
        folders
    }

    pub(crate) fn group(&self) -> &adw::PreferencesGroup {
        &self.group
    }

    fn settings(&self) -> Option<Rc<SearchSettings>> {
        self.settings
            .borrow()
            .as_ref()
            .and_then(std::rc::Weak::upgrade)
    }

    /// Build the rows from what the file says.
    fn reload(self: &Rc<Self>) {
        // Also puts back the explanation, over whatever refusal was last shown
        // in its place.
        self.group.set_description(Some(&tr(
            "The computer keeps a ready list of what is in these folders, so searching them is instant.",
        )));
        for row in self.rows.borrow_mut().drain(..) {
            self.group.remove(&row);
        }
        let config = big_search_config::read();
        let listed: Vec<big_search_config::SourceEntry> = if config.sources.is_empty() {
            // Nothing written means the home folder, and the list has to show
            // the person what is happening rather than an empty box.
            vec![big_search_config::SourceEntry::new(
                big_search_config::home_dir(),
            )]
        } else {
            config.sources.clone()
        };
        let reads_content_by_default = config.defaults.content && !config.defaults.names_only;
        for source in listed {
            let row = self.build_row(&source, reads_content_by_default);
            self.group.add(&row);
            self.rows.borrow_mut().push(row);
        }
    }

    fn build_row(
        self: &Rc<Self>,
        source: &big_search_config::SourceEntry,
        reads_content_by_default: bool,
    ) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&folder_name(source)))
            .subtitle(glib::markup_escape_text(&subtitle_for(source)))
            .build();
        // The row carries its own folder, so the answer about which disks are
        // plugged in — which arrives later, from the service — knows which row
        // it is talking about.
        row.set_widget_name(&source.path.to_string_lossy());

        let inside = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .active(source.content.unwrap_or(reads_content_by_default))
            .tooltip_text(tr("Also look at the text inside the files"))
            .build();
        inside.update_property(&[gtk::accessible::Property::Label(&tr(
            "Also look at the text inside the files",
        ))]);
        {
            let folders = Rc::clone(self);
            let path = source.path.clone();
            inside.connect_state_set(move |_, state| {
                if let Err(error) = big_search_config::set_source_content(&path, state) {
                    log::warn!("the folder setting could not be written: {error}");
                } else if let Some(settings) = folders.settings() {
                    settings.folders_changed();
                }
                glib::Propagation::Proceed
            });
        }
        row.add_suffix(&inside);

        let remove = gtk::Button::builder()
            .icon_name("list-remove-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .tooltip_text(tr("Take this folder out of the search"))
            .build();
        remove.update_property(&[gtk::accessible::Property::Label(&tr(
            "Take this folder out of the search",
        ))]);
        {
            let folders = Rc::clone(self);
            let path = source.path.clone();
            remove.connect_clicked(move |_| folders.remove_folder(&path));
        }
        row.add_suffix(&remove);
        row
    }

    /// Ask the service which of these folders it can reach, so an unplugged
    /// disk says so instead of looking like it stopped working.
    fn refresh_availability(self: &Rc<Self>) {
        let folders = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let roots = gio::spawn_blocking(|| {
                big_indexd_client::Client::connect_default()
                    .and_then(|client| client.sources().ok())
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            let Some(folders) = folders.upgrade() else {
                return;
            };
            *folders.available.borrow_mut() = roots
                .iter()
                .filter(|root| root.available)
                .map(|root| PathBuf::from(&root.path))
                .collect();
            folders.mark_unavailable();
        });
    }

    /// Say, on each row, whether the folder is reachable right now.
    fn mark_unavailable(&self) {
        let available = self.available.borrow();
        if available.is_empty() {
            return;
        }
        for row in self.rows.borrow().iter() {
            let path = PathBuf::from(row.widget_name().to_string());
            if path.as_os_str().is_empty() {
                continue;
            }
            if !available.iter().any(|root| path.starts_with(root)) {
                row.set_subtitle(&glib::markup_escape_text(&tr(
                    "Not connected right now. What was found before stays in the search.",
                )));
            }
        }
    }

    fn choose_folder(self: &Rc<Self>, anchor: &gtk::Button) {
        let dialog = gtk::FileDialog::builder()
            .title(tr("Choose a folder to search"))
            .modal(true)
            .build();
        let window = anchor.root().and_downcast::<gtk::Window>();
        let folders = Rc::clone(self);
        dialog.select_folder(window.as_ref(), gio::Cancellable::NONE, move |chosen| {
            let Ok(file) = chosen else { return };
            let Some(path) = file.path() else { return };
            folders.add_folder(&path);
        });
    }

    fn add_folder(self: &Rc<Self>, path: &Path) {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        // A folder already inside another catalogued folder adds nothing and
        // would read as two places to turn the same thing on and off.
        let config = big_search_config::read();
        let covered = if config.sources.is_empty() {
            path.starts_with(big_search_config::home_dir())
        } else {
            config
                .sources
                .iter()
                .any(|source| path.starts_with(&source.path))
        };
        if covered {
            self.say(&tr("That folder is already inside one being searched."));
            return;
        }
        let mut entry = big_search_config::SourceEntry::new(path.clone());
        // A disk that comes and goes is recognised, not asked about: the person
        // knows they plugged in a pendrive, not what "removable source" means.
        entry.removable = is_removable(&path);
        entry.network = is_network(&path);
        if let Err(error) = big_search_config::add_source(&entry) {
            log::warn!("the folder could not be added: {error}");
            self.say(&tr("That folder could not be added."));
            return;
        }
        self.reload();
        if let Some(settings) = self.settings() {
            settings.folders_changed();
        }
    }

    fn remove_folder(self: &Rc<Self>, path: &Path) {
        // The home folder is what the service falls back to when the list is
        // empty, so removing the last folder would put it straight back.
        let config = big_search_config::read();
        if config.sources.len() <= 1 {
            self.say(&tr(
                "At least one folder has to be searched, so this one stays.",
            ));
            return;
        }
        if let Err(error) = big_search_config::remove_source(path) {
            log::warn!("the folder could not be removed: {error}");
            return;
        }
        self.reload();
        if let Some(settings) = self.settings() {
            settings.folders_changed();
        }
    }

    /// Say something that refused to happen, where the person is looking.
    fn say(&self, message: &str) {
        self.group.set_description(Some(message));
    }
}

/// The name to show for a folder: what the person called it, else the folder's
/// own name.
fn folder_name(source: &big_search_config::SourceEntry) -> String {
    if let Some(name) = &source.name {
        return name.clone();
    }
    if source.path == big_search_config::home_dir() {
        return tr("Home folder");
    }
    source
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.path.to_string_lossy().into_owned())
}

/// What to write under a folder's name: where it is, unless that would repeat
/// the name back.
fn subtitle_for(source: &big_search_config::SourceEntry) -> String {
    let path = friendly_path(&source.path);
    if path == folder_name(source) {
        // The home folder is called "Home folder" and lives at "Home folder";
        // saying it twice tells nobody anything.
        return String::new();
    }
    path
}

/// A path with the home folder written the way it is spoken.
fn friendly_path(path: &Path) -> String {
    let home = big_search_config::home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => tr("Home folder"),
        Ok(rest) => format!("{} › {}", tr("Home folder"), rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Where the system mounts disks that come and go.
fn is_removable(path: &Path) -> bool {
    path.starts_with("/run/media") || path.starts_with("/media") || path.starts_with("/mnt/usb")
}

/// Where the system mounts other machines' folders.
fn is_network(path: &Path) -> bool {
    path.starts_with("/run/user")
        && path
            .components()
            .any(|part| part.as_os_str() == "gvfs" || part.as_os_str() == "doc")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disk plugged into the machine is recognised as one without asking.
    #[test]
    fn a_plugged_in_disk_is_recognised() {
        assert!(is_removable(Path::new("/run/media/bruno/BACKUP")));
        assert!(is_removable(Path::new("/media/usb0")));
        assert!(!is_removable(Path::new("/home/ana/Documentos")));
        assert!(!is_network(Path::new("/home/ana")));
    }
}
