# BigSearch

**Encontre arquivos locais por nome e conteúdo, com indexação incremental.**

Engine sem privilégios, serviço por socket Unix e CLI mantêm um índice Tantivy. Perfis automáticos consideram memória disponível. O desempenho deve ser medido com os arquivos, armazenamento e máquina reais.

[English](README.md) · [Guia de início](docs/handbook/getting-started.md) · [Contribuir](CONTRIBUTING.md)

**Pré-lançamento.** Ainda não houve release stable. Use dados descartáveis e uma
sessão de teste. Baixo consumo é uma meta; esta página não apresenta benchmarks.

## Experimente este componente

Execute na raiz deste Git, com Rust/Cargo 1.98.1, bibliotecas nativas e vendor
já preparados. Dependências diretas de repositórios: `big-framework`
Mantenha os irmãos necessários nos commits selecionados pelo integrador; eles
não são baixados automaticamente. Os diretórios das fontes mantêm seus nomes
originais. O inventário em [Componentes](docs/COMPONENTS.md) contém os caminhos locais.

```sh
cargo build -p big-search --bin big-search --locked --offline
```

Programas gráficos exigem sessão como usuário comum e um prazo finito nos testes.
Compilar não instala helpers, schemas ou portais. Veja a [validação](docs/handbook/validation.md).

## Desenvolva separadamente, integre em conjunto

Este Git mantém seu componente; `big-suite` reúne as versões aprovadas. A composição
opcional `builtin-session` mantém desktop, terminal, arquivos, editor e imagens
nas interfaces elegíveis do mesmo host. BigShot ainda não está hospedado; engine de
busca, compositor e helpers permanecem separados. Isso não garante isolamento
contra falhas nativas ou atualização binária independente.

[Arquitetura](docs/handbook/architecture.md) · [Manutenção](docs/handbook/maintenance.md) · [Proveniência](REPOSITORY.md)

## Participe

Relate um fluxo reproduzível, melhore traduções, teste acessibilidade ou envie uma
correção pequena. Deixe uma estrela se este projeto representa o desktop que você
gostaria de usar. Comece em [CONTRIBUTING.md](CONTRIBUTING.md); agentes leem [AGENTS.md](AGENTS.md).

A [lista de componentes](docs/COMPONENTS.md) informa as licenças declaradas pelos
manifests. A licença da raiz não substitui as de componentes ou terceiros.
