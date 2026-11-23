// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

use super::*;

/// A machine nobody has configured shows the recommended amount of work.
#[test]
fn untouched_machine_is_balanced() {
    assert_eq!(preset_of(&Defaults::default()), Some(Preset::Balanced));
}

#[test]
fn every_preset_recognises_its_own_values() {
    for preset in Preset::all() {
        let mut defaults = Defaults::default();
        apply_preset_in_memory(&mut defaults, preset);
        assert_eq!(preset_of(&defaults), Some(preset), "{}", preset.as_str());
    }
}

#[test]
fn custom_values_read_as_mixture() {
    let defaults = Defaults {
        extract_max_mb: 7,
        ..Defaults::default()
    };
    assert_eq!(preset_of(&defaults), None);
}

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

#[test]
fn implicit_home_removal_refused() {
    let mut lines: Vec<String> = "[defaults]\ncontent = true\n"
        .lines()
        .map(String::from)
        .collect();
    let home = Path::new("/home/p");
    let refused = remove_source_from(&mut lines, home, home).expect_err("home stays catalogued");
    assert_eq!(refused.kind(), io::ErrorKind::NotFound);
    remove_source_from(&mut lines, Path::new("/mnt/disk"), home)
        .expect("an unlisted folder is out");
    assert_eq!(lines, ["[defaults]", "content = true"]);
}

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
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let dir = std::env::temp_dir().join(format!(
        "big-search-config-{name}-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    // Created here and only here: a path someone else planted fails the test.
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .expect("create a fresh scratch directory");
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

/// The disk budget fits the disk it is on, and a number written by hand
/// wins.
#[test]
fn the_budget_fits_the_disk() {
    const GIB: u64 = 1 << 30;
    assert_eq!(history_budget_bytes(0, 120 * GIB), 6 * GIB);
    assert_eq!(history_budget_bytes(0, 2000 * GIB), 10 * GIB);
    assert_eq!(history_budget_bytes(2, 2000 * GIB), 2 * GIB);
    assert_eq!(history_budget_bytes(0, 20 * GIB), GIB);
}

/// A choice another build added reads as the default, and leaves the rest of
/// the file, its folders included, as written.
#[test]
fn unknown_choice_keeps_the_rest() {
    let config = parse(
        r#"
        [defaults]
        names_only = true
        content_index_mode = "positional"

        [[source]]
        path = "/srv/fotos"
        symlink_policy = "everywhere"
        "#,
    )
    .expect("the file reads");
    assert!(config.defaults.names_only);
    assert_eq!(
        config.defaults.content_index_mode,
        ContentIndexMode::default()
    );
    assert_eq!(config.sources[0].path, PathBuf::from("/srv/fotos"));
    assert_eq!(
        config.sources[0].symlink_policy,
        Some(SymlinkPolicy::default())
    );
}

/// A folder whose name needs escaping in the file is still found.
#[test]
fn an_escaped_folder_path_is_found() {
    let path = Path::new("/mnt/a \"quoted\" \\ disk");
    let mut lines = Vec::new();
    append_source(&mut lines, &SourceEntry::new(path));
    assert!(find_source(&lines, path).is_some(), "{lines:?}");
}

/// A line break or another control character in a folder name stays inside
/// its one line of TOML.
#[test]
fn control_characters_stay_valid_toml() {
    let path = Path::new("/home/u/notes\nold\u{1}\u{7f}\ttab");
    let mut lines = Vec::new();
    append_source(&mut lines, &SourceEntry::new(path));
    assert_eq!(lines.len(), 2, "{lines:?}");
    let config = parse(&lines.join("\n")).unwrap();
    assert_eq!(config.sources[0].path, path);
    assert!(find_source(&lines, path).is_some());
}

/// The home folder a file without folders stands for gets its own block.
#[test]
fn implicit_home_gets_written() {
    let mut lines = vec!["[defaults]".to_owned(), "content = true".to_owned()];
    let home = Path::new("/home/p");
    assert!(set_content_in(&mut lines, home, home, false));
    let parsed = read_text(&lines.join("\n"));
    assert_eq!(parsed.sources.len(), 1);
    assert_eq!(parsed.sources[0].path, home);
    assert_eq!(parsed.sources[0].content, Some(false));
}

/// A folder the file does not list is not added back.
#[test]
fn unlisted_folder_is_refused() {
    let mut lines = vec!["[[source]]".to_owned(), "path = \"/mnt/disk\"".to_owned()];
    let before = lines.clone();
    assert!(!set_content_in(
        &mut lines,
        Path::new("/home/p"),
        Path::new("/home/p"),
        true
    ));
    assert_eq!(lines, before);
}

/// The first folder added brings the home folder with it, once.
#[test]
fn first_folder_keeps_home() {
    let home = Path::new("/home/p");
    let mut lines = vec!["[defaults]".to_owned()];
    add_source_to(&mut lines, &SourceEntry::new("/mnt/disk"), home);
    add_source_to(&mut lines, &SourceEntry::new("/mnt/disk"), home);
    let parsed = read_text(&lines.join("\n"));
    assert_eq!(
        parsed.sources,
        vec![SourceEntry::new(home), SourceEntry::new("/mnt/disk")]
    );
}

fn scratch_file(name: &str, text: &str) -> (PathBuf, PathBuf) {
    let dir = scratch(name);
    let file = dir.join("config.toml");
    std::fs::write(&file, text).unwrap();
    (dir, file)
}

/// A file that does not parse is not edited: a folder added to it could land
/// as a second block for one folder, or the rewrite could drop the rest.
#[test]
fn broken_file_refuses_new_folder() {
    let original = "[[source]]\npath = \"/home/p\"\ncontent = = true\n";
    let (dir, file) = scratch_file("broken", original);
    let result = edit_file(&file, |lines| {
        add_source_to(lines, &SourceEntry::new("/mnt/new"), Path::new("/home/p"));
        Ok(())
    });
    let left = std::fs::read_to_string(&file).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(left, original);
}

/// A file somebody made read-only is not replaced by the rename.
#[test]
fn read_only_file_is_kept() {
    use std::os::unix::fs::PermissionsExt;
    let original = "[defaults]\ncontent = true\n";
    let (dir, file) = scratch_file("read-only", original);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
    let result = edit_file(&file, |lines| {
        set_in_table(lines, "[defaults]", &[("content", "false")]);
        Ok(())
    });
    let left = std::fs::read_to_string(&file).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(left, original);
}

/// Two writers at once keep each other's changes.
#[test]
fn concurrent_writers_keep_both() {
    let (dir, file) = scratch_file("concurrent", "[defaults]\n");
    let writers: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|prefix| {
            let file = file.clone();
            std::thread::spawn(move || {
                for index in 0..40 {
                    let key = format!("{prefix}{index}");
                    edit_file(&file, |lines| {
                        set_in_table(lines, "[defaults]", &[(key.as_str(), "1")]);
                        Ok(())
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    let table: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(table["defaults"].as_table().unwrap().len(), 80, "{text}");
}

/// A file written with Windows line endings keeps them.
#[test]
fn crlf_file_stays_crlf() {
    let (dir, file) = scratch_file("crlf", "# mine\r\n[defaults]\r\ncontent = true\r\n");
    edit_file(&file, |lines| {
        set_in_table(lines, "[defaults]", &[("content", "false")]);
        Ok(())
    })
    .unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(text, "# mine\r\n[defaults]\r\ncontent = false\r\n");
}
