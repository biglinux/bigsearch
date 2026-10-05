// SPDX-FileCopyrightText: 2026 BigLinux contributors
// SPDX-License-Identifier: MIT

//! `big-docinfo` — suggest a name for a document from what it says.
//!
//! ```text
//! big-docinfo FILE              what the document is, and a suggested name
//! big-docinfo --json FILE       the same as JSON
//! big-docinfo --name FILE       only the suggested name, for the file manager's menu
//! big-docinfo rename FILE NAME  rename FILE to NAME in its own folder
//! ```
//!
//! Text comes out of the file the way the search service extracts it (same
//! limits, same `pdftotext` sandbox); the service does not need to be running.
//! Nothing is renamed without an explicit `rename`, and `rename` never replaces
//! an existing file: the kernel refuses atomically (`RENAME_NOREPLACE`).

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use big_docinfo::analyze;

const USAGE: &str = "usage: big-docinfo [--json | --name] FILE | big-docinfo rename FILE NAME";
/// Longest file name most Linux file systems accept, in bytes.
const NAME_MAX: usize = 255;

fn main() {
    // Messages are written for the person reading the dialog: one sentence with
    // the cause and what to do, no paths or error chains.
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, file, name] if cmd == "rename" => rename(Path::new(file), name),
        [flag, file] if flag == "--json" => describe(Path::new(file), Output::Json),
        [flag, file] if flag == "--name" => describe(Path::new(file), Output::Name),
        [file] if !file.starts_with('-') => describe(Path::new(file), Output::Text),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

enum Output {
    Text,
    Json,
    Name,
}

fn describe(path: &Path, output: Output) -> Result<()> {
    let (text, declared_title) = big_search::extract::content_with_lines_and_title(path).context(
        "Este arquivo não tem texto legível: o tipo não é suportado ou é um PDF escaneado.",
    )?;
    let ext = extension(path);
    let info = analyze(&text, ext, declared_title.as_deref());
    // With neither a kind nor a title the suggestion would say less than the
    // current name ("documento.odt"), so the current name stays.
    let kept = info.kind.is_none() && info.title.is_none();
    let free = if kept {
        path.file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    } else {
        free_name(path, &info.suggested_name, ext)
    };
    match output {
        Output::Json => {
            let value = serde_json::json!({
                "kind": info.kind,
                "date": info.date,
                "entity": info.entity,
                "title": info.title,
                "highlights": info.highlights,
                "suggested_name": info.suggested_name,
                "free_name": free,
                "kept_name": kept,
            });
            println!("{value}");
        }
        Output::Name => println!("{free}"),
        Output::Text => {
            let show =
                |label: &str, value: Option<&str>| println!("{label}: {}", value.unwrap_or("—"));
            show("Tipo", info.kind.map(kind_label));
            show("Data", info.date.as_deref());
            show("Entidade", info.entity.as_deref());
            show("Título", info.title.as_deref());
            if kept {
                println!(
                    "Não encontrei tipo nem título neste documento; o nome atual foi mantido."
                );
            }
            show("Nome", Some(&free));
        }
    }
    Ok(())
}

/// How a kind reads in the dialog.
fn kind_label(kind: &str) -> &str {
    match kind {
        "nota_fiscal" => "Nota fiscal",
        "recibo" => "Recibo",
        "fatura_boleto" => "Fatura ou boleto",
        "extrato" => "Extrato",
        "contrato" => "Contrato",
        "ata" => "Ata",
        "ato_normativo" => "Lei, decreto ou portaria",
        "edital" => "Edital",
        "parecer" => "Parecer ou laudo",
        "relatorio" => "Relatório",
        "oficio_carta" => "Ofício ou carta",
        "email" => "E-mail",
        "declaracao_certidao" => "Declaração ou certidão",
        "curriculo" => "Currículo",
        "manual" => "Manual",
        "artigo" => "Artigo",
        "orcamento" => "Orçamento",
        "anotacoes" => "Anotações",
        "saude" => "Documento de saúde",
        other => other,
    }
}

fn extension(path: &Path) -> &str {
    path.extension().and_then(|e| e.to_str()).unwrap_or("")
}

/// `name` if no other file in the folder has it, else `name-2`, `name-3`… before the extension.
fn free_name(path: &Path, name: &str, ext: &str) -> String {
    let dir = path.parent().unwrap_or(Path::new("."));
    let taken = |candidate: &str| {
        let target = dir.join(candidate);
        target.exists() && !same_file(&target, path)
    };
    if !taken(name) {
        return name.to_owned();
    }
    let stem = name
        .strip_suffix(&format!(".{ext}"))
        .filter(|_| !ext.is_empty())
        .unwrap_or(name);
    let dotted = if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    };
    (2..1000)
        .map(|n| format!("{stem}-{n}{dotted}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| name.to_owned())
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// Renames `path` to `name` in the same folder. The original extension is kept
/// (added back if the new name lost it); an existing file is never replaced.
fn rename(path: &Path, name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        bail!("“{name}” não serve como nome de arquivo. Use um nome sem barras.");
    }
    let ext = extension(path);
    let mut name = name.to_owned();
    if !ext.is_empty()
        && !name
            .to_lowercase()
            .ends_with(&format!(".{}", ext.to_lowercase()))
    {
        name = format!("{name}.{ext}");
    }
    if name.len() > NAME_MAX {
        bail!("Esse nome é longo demais. Use um nome mais curto.");
    }
    let target: PathBuf = path.parent().unwrap_or(Path::new(".")).join(&name);
    if same_file(&target, path) {
        return Ok(());
    }
    rename_noreplace(path, &target, &name)?;
    println!("{}", target.display());
    Ok(())
}

fn rename_noreplace(from: &Path, to: &Path, name: &str) -> Result<()> {
    let c_from = CString::new(from.as_os_str().as_bytes())?;
    let c_to = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both pointers are NUL-terminated strings that outlive the call, and
    // AT_FDCWD makes relative paths resolve exactly as `std::fs` would.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            c_from.as_ptr(),
            libc::AT_FDCWD,
            c_to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EEXIST) => {
            bail!("Já existe um arquivo chamado “{name}” nesta pasta. Escolha outro nome.")
        }
        // File systems without RENAME_NOREPLACE: a hard link also refuses to
        // replace, then the old name goes. Where links are impossible too, stop.
        Some(libc::EINVAL | libc::ENOSYS) => {
            std::fs::hard_link(from, to).context(
                "Este disco não permite renomear sem risco de apagar outro arquivo. Renomeie pelo gerenciador de arquivos.",
            )?;
            std::fs::remove_file(from)?;
            Ok(())
        }
        _ => bail!("Não foi possível renomear: {err}."),
    }
}
