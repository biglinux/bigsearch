# Big Design System 2026 — clareza contextual

R14 · 21 de setembro de 2026. **Guideline própria, com requisitos de implementação e validação. Não descreve uma interface nova já entregue.**

O Big deve parecer um produto único, pensado para encontrar e realizar tarefas, não uma coleção de painéis técnicos com uma camada de CSS. O diferencial não é imitar um sistema existente: é diminuir a necessidade de memorizar ícones, caminhos, comandos e acontecimentos passados, conservando controle, privacidade e desempenho. A meta é uma experiência contemporânea que continue confortável num computador antigo.

Este documento define o padrão futuro. O código contém partes reutilizáveis, mas a integração de todas elas, o perfil global Com nomes e a nova apresentação dos 23 applets ainda exigem implementação. A auditoria separada registra o que já existe e o que falta. Nenhuma afirmação de exclusividade histórica, premiação, conformidade ou economia de memória decorre desta guideline.

## 1. Referências web recentes e o que aproveitar

As referências de direção visual e componentes são os produtos e implementações web abaixo, com datas verificadas. A data é do evento citado, não uma afirmação de que todos os princípios nasceram naquele ano.

| Referência | Evidência de atualidade | Aplicar no Big | Não copiar |
|---|---|---|---|
| Linear | Redesign de 12/03/2026 | Cabeçalhos e controles consistentes; reduzir competição entre navegação e tarefa; contexto contínuo. | Ícones pequenos sem nome, contraste baixo ou atalhos destinados a usuários especialistas. |
| Atlassian | Atualização visual 22/04/2025; tipografia GA 09/09/2025 | Tipografia por papel, métricas distintas de títulos, iconografia alinhada ao texto e migração testada. | Fonte/marca proprietária por padrão ou complexidade de ferramentas corporativas. |
| React Spectrum 2 | Primeira versão estável dessa implementação 16/12/2025; evolução em 2026 | Controles completos, estados, seleção, detalhes e validação; composição com acessibilidade preservada. | React dentro do desktop ou confundir o lançamento da implementação com a criação original de Spectrum 2. |
| Base UI | v1.0 em 11/12/2025; v1.8 em 04/09/2026 | Separar semântica/interação de estilo; foco e popovers são parte da qualidade, assim como custo de montagem. | Código DOM/ARIA, media queries ou CSS web presumidos equivalentes ao GTK. |
| Vercel Geist | Documentação web atual consultada em 21/09/2026, sem atribuir uma data de criação fictícia | Papéis de superfícies e estados de carregamento/erro; densidade consistente e componentes especializados. | Estética de console, excesso de monocromia ou menus de comando como única entrada para iniciantes. |
| DTCG | Relatórios estáveis 2025.10 de 28/10/2025 | Tokens legíveis por ferramentas e referências semânticas em vez de números dispersos. | Tratar relatório de Community Group como Recomendação W3C ou adicionar um interpretador de tokens em runtime sem necessidade. |

Fluent deixa de ser a referência organizadora desta evolução. Seus princípios úteis não precisam ser rejeitados, mas não determinam a identidade do Big. O mesmo vale para Carbon: fundamentos podem permanecer, sem dominar a direção visual.

As recomendações cognitivas WAI sobre rótulos visíveis são anteriores, de 2021/2022. São usadas por pertinência, não vendidas como tendência nova. A WCAG2ICT publicada em dezembro de 2025 é orientação para software não web, não um certificado. Atualidade estética não dispensa fundamentos de legibilidade e operação.

## 2. O critério principal: descoberta sem adivinhação

**Descobrir → compreender → agir → reconhecer o resultado → recuperar → reencontrar depois.** A última etapa é particularmente importante para o BigSearch e a proveniência de arquivos.

Uma função frequente deve ter uma entrada visível, com nome familiar e resultado compreensível. Uma função contextual deve aparecer junto do objeto ao qual se aplica. Uma função rara pode ficar numa expansão nomeada; o usuário não pode precisar saber previamente o conteúdo de reticências. O tooltip complementa, não revela sozinho uma capacidade essencial.

Mostrar menos não é sempre simplificar. No som, volumes de aplicativos à esquerda e dispositivos à direita evitam a necessidade de descobrir uma aba. Essa disposição permanece. Ao contrário, mostrar simultaneamente todos os protocolos e endereços da rede aumenta a dificuldade de escolher uma conexão. Preservar capacidade e reorganizar parâmetros são decisões distintas.

Cada tarefa deve ter mais de uma porta coerente quando útil: applet, central pertinente e busca por termos cotidianos. São entradas para o **mesmo comando/destino**, não implementações paralelas. A busca é uma ponte, não justificativa para navegação pobre: uma pessoa pode nem saber qual palavra pesquisar.

### Regras verificáveis

- Ações essenciais são reconhecíveis antes do hover e sem botão direito.
- Nomes visíveis e acessíveis se referem à mesma ação e ao mesmo objeto.
- Um botão escondido porque falta hardware não apaga a explicação da capacidade quando ela é procurada.
- Existe caminho seguro de volta. Mudança externa não troca o item sob o clique, o foco ou Enter.
- Zero, sem dado, aguardando, pausado, recusado e indisponível não compartilham uma representação ambígua.
- O sistema nunca remove recursos, favoritos, organização ou documentos para ativar uma apresentação mais fácil.

## 3. Linguagem visual própria

A proposta estética é **superfície limpa, hierarquia tipográfica forte, cor de acento controlada, nomes bem resolvidos e movimento curto**. Não é um tema monocromático de ferramenta de desenvolvedor nem um mosaico de cartões decorativos. A identidade emerge da consistência e dos pequenos detalhes, não da quantidade de efeitos.

### 3.1 Superfícies e composição

Um popover é uma superfície flutuante. Seu interior organiza-se por espaçamento, cabeçalhos discretos e divisores pertinentes. Não adicionar um cartão sombreado a cada linha se todas estão no mesmo plano. Uma borda externa e uma elevação consistente explicam a separação do desktop. Se uma linha precisa de seleção persistente, usar um preenchimento localizado e um marcador que não seja apenas cor.

No configurador, a moldura não deve disputar atenção com o conteúdo. Navegação de categorias à esquerda quando realmente ajuda; conteúdo alinhado numa largura de leitura confortável; preview junto ao ajuste que modifica. Não repetir todas as categorias como cartões no centro e na barra lateral ao mesmo tempo.

O primeiro olhar deve identificar título, estado, tarefa principal e caminho de configuração. O desenho pode ter outras ações de igual importância, como os sliders simultâneos; não aplicar a regra artificial de um único botão de destaque se há múltiplas tarefas primárias legítimas.

### 3.2 Parâmetros ópticos iniciais

Os números abaixo são **propostas para protótipos comparáveis**, em unidades lógicas de layout. Não são estilos já aplicados, alturas máximas, medidas de screenshot ou obrigações de uma guideline externa. O baseline usa 6/12 para alguns raios e 150/250 ms em transições; uma alteração deve ser central e validada por papel, não um replace global.

| Papel | Proposta inicial | Regra que prevalece |
|---|---|---|
| Espaçamento | 4/8/12/16/24; externo 16, seções 16–24, itens relacionados 4–8 | Relação perceptível e alcance; evitar padding que cria rolagem sem benefício. |
| Cantos | Controle 8; superfície flutuante 16; grupos neutros 8–12; pill só onde comunica escolha compacta | Comparar com 6/12 atual. Raios internos devem harmonizar com padding, não quase tangenciar a borda externa. |
| Elevação | Contorno fino mais sombra ambiente moderada em apenas uma camada principal | Tema escuro e alto contraste não podem depender da mesma sombra preta do claro. Não sombrear cada item. |
| Superfície principal | Opaca atrás de textos e controles | Transparência pode ser uma opção de casca, não requisito para o produto parecer moderno. |
| Texto principal | Escala nativa; referência óptica 14–16 no tamanho usual | Preferência de fonte/tamanho do sistema e ampliação prevalecem. |
| Títulos | Poucos níveis, peso moderado, espaço antes da seção | Não transformar todo rótulo ou métrica em heading. |
| Ícones de ação | Arte 16/20/24 por papel, mesmo peso óptico | Hitbox maior; não usar o tamanho do SVG como área clicável. |
| Alvos de ponteiro | 36 como proposta usual, avaliando composição e espaçamento | Alvos frequentes/críticos e modo espaçoso/toque com pelo menos 44; mínimo web não é ideal universal. |
| Linhas | Altura por conteúdo, não por corte fixo | Nome longo pode crescer; rótulo essencial não desaparece para manter uma tabela de alturas. |
| Feedback | Estado pressionado/foco imediato; transição discreta 100–160 ms; expansão 180–240 ms | Interrompível, sem atrasar comando; redução de movimento pode usar duração zero. |

Não escolher uma porcentagem única de translucidez para todas as superfícies: o resultado depende do wallpaper, tema e contraste efetivo. Uma opção translúcida exige texto/controlos em camada legível e fallback opaco. **Nunca aplicar opacity ao pai de toda a interface** para simular vidro. Blur contínuo e distorção de vidro não serão requisitos, sobretudo na GPU antiga sem Vulkan.

### 3.3 Cor e identidade

Usar papéis `surface`, `content`, `secondary`, `border`, `accent`, `selected`, `focus`, `pending` e `critical`. Esses nomes são papéis propostos, não novas APIs disponíveis. Uma paleta clara e uma escura devem ser desenhadas, não invertidas mecanicamente. O acento pode aparecer em ação, seleção e pequeno detalhe de marca; não pintar todos os cards nem usar cores decorativas diferentes atrás de cada ícone de áudio/rede.

Ícones dos aplicativos conservam suas identidades; ícones de comando pertencem a uma família consistente. Uma identidade não muda para outro objeto só porque está silenciada: microfone não vira alto-falante. Erro usa texto/contexto além da cor. Uma borda de foco não significa que a ação é recomendada, selecionada ou concluída.

### 3.4 Texto e informação

O nome útil ganha espaço do metadado secundário. Diferenciadores como USB, sala ou dispositivo escolhido devem permanecer visíveis quando há prefixos iguais. Permitir duas linhas ou realocar contexto antes de elipse. Tooltip não resolve comparação de dois nomes cortados nem toque.

A informação secundária permanece legível; não acumular `caption`, `dim-label` e opacity. Valores usam unidades e alinhamento estável. Percentuais, barras e adjetivos não precisam repetir a mesma grandeza na primeira camada. A informação precisa pode continuar nos detalhes, selecionável/copiável.

O texto segue verbo + objeto quando há risco de ambiguidade: “Novo evento”, “Esquecer esta rede”, “Remover com segurança”, “Copiar novamente”, “Mostrar na pasta”. Não chamar uma operação de Colar se ela apenas prepara a área de transferência. Não dizer Internet conectada apenas porque o rádio está associado.

### 3.5 Estados e movimento

Hover colore somente a área interativa e não faz reflow. Pressionado responde imediatamente, preservando a possibilidade de cancelar o clique de acordo com o componente nativo. Foco não depende do mouse e não é cortado. Seleção permanece diferente do hover. A configuração pedida só vira estado confirmado após resposta real; falha conserva entrada e oferece uma recuperação pertinente.

Skeleton só onde a forma do conteúdo já é conhecida; não desenhar itens falsos de rede nem apresentar ausência como carregamento infinito. Evitar shimmer contínuo, especialmente com redução de movimento e economia de energia. Uma descrição “Buscando dispositivos…” com lista anterior preservada pode ser mais útil que apagar o cartão inteiro.

Movimento deve explicar abrir, expandir, reposicionar e concluir. Não usar bounce em cada botão, zoom no texto, animação de largura a cada amostra ou contadores animados indefinidamente. O layout não pode mudar sob uma ação em andamento. A lista pode reagrupar depois, com identidade estável e foco preservado.

## 4. Perfil Com nomes: uma apresentação de primeira classe

A pessoa escolhe por preferência, não por diagnóstico inferido. O nome **Com nomes** é uma proposta clara; não usar “modo para incapazes”, “modo infantil” ou “modo reduzido”. Todas as funcionalidades permanecem disponíveis.

| Superfície | Regra no perfil |
|---|---|
| Ícones da área de trabalho | Nome abaixo, distinguível, até duas linhas quando necessário, tamanho independente da arte. |
| Menu e resultados de busca | Ícone ao lado do nome; contexto curto quando ajuda a escolher. Resultados de arquivo não parecem aplicativos executáveis. |
| Painel e applets | Nome ao lado ou abaixo conforme a orientação; estado atual adicional sem substituir o nome pela leitura. |
| Barras de ferramentas do BigFiles e editor | Nome abaixo do ícone nas ações configuradas; ações em menus continuam com nome ao lado. |
| Botões perigosos ou pouco convencionais | Nome explícito também no perfil compacto quando necessário para entender consequência. |
| Mais ações / falta de espaço | Overflow com rótulos completos e agrupamento significativo; não virar icon-only silenciosamente. |

### 4.1 Reutilização concreta existente

A fonte já contém `BigControlDisplayMode::TextUnderIcon`, além de IconOnly, TextOnly e IconAndText; `LauncherContent::NameBelow`/NameBeside; `show_widget_names` no painel; configurações de widget e a infraestrutura compartilhada de personalização/fan-out. A página Conforto já possui um interruptor que mostra nomes ao lado dos ícones em todos os painéis. Isso é um ponto de partida concreto, não uma função a reescrever. Falta coordenar a apresentação entre desktop, menus e toolbars dos produtos, fechar as lacunas e oferecer preview/desfazer; não inventar um renderer novo para cada aplicativo.

O perfil global deve funcionar como padrão compartilhado, com override local explícito. “Seguir apresentação do sistema” e “Personalizar só este aplicativo” precisam ser distinguíveis. A resolução deve seguir um contrato simples: padrão da suíte → perfil escolhido → override local. Mudar o perfil não apaga o override; restaurar padrões informa o alcance.

Usar a coordenação já existente em `big-personalization`/configuração de shell e os contratos de eventos. Nome de chave, versão de schema e transporte novos só podem ser definidos na implementação, com testes. Não guardar rótulos num arquivo por applet nem transferir tipos Rust privados pela ABI C entre builds diferentes.

### 4.2 Aplicação e recuperação

Oferecer Com nomes na primeira configuração, em Aparência/Facilidade de uso e na busca. Mostrar uma prévia real pequena, incluindo painel, menu e toolbar. Aplicar permite Desfazer em uma operação coerente. **Não aplicar um Look que substitui painéis e organização só para mostrar nomes**; aparência textual e preset completo de desktop são ações diferentes.

Densidade, tamanho do texto, contraste e movimento são dimensões independentes. A pessoa pode combinar nomes com modo compacto, ou ícones com alvos espaçosos. Não forçar tema claro, sombras ou animações ao ativar rótulos.

Testes: painel horizontal/vertical, 200% de texto, traduções longas, RTL, app aberto durante a mudança, novo app após a mudança, override local, restart e desfazer. Não perder favoritos, abas, posições, atalhos nem conteúdo de documentos. No processo compartilhado, uma alteração em um produto não recria todos os aplicativos.

## 5. BigSearch como camada de reencontro e contexto

A busca não será somente uma lista de programas. Ela deve permitir reencontrar um arquivo por contexto, chegar à configuração que resolve uma tarefa e entender por que um resultado apareceu. As propostas abaixo não presumem compreensão livre de linguagem natural já implementada.

### 5.1 Antes da primeira letra

Usar uma entrada visível: **Buscar aplicativos, arquivos e configurações**, nos formatos cujo backend realmente possui esse escopo. O Clássico que só busca aplicativos deve anunciar esse limite. No estado vazio, atalhos textuais como Downloads e Arquivos recentes tornam o potencial visível sem exigir aprender um operador de consulta. Não exibir histórico sensível sem política e consentimento apropriados.

Após digitar, categorias distinguem Aplicativos, Arquivos, Configurações e outros tipos realmente suportados. Filtros contextuais surgem quando ajudam: origem disponível, período, tipo e pasta. Nenhum novo filtro aparece como funcional antes de contrato/teste com o engine. Resultados parciais indicam escopo, indexação e dados ausentes sem a frase absoluta “O arquivo não existe”.

### 5.2 Cartão de resultado contextual

Um arquivo pode mostrar nome, pasta, data pertinente e **origem registrada quando conhecida**. Ações diretas: Abrir, Mostrar na pasta e Ver informações. Quando só o domínio é conhecido, oferecer Abrir site apenas como domínio e não alegar que recupera a página original. Links completos só podem vir de fonte válida e segura, não reconstruídos do nome do arquivo.

Datas precisam de rótulo: baixado em, visto pela primeira vez, alterado em são fatos diferentes. Separar “Registrado pelo navegador”, “Informação do arquivo” e “Possível origem pela pasta” sem inventar um nível de certeza numérico. Ausência é “Origem não registrada”, não diagnóstico de falha do usuário.

Resultados de configuração levam à página/opção exata, com destaque não piscante e volta preservada. Reutilizar `settings/front_door.rs` e seu catálogo de sinônimos; não manter listas divergentes no menu, indexador e configurador. Termos como “letras maiores”, “nome dos ícones” e “não imprime” são casos de teste propostos, não promessas de entendimento atual.

### 5.3 O que a implementação examinada realmente suporta

`browser_downloads.rs` importa históricos apenas quando `origin.enabled` e `origin.browser_history` permitem. O caminho examinado atende seis famílias Chromium; Firefox não é lido por esse importador. Mantém domínio, data e metadados limitados, não URL completa com caminho/query. Seu comentário com quantidades de downloads vem de outra máquina; não é benchmark desta auditoria.

BigFiles consulta o serviço de origem e depois usa metadados/indícios locais. Portanto, não podemos anunciar recuperação universal da origem de qualquer download. O roteiro deve cobrir origem conhecida, opt-in desligado, navegador não suportado, arquivo movido/renomeado, mídia removível, serviço indisponível e metadado conflitante. Caminhos não UTF-8 merecem revisão específica: a consulta atual usa conversão lossy no caminho antes de consultar o índice; a guideline não autoriza atribuir origem de outro arquivo.

### 5.4 Privacidade e desempenho como parte da UX

Histórico de navegador, clipboard e consultas são informações pessoais. Nada disso passa a ser indexado automaticamente por um novo tema. Explicar o benefício e o dado lido antes de opt-in; oferecer exclusões e limpeza. Não guardar tokens, URLs assinadas ou texto de senha em evidências. A importação de dados não pode pausar a interface nem fazer o HDD disputar continuamente com ações do usuário.

Consultas cancelam gerações antigas. Índice e UI não trocam o alvo selecionado quando chegam mais resultados. Abrir executável, executar comando e modificar configurações têm intenção distinta de abrir documento; foco ou preview não executam nada. Resultado antigo verifica se o arquivo ainda corresponde à identidade esperada.

## 6. Gramática de todos os applets

Usar uma gramática comum, não 23 cartões idênticos:

**Entrada reconhecível → título e estado → ações cotidianas → contexto necessário → detalhes nomeados → configurador pertinente.**

Quatro famílias visuais bastam como ponto de partida: controle imediato (som/brilho), conexão/dispositivo (rede/Bluetooth/USB), coleção/atividade (notificações/clipboard/impressão), informação/ação pontual (clima/agenda/captura/sessão). Elas compartilham tokens e primitivos, mas não forçam um gráfico, capa, tab ou cabeçalho redundante onde não ajuda.

A ficha dos 23 applets acompanha esta guideline. Há também 27 IDs no catálogo de painel, com aliases e composições; monitorar todos evita usar a matriz de popovers como sinônimo de desktop completo. Tray de terceiros, indicadores de privacidade, taskbar, mostrar desktop e lixeira têm requisitos próprios.

Para som: apps à esquerda, dispositivos à direita; identidade, destino, mudo explícito e nomes legíveis. Para rede: uma coluna; conexão real, escolha, recepção/envio discretos, detalhes técnicos subordinados. Para Tela e energia: compor corpos de seção, não três cartões inteiros com três níveis de cabeçalho. Para outras famílias, consultar a tarefa específica em vez de copiar a densidade de Wi-Fi.

## 7. Gramática dos configuradores

Configurador não é um depósito das opções removidas do applet. Ele resolve tarefas maiores com o mesmo vocabulário e dono de estado.

Uma página começa com nome e estado pertinente; as escolhas mais usuais aparecem agrupadas por intenção. A explicação precede parâmetros pouco familiares, mas não repete obviedades. “Detalhes técnicos” não é sinônimo de opções de uso diário. Uma expansão genérica global não deve causar mudanças inesperadas em todas as páginas sem intenção; esse comportamento requer decisão/testes sobre o atual `show_advanced`.

Distinguir **configurar o dispositivo** de **mudar a aparência do botão**. Som, rede e Bluetooth já possuem centrais de tarefa na engrenagem; não desfazer esse avanço. Nos outros casos, rótulo e menu mostram o destino real. Pode haver uma entrada compacta “Configurações de…” visível no perfil Com nomes, sem replicar duas ações idênticas na mesma tela.

Volume e luminosidade podem oferecer aplicação imediata com feedback de serviço. Um formulário de conexão precisa revisão/erro sem apagar campos. Alteração de telas exige recuperação apropriada. O usuário não deve adivinhar se algo já valeu, está pendente ou ainda precisa Aplicar. Reset nomeia o escopo; mudança por terceiro não é sobrescrita silenciosamente.

O preview usa componentes existentes e dados seguros. Alterar a aparência de um widget não pode ativar Bluetooth real como efeito de preview. Listas atualizadas preservam foco e scroll; erro de autenticação conserva entrada não sensível e limpa segredos segundo a política. Campos secretos não são gravados em snapshots de teste.

O catálogo de 53 destinos e as páginas das centrais estão enumerados na auditoria. Preservar todos os caminhos não obriga exibi-los simultaneamente em toda máquina. Acesso contextual e busca reutilizam o mesmo resolver; hardware ausente tem explicação e teste, não estado vazio enganoso.

## 8. Contrato de implementação nativa e manutenção

O sistema continua Rust/GTK4/libadwaita, com GTK 4.22.5 e libadwaita 1.9.4 no snapshot examinado. Layout usa os containers e modelos nativos; CSS estiliza nós GTK, não DOM/Flexbox/Grid do navegador. GSK/Cairo servem a gráficos, indicadores e desenho justificado. Não substituir entrada, slider ou lista nativa por canvas sem teclado, foco e ações AT-SPI equivalentes.

Os padrões web orientam o comportamento, mas a API suportada do GTK define a implementação. Checar propriedades CSS disponíveis; `:focus-visible` pode atingir ancestrais. Temas de alto contraste/redução de movimento não são vistos como degradação. Um teste que exige sempre um véu translúcido precisa passar a testar modos e contraste, não ser simplesmente apagado.

Tokens têm nomes por papel, componentes seguem um catálogo comum e APIs de domínio continuam separadas. Um arquivo DTCG 2025.10 pode ser adotado como fonte de build após decidir como substituir o atual proprietário de tokens; não manter JSON e CSS manuais divergentes nem adicionar um daemon de design. Sem migração aprovada, os tokens atuais continuam a fonte de verdade.

O framework mantém primitivas e preferências comuns. O desktop mantém applets e seus fluxos; BigFiles mantém picker/propriedades; BigSearch mantém indexação/proveniência. O integrador liga as bibliotecas para o mesmo processo e valida o conjunto. Repositórios separados não exigem processos, nem duas cópias dos caches/controles. Não copiar a guideline para nove versões divergentes: o exportador copia o handbook do snapshot fixado.

No hardware de referência: nenhuma animação demanda rede, um novo pool ou coleta após fechar. Não reconstruir cartão a cada valor; limitar filas, cancelar gerações e devolver recursos por proprietário. Transparência e sombras têm custo e exigem medição; “CSS” não implica custo zero. OpenGL deve continuar utilizável; Vulkan não é requisito para aparência correta.

## 9. Aprovação por tarefa, evidência e cobertura

Para cada componente/fluxo, guardar **revisão, dados/serviço, idioma, tamanho de texto, tema, escala, renderer, área disponível e alocação real**. Separar imagem de componente, popup ancorado, vídeo/interação, backend controlado, hardware real e sessão com pessoas. O nome narrow num PNG não prova largura reduzida. Fullscreen em GtkWindow de 760×520 não é validação do monitor inteiro.

Estados mínimos: normal, sem dados, carregando, erro, sem permissão, hardware ausente, item que desaparece e nome longo. Entradas: ponteiro, teclado, leitor de tela e toque quando suportado. Perfis: Equilibrado e Com nomes, com texto aumentado e pelo menos claro/escuro/alto contraste. Medir foco, alvo e contraste; não derivar razão normativa das bordas antialiasadas das letras.

Adotar contrastes WCAG pertinentes como referência documentada: texto comum 4,5:1, componentes/estados não textuais essenciais 3:1, 7:1 como meta aprimorada para texto. A exceção de texto grande possui definição própria. 24 CSS px é um mínimo web AA com exceções; 44 é a referência ampliada, não uma prova de que todo widget do projeto já a cumpre. Mapeamento para software nativo e unidades lógicas deve estar explícito.

A tarefa começa no desktop fechado e não instrui onde clicar. Exemplo: “Você baixou um documento há alguns dias; encontre-o e descubra de onde veio”. Medir primeiro destino, sucesso sem ajuda, entendimento da origem, recuperação e retenção após intervalo. Outra tarefa: “Não lembro o que estes símbolos significam; faça os nomes aparecerem”. Não contar apenas cliques; uma pessoa pode concluir sem compreender o resultado.

Pequenas rodadas formativas encontram problemas, não estimam automaticamente percentuais da população. Usar participantes consentidos, incluindo pessoas com diferentes necessidades cognitivas/sensoriais/motoras; nunca inferir deficiência pela forma de uso. Comparar versões com ordem controlada para reduzir aprendizado. Relatos de vocês são requisitos e hipóteses úteis, não prova de resultado com todas as pessoas.

Testes de código, interação e desempenho acompanham essas tarefas. Fechar 300 vezes prova somente o escopo medido de lifetime, não meses de estabilidade. Uma falha grave não é compensada por média estética. Não publicar nota AAA+, benchmark ou prêmio esperado sem a evidência correspondente.

## 10. Ordem de execução

1. **Base comum e descoberta:** integrar Com nomes sobre recursos existentes; fechar nomes truncados, estados ambíguos e entradas ocultas sem alternativa. Corrigir o dado ausente do clima antes de embelezar o gráfico.
2. **Configuração contextual e BigSearch:** destinos diretos, recuperação/proveniência legível e consentida, tarefas de reencontro, sinônimos testados e resultado estável.
3. **Componentes e superfícies:** seções reaproveitáveis, hierarquia e tokens por papel; revisar os 23 applets e os configuradores por família, sem preservar um único applet como exceção privilegiada.
4. **Acabamento e comprovação:** pixels e interações reais, perfis, movimentos, sessões com pessoas e medições no notebook. Aplicar correções por mudanças comparáveis, não um lote que mistura backend, tema, fonte e animação sem rastreio.

Cada etapa produz patch pequeno, testes de regressão e evidência compatível. Não considerar a guideline executada porque o documento foi escrito. O progresso precisa separar política, protótipo, implementação, teste de componente e validação com pessoas/hardware.

## Referências primárias

- [Linear: UI refresh, 12/03/2026](https://linear.app/changelog/2026-03-12-ui-refresh) e [processo de design](https://linear.app/now/behind-the-latest-design-refresh).
- [Atlassian: atualização visual, 22/04/2025](https://atlassian.design/whats-new/atlassian-ui-refresh-updates) e [tipografia GA, 09/09/2025](https://atlassian.design/whats-new/new-typography-in-general-availability).
- [React Spectrum 2: implementação v1.0, 16/12/2025](https://react-spectrum.adobe.com/releases/v1-0-0) e [releases posteriores](https://react-spectrum.adobe.com/releases/).
- [Base UI: releases 2025/2026](https://base-ui.com/react/overview/releases) e [responsabilidades de acessibilidade](https://base-ui.com/react/overview/accessibility).
- [Geist: sistema web atual](https://vercel.com/geist/introduction), [skeleton](https://vercel.com/geist/skeleton) e [spinner](https://vercel.com/geist/spinner).
- [DTCG: relatórios 2025.10](https://www.designtokens.org/tr/2025.10/).
- [WAI: rótulos visíveis, orientação cognitiva 2021/2022](https://www.w3.org/WAI/WCAG2/supplemental/patterns/o4p06-clear-labels/).
- [WCAG2ICT: nota de 11/12/2025](https://www.w3.org/TR/wcag2ict-22/) e [WCAG 2.2](https://www.w3.org/TR/WCAG22/).
- [GTK: CSS](https://docs.gtk.org/gtk4/css-overview.html), [propriedades suportadas](https://docs.gtk.org/gtk4/css-properties.html) e [acessibilidade](https://docs.gtk.org/gtk4/section-accessibility.html).

Referências orientam escolhas; valores ópticos, perfis e estrutura propostos aqui são decisões do Big a validar. Nenhuma fonte tipográfica, imagem de marca ou componente React é redistribuído com este guia.

## Registro de implementação

A R15 implementou o tratamento de dados ausentes e a grade dos detalhes horários do
Clima. A R16 trabalha descoberta e identidade nas linhas de áudio, confirmação de
Esquecer rede, propriedade/foco dos controles e nomes completos de categorias.
O contrato mantido de comportamento e testes está em [Applet layout](applet-layout.md).
Esses escopos não equivalem à aplicação integral deste sistema: **Com nomes** global,
todos os configuradores, proveniência/reencontro e validação com pessoas continuam
exigindo implementação e evidência próprias. Resultados executados pertencem aos
relatórios de cada artefato, não são presumidos por este registro.


## Implementação R19: nomes nas barras

A primeira camada implementada é [Nomes nas barras](panel-names.md): padrão reversível
do desktop, exceções por barra identificada, apresentações individuais preservadas e
prévia nativa. Ainda não coordena barras de ferramentas do BigFiles/editor, exceções
por aplicativo, títulos de janelas na barra de tarefas ou todo o conteúdo do menu.
O perfil amplo acima continua sendo requisito, não funcionalidade concluída.
Um contador de estado não substitui o nome do controle; sua preservação e atualização
dos ícones são verificadas separadamente.
