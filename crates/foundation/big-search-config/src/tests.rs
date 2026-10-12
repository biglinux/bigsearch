// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

use super::*;

/// A machine nobody has configured shows the recommended amount of work.
#[test]
fn untouched_machine_is_balanced() {
    assert_eq!(preset_of(&Defaults::default()), Some(Preset::Balanced));
}

/// Each preset recognises itself once written.
#[test]
fn every_preset_recognises_its_own_values() {
    for preset in Preset::all() {
        let mut defaults = Defaults::default();
        apply_to(&mut defaults, preset);
        assert_eq!(preset_of(&defaults), Some(preset), "{}", preset.as_str());
    }
}

/// A value nobody's preset writes is a mixture, and says so.
#[test]
fn custom_values_read_as_mixture() {
    let defaults = Defaults {
        extract_max_mb: 7,
        ..Defaults::default()
    };
    assert_eq!(preset_of(&defaults), None);
}

/// Changing a setting keeps the comments and the layout around it.
#[test]
fn edit_keeps_comments_and_layout() {
    let mut lines: Vec<String> =
        "# mine\n[defaults]\ncontent = true          # keep this\nmetadata = false\n"
            .lines()
            .map(String::from)
            .collect();
    set_in_table(&mut lines, "[defaults]", &[("content", "false")]);
    let text = lines.join("\n");
    assert!(text.starts_with("# mine\n"), "{text}");
    assert!(text.contains("content = false      # keep this"), "{text}");
    assert!(text.contains("metadata = false"), "{text}");
}

/// A key the file does not have yet is added inside its own table.
#[test]
fn missing_key_joins_its_table() {
    let mut lines: Vec<String> = "[defaults]\ncontent = true\n"
        .lines()
        .map(String::from)
        .collect();
    set_in_table(&mut lines, "[origin]", &[("browser_history", "true")]);
    let text = lines.join("\n");
    assert!(text.contains("[origin]"), "{text}");
    assert!(text.contains("browser_history = true"), "{text}");
    let parsed = read_text(&text);
    assert!(parsed.origin.browser_history);
    assert!(parsed.defaults.content);
}

/// A source block is found by its path, and taken out whole.
#[test]
fn folder_removal_drops_whole_block() {
    let text = "[defaults]\ncontent = true\n\n[[source]]\npath = \"/home/p\"\n\n[[source]]\npath = \"/mnt/disk\"\nname = \"Disk\"\nremovable = true\n";
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let block = find_source(&lines, Path::new("/mnt/disk")).expect("block is there");
    lines.drain(block);
    let left = read_text(&lines.join("\n"));
    assert_eq!(left.sources.len(), 1);
    assert_eq!(left.sources[0].path, PathBuf::from("/home/p"));
    assert!(left.defaults.content);
}

/// Reading the text inside one folder is turned off without touching the
/// others.
#[test]
fn one_folder_stops_being_read_inside() {
    let text = "[[source]]\npath = \"/home/p\"\n\n[[source]]\npath = \"/mnt/disk\"\n";
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let block = find_source(&lines, Path::new("/mnt/disk")).expect("block is there");
    let (start, end) = (block.start, block.end);
    upsert_line(&mut lines, start + 1, end, "content", "false");
    let parsed = read_text(&lines.join("\n"));
    assert_eq!(parsed.sources[0].content, None);
    assert_eq!(parsed.sources[1].content, Some(false));
}

/// A file that is not TOML at all keeps the search running, with nothing
/// recorded about the person.
#[test]
fn a_broken_file_turns_privacy_features_off() {
    let parsed = read_text("this is not = = toml [[[");
    assert!(parsed.sources.is_empty());
    assert_eq!(parsed.defaults, Defaults::default());
    assert!(!parsed.origin.enabled);
    assert!(!parsed.origin.browser_history);
    assert!(!parsed.history.enabled);
}

/// One bad value loses its own table, not the rest of the file.
#[test]
fn a_bad_value_keeps_the_valid_tables() {
    let text = "[defaults]\ncontent = false\n\n[origin]\nenabled = true\n\n\
                [history]\nkeep_days = \"30\"\n\n[[source]]\npath = \"/mnt/data\"\n";
    assert!(parse(text).is_err());
    let parsed = read_text(text);
    assert!(!parsed.defaults.content);
    assert!(parsed.origin.enabled);
    assert!(!parsed.history.enabled);
    assert_eq!(parsed.sources, vec![SourceEntry::new("/mnt/data")]);
}

/// A header with a comment after it is still the same table.
#[test]
fn commented_headers_are_recognised() {
    let text = "[defaults]  # mine\ncontent = true\n\n[[source]] # usb\npath = \"/mnt/disk\"\n";
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    set_in_table(&mut lines, "[defaults]", &[("metadata", "true")]);
    let edited = lines.join("\n");
    assert_eq!(edited.matches("[defaults]").count(), 1, "{edited}");
    let parsed = parse(&edited).expect("still TOML");
    assert!(parsed.defaults.metadata);
    assert_eq!(parsed.sources[0].metadata, None);
    assert!(find_source(&lines, Path::new("/mnt/disk")).is_some());
}

/// A path is matched by its value, however the file spells it.
#[test]
fn folder_found_by_path_value() {
    for (line, path) in [
        ("path = \"/mnt/disk\"  # USB", "/mnt/disk"),
        ("path = '/mnt/disk'", "/mnt/disk"),
        ("path = \"/mnt/a#b\"", "/mnt/a#b"),
        (
            &format!("path = {}", string_literal(r#"/mnt/a"b\c"#)),
            r#"/mnt/a"b\c"#,
        ),
    ] {
        let lines = vec!["[[source]]".to_owned(), line.to_owned()];
        assert_eq!(find_source(&lines, Path::new(path)), Some(0..2), "{line}");
    }
    let lines = vec!["[[source]]".to_owned(), "path = \"/mnt/disk\"".to_owned()];
    assert_eq!(find_source(&lines, Path::new("/mnt/other")), None);
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("big-search-config-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A file that exists but cannot be read is never replaced.
#[test]
fn unreadable_file_stays_untouched() {
    let dir = scratch("unreadable");
    let file = dir.join("config.toml");
    let latin1 = b"[[source]]\npath = \"/mnt/M\xfasica\"\n";
    std::fs::write(&file, latin1).unwrap();
    let result = edit_file(&file, |lines| {
        set_in_table(lines, "[defaults]", &[("content", "false")]);
        Ok(())
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(std::fs::read(&file).unwrap(), latin1);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// An edit keeps the file's mode and writes through a symbolic link.
#[test]
fn an_edit_keeps_mode_and_link() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("link");
    let target = dir.join("dotfiles.toml");
    let link = dir.join("config.toml");
    std::fs::write(&target, "[defaults]\ncontent = true\n").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    edit_file(&link, |lines| {
        set_in_table(lines, "[defaults]", &[("content", "false")]);
        Ok(())
    })
    .unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(
        !read_text(&std::fs::read_to_string(&target).unwrap())
            .defaults
            .content
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Remembering where files came from is on; reading the browsers' download
/// list is not.
#[test]
fn browser_list_starts_off() {
    assert!(Origin::default().enabled);
    assert!(!Origin::default().browser_history);
}

/// The disk budget fits the disk it is on, and a number written by hand
/// wins.
#[test]
fn the_budget_fits_the_disk() {
    const GIB: u64 = 1 << 30;
    assert_eq!(history_budget_bytes(0, 120 * GIB), 6 * GIB);
    assert_eq!(history_budget_bytes(0, 2000 * GIB), 10 * GIB);
    assert_eq!(history_budget_bytes(2, 2000 * GIB), 2 * GIB);
    // A disk too small for a sensible share gets a share anyway, not a
    // division by zero or a surprise ceiling.
    assert_eq!(history_budget_bytes(0, 20 * GIB), GIB);
}

/// Keeping versions is on, and the numbers people may want to change are
/// the only ones in the file.
#[test]
fn history_defaults_keep_ten_versions() {
    let history = FileHistory::default();
    assert!(history.enabled);
    assert_eq!(history.keep_versions, 10);
    assert_eq!(history.keep_days, 30);
    assert_eq!(history.max_total_gib, 0);
    assert!(history.skip_git_repositories);
    assert!(history.respect_gitignore);
}
