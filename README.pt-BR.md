# BigSearch

Busca de desktop para Linux que encontra arquivos pelo nome e pelo conteúdo,
mantém o índice em dia conforme os arquivos mudam e não atrapalha em hardware
antigo.

O BigSearch roda como serviço do usuário. Não precisa de root, observa as pastas
visíveis (arquivos ocultos e o que um `.gitignore` exclui ficam de fora) e
responde consultas por um socket Unix. O índice é o
[Tantivy](https://github.com/quickwit-oss/tantivy); o controle em volta dele é
SQLite.

[English](README.md)

> **Estado:** pré-lançamento. O protocolo do socket, o arquivo de configuração e
> o formato em disco ainda podem mudar entre versões.

## Recursos

- **Nomes:** busca por trecho, sem diferenciar maiúsculas, no caminho completo,
  com filtro por pasta e extensão na CLI e por tipo de mídia pelo socket.
- **Conteúdo:** texto puro, Markdown, código-fonte, PDF (via `pdftotext`) e
  documentos OpenDocument / Office Open XML. Chinês, japonês, coreano e as
  escritas do Sudeste Asiático são indexados em bigramas sobrepostos, então
  podem ser buscados sem dicionário.
- **Índice ao vivo:** inotify em cada pasta visível; um arquivo salvo aparece na
  busca em segundos. Depois de reiniciar, só o que mudou com o serviço parado é
  lido de novo.
- **Versões anteriores:** em sistemas de arquivos com reflink (Btrfs, XFS), uma
  cópia do documento é guardada antes de cada gravação, dentro de um limite de
  espaço.
- **Origem:** de onde um arquivo veio — download do navegador, cópia,
  movimentação — quando isso é conhecido.
- **Leve para a máquina:** a extração roda com prioridade ociosa e se ajusta à
  carga de CPU, à pressão de E/S e de memória e ao uso de bateria. Parado, o
  daemon dorme até algum arquivo mudar.

## Requisitos

- Linux com inotify, e systemd para o serviço do usuário.
- Rust 1.98.1; com rustup, o `rust-toolchain.toml` o seleciona.
- SQLite 3 e, para conteúdo de PDF, poppler (`pdftotext`).
- Só para a página de configurações: GTK 4.22 e libadwaita 1.9.

## Compilar e instalar

```sh
cargo build --release
install -Dm755 target/release/big-search ~/.local/bin/big-search
install -Dm644 apps/big-search/packaging/big-search.service \
    ~/.config/systemd/user/big-search.service
systemctl --user enable --now big-search.service
```

A unit é endurecida: `ProtectSystem=strict`, `ProtectHome=read-only` com um
diretório de dados gravável, `RestrictAddressFamilies=AF_UNIX` e filtro de
syscalls. Em distribuições baseadas no Arch,
[`apps/big-search/packaging/arch`](apps/big-search/packaging/arch) gera um pacote.

## Uso

```sh
big-search contrato                     # nomes de arquivo
big-search content nota fiscal 2026     # conteúdo
big-search both relatorio -n 20         # os dois, sem repetir
big-search contrato --ext pdf --under ~/Documentos

big-search status                       # o que o daemon está fazendo
big-search pause | resume | rebuild
big-search versions ~/notas/tarefas.md  # versões anteriores guardadas
big-search origin ~/Downloads/arquivo.zip  # de onde o arquivo veio
```

As consultas vão para o daemon quando ele está rodando e, se não estiver, leem o
índice direto. `big-search --help` lista todos os comandos. A saída é colorida
no terminal; `--color never`, `NO_COLOR` e `CLICOLOR` são respeitados.

## Configuração

`~/.config/big-search/config.toml` é criado na primeira execução, com cada
chave explicada. Sem nenhum bloco `[[source]]`, a pasta pessoal é indexada. A
CLI cobre as mudanças mais comuns:

```sh
big-search config show
big-search config preset low-memory     # names-only | low-memory | balanced | complete
big-search config set extract-max-mb 16
```

Exclusões extras vão em `~/.config/big-search/ignore`, na sintaxe do
gitignore. Uma linha com `!` traz de volta algo que as regras embutidas pulam
(`node_modules/`, `target/`, arquivos minificados e afins).

Índices e estado ficam em `~/.local/share/big-search/`; o socket é
`$XDG_RUNTIME_DIR/biglinux/indexd.sock`, com modo `0600`.

## Organização do repositório

| Caminho | Conteúdo |
|---|---|
| [`apps/big-search`](apps/big-search) | O daemon e a CLI. O [DESIGN.md](apps/big-search/DESIGN.md) explica a arquitetura e as medições por trás dela. |
| [`crates/foundation/big-indexd-client`](crates/foundation/big-indexd-client) | Tipos do protocolo do socket e um cliente pequeno para outros programas. |
| [`crates/foundation/big-search-config`](crates/foundation/big-search-config) | Lê e grava o `config.toml` sem perder os comentários do usuário. |
| [`crates/ui/generic/big-search-settings`](crates/ui/generic/big-search-settings) | A página de configurações em GTK que outros programas incorporam. |

`cargo build` compila só o daemon; use `--workspace` para incluir a página de
configurações.

## Contribuir

Veja o [CONTRIBUTING.md](CONTRIBUTING.md). Problemas de segurança seguem o
[SECURITY.md](SECURITY.md).

## Licença

MIT. Veja o [LICENSE](LICENSE). O `scripts/third-party-notices.py` gera as
licenças dos crates compilados no binário, para quem for redistribuí-lo; o
pacote do Arch as instala junto do LICENSE.
