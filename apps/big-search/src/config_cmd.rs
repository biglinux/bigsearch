//! The `config` subcommand: inspect and edit config.toml from the CLI.
//! Split out of `main.rs`; a child module of the crate root, so
//! `use super::*` supplies the shared imports, color helpers, and the
//! library `config`/`settings` modules. The dispatcher glob-imports the
//! entry point (`cmd_config`) back.

use super::*;

/// `config`: inspect and modify the documented config.toml without requiring a text editor.
pub(super) fn cmd_config(tokens: &[String]) -> Result<()> {
    settings::seed_config_template();
    match tokens.first().map(String::as_str) {
        None | Some("-h") | Some("--help") => {
            print_config_help();
            Ok(())
        }
        Some("path") => {
            println!("{}", config::config_file().display());
            Ok(())
        }
        Some("show") => cmd_config_show(),
        Some("set") => {
            if tokens.len() != 3 {
                print_config_help();
                anyhow::bail!("usage: config set KEY VALUE");
            }
            set_config_key(&tokens[1], &tokens[2])?;
            println!("{} {} = {}", green("set"), tokens[1], tokens[2]);
            println!(
                "{}",
                dim(
                    "Run `big-search rebuild` (daemon on) or `big-search reindex` for indexing-policy changes."
                )
            );
            Ok(())
        }
        Some("preset") => {
            if tokens.len() != 2 {
                print_config_help();
                anyhow::bail!("usage: config preset names-only|low-memory|balanced|complete");
            }
            apply_config_preset(&tokens[1])?;
            println!("{} preset {}", green("applied"), tokens[1]);
            println!(
                "{}",
                dim(
                    "Run `big-search rebuild` (daemon on) or `big-search reindex` to apply this profile."
                )
            );
            Ok(())
        }
        Some(other) => {
            print_config_help();
            anyhow::bail!("unknown config command: {other}")
        }
    }
}

pub(super) fn print_config_help() {
    let title = bold("big-search config");
    println!(
        "{title}\n\n{}\n  config show\n  config path\n  config set names-only true|false\n  config set content-index-mode auto|basic|freqs\n  config set extract-max-mb 0|4|8|16|32\n  config preset names-only|low-memory|balanced|complete\n\n{}\n  names-only     menor índice: só nomes/caminhos\n  low-memory     conteúdo ligado, postings Basic, caps conservadores\n  balanced       automático: Basic abaixo de 6 GiB RAM, Freqs acima\n  complete       Freqs + limite maior de extração\n",
        cyan("Uso:"),
        cyan("Perfis:"),
    );
}

pub(super) fn cmd_config_show() -> Result<()> {
    let requested = settings::content_index_mode().as_str();
    let resolved = settings::resolved_content_index_mode().as_str();
    let ram = settings::total_memory_bytes()
        .map(format_bytes)
        .unwrap_or_else(|| "desconhecida".to_string());
    let profile = if settings::names_only() {
        yellow("só nomes")
    } else {
        green("nomes + conteúdo")
    };
    println!(
        "{} {}",
        bold("Arquivo de configuração:"),
        config::config_file().display()
    );
    println!("{} {ram}", bold("RAM total:"));
    let capacity = settings::effective_memory_bytes()
        .map(format_bytes)
        .unwrap_or_else(|| "desconhecida (limites conservadores)".to_owned());
    println!(
        "{} {capacity}",
        bold("Capacidade observada para dimensionamento:")
    );
    println!("{} {profile}", bold("Perfil efetivo:"));
    println!(
        "{} pedido={}, efetivo={}",
        bold("Modo de índice de conteúdo:"),
        requested,
        resolved,
    );
    println!(
        "{} {}",
        bold("Limite de extração por arquivo:"),
        settings::extract_max_mb_label()
    );
    println!(
        "{} {}",
        bold("Limite de entrada de texto:"),
        input_cap_label(settings::text_max_input_bytes())
    );
    println!(
        "{} {}",
        bold("Limite de entrada Office:"),
        input_cap_label(settings::office_max_input_bytes())
    );
    println!(
        "{} {}",
        bold("Limite de entrada PDF:"),
        input_cap_label(settings::pdf_max_input_bytes())
    );
    println!(
        "{} {}s",
        bold("Tempo limite de PDF:"),
        settings::pdf_timeout_secs()
    );
    println!(
        "{} BIG_SEARCH_NAMES_ONLY, BIG_SEARCH_CONTENT_INDEX_MODE, BIG_SEARCH_EXTRACT_MAX_MB, BIG_SEARCH_TEXT_MAX_MB, BIG_SEARCH_OFFICE_MAX_MB, BIG_SEARCH_PDF_MAX_MB",
        bold("Variáveis de ambiente:"),
    );
    Ok(())
}

pub(super) fn format_bytes(bytes: u64) -> String {
    if bytes >= (1 << 30) {
        format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes >= (1 << 20) {
        format!("{} MiB", bytes / (1 << 20))
    } else {
        format!("{bytes} bytes")
    }
}

pub(super) fn input_cap_label(bytes: u64) -> String {
    if bytes == 0 {
        "sem limite de entrada".to_string()
    } else {
        format!("{} MiB", bytes / (1 << 20))
    }
}

pub(super) fn set_config_key(key: &str, value: &str) -> Result<()> {
    let normalized = key.replace('_', "-").to_ascii_lowercase();
    match normalized.as_str() {
        "names-only" => set_default_config_value("names_only", parse_bool_value(value)?),
        "content" => set_default_config_value("content", parse_bool_value(value)?),
        "metadata" => set_default_config_value("metadata", parse_bool_value(value)?),
        "follow-symlinks" => set_default_config_value("follow_symlinks", parse_bool_value(value)?),
        "content-index-mode" | "content-mode" => {
            let Some(mode) = settings::ContentIndexMode::parse(value) else {
                anyhow::bail!("content-index-mode must be auto, basic, or freqs");
            };
            set_default_config_value("content_index_mode", string_literal(mode.as_str()))
        }
        "extract-max-mb" => set_default_config_value("extract_max_mb", parse_integer_value(value)?),
        "text-max-mb" => set_default_config_value("text_max_mb", parse_integer_value(value)?),
        "office-max-mb" | "content-max-mb" => {
            set_default_config_value("office_max_mb", parse_integer_value(value)?)
        }
        "pdf-max-mb" => set_default_config_value("pdf_max_mb", parse_integer_value(value)?),
        "pdf-timeout-secs" => {
            set_default_config_value("pdf_timeout_secs", parse_integer_value(value)?)
        }
        "content-cooldown-secs" | "cooldown-secs" => {
            set_default_config_value("content_cooldown_secs", parse_integer_value(value)?)
        }
        "symlink-policy" => match value {
            "personal-local" | "local-exact" => {
                set_default_config_value("symlink_policy", string_literal(value))
            }
            _ => anyhow::bail!("symlink-policy must be personal-local or local-exact"),
        },
        _ => anyhow::bail!("unknown config key: {key}"),
    }
}

/// The literal to write for a yes/no setting.
///
/// A literal, not a parsed value: what goes into the file is text, and building
/// a `toml::Value` first only to print it again was the service's whole reason
/// for depending on a TOML parser of its own.
pub(super) fn parse_bool_value(value: &str) -> Result<String> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok("true".to_string()),
        "0" | "false" | "no" | "off" => Ok("false".to_string()),
        _ => anyhow::bail!("expected boolean: true|false"),
    }
}

pub(super) fn parse_integer_value(value: &str) -> Result<String> {
    let parsed = value.parse::<i64>().context("integer config value")?;
    if parsed < 0 {
        anyhow::bail!("value must be >= 0");
    }
    Ok(parsed.to_string())
}

pub(super) fn set_default_config_value(key: &str, literal: String) -> Result<()> {
    set_default_config_literals(&[(key, literal)])
}

/// Apply one of the named profiles.
///
/// The profiles themselves are data in `big-search-config`, so the command line
/// and the desktop's settings window offer the same four and write the same
/// values.
pub(super) fn apply_config_preset(name: &str) -> Result<()> {
    let Some(preset) = big_search_config::Preset::parse(name) else {
        anyhow::bail!("unknown preset: {name}");
    };
    settings::seed_config_template();
    big_search_config::apply_preset(preset)
        .with_context(|| format!("write {}", config::config_file().display()))
}

pub(super) fn string_literal(value: &str) -> String {
    big_search_config::string_literal(value)
}

/// Write settings into `[defaults]`, keeping the person's comments and layout.
///
/// The rewriting itself lives in `big-search-config`, which the desktop's
/// settings window uses too: one description of the file, so the two ways of
/// changing it cannot disagree about what a line means. That crate also writes
/// through a rename, which this used to do in place.
pub(super) fn set_default_config_literals(values: &[(&str, String)]) -> Result<()> {
    settings::seed_config_template();
    let pairs: Vec<(&str, &str)> = values
        .iter()
        .map(|(key, literal)| (*key, literal.as_str()))
        .collect();
    big_search_config::set_defaults(&pairs)
        .with_context(|| format!("write {}", config::config_file().display()))
}
