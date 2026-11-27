# Como contribuir

Obrigado por ajudar. O projeto é experimental; apresente resultados e limitações com clareza para que outra pessoa consiga reproduzir e revisar o trabalho. Ferramentas como Claude e Codex são bem-vindas, com revisão humana das mudanças.

## Antes de começar

- Leia [README.md](README.md), [arquitetura](docs/architecture.md) e a [política de dados](docs/asset-policy.md). Não inclua no repositório arquivos do jogo nem materiais derivados proibidos pela política. Use dados e caches de teste separados e revise logs antes de compartilhá-los.
- Procure uma issue relacionada. Antes de implementar, avise nela qual abordagem e quais arquivos pretende investigar. Se houver sobreposição, combine as fronteiras com o mantenedor e demais colaboradores. Investigação paralela é bem-vinda; uma issue não concede exclusividade sobre um subsistema.
- Trabalhe em uma branch ou fork. Abra um PR em rascunho quando houver trabalho concreto e atualize a issue se o escopo mudar. O mantenedor faz a triagem, ajuda a resolver sobreposições e decide quando integrar.
- Quando disponíveis, labels como `testes`, `documentacao`, `gpu`, `memoria` e `desempenho` ajudam na triagem; o mantenedor pode ajustá-las. Não é preciso ter permissão para aplicá-las ao abrir uma issue.

## Áreas do projeto

Os caminhos são pontos de partida, não fronteiras exclusivas. Veja a arquitetura para a relação completa entre crates.

| Frente | Onde começar | Evidência útil |
| --- | --- | --- |
| Renderização e fidelidade | `crates/victory-gpu`, `crates/victory-shader`, `crates/victory-formats` | Reprodução e comparação visual compatíveis com a política de dados. |
| Memória e streaming | `crates/victory-gpu` — recursos, texturas, residency e uploads | Uso de memória e filas na mesma cena. |
| Desempenho e concorrência | `crates/victory-gpu` — filas, cópias, submissões e sincronização | Tempo de quadro e método comparável antes/depois. |
| Compatibilidade D3D12 e estabilidade | `crates/victory-gpu`, `crates/victory-shader` e, quando pertinente, `crates/victory-hle` | Reprodução, GPU/driver, comportamento esperado e teste da correção. |
| Reprodução e validação | `tests/`, iniciador, documentação e ferramentas | Cenário, versão, resultados e limitações. |

## Abrir issues e propor alterações

Use os modelos de bug ou desempenho e preencha o que souber. Se uma informação estiver indisponível, diga o que falta. Para desempenho, registre a mesma cena ou percurso, duração, configuração, estado do cache e método de medição; uma captura isolada não demonstra ganho estável.

No PR, explique o problema, a mudança, suas evidências e os testes executados. Diferencie causa comprovada de hipótese. Relate testes ignorados ou impossíveis de executar, impactos visuais e de desempenho, e riscos conhecidos. Use o modelo de PR; itens não pertinentes podem ser marcados como não aplicáveis com uma explicação curta. Para mudanças que dependam do jogo ou da GPU, indique se a validação real foi feita ou está pendente.

Registre GPU e driver nos casos gráficos. Se uma correção for específica da NVIDIA, identifique o motivo e mantenha seu escopo explícito; se puder afetar AMD ou Intel, descreva o caminho afetado e teste quando houver hardware disponível. Não afirme compatibilidade sem evidência: anote o que não foi testado.

Ao adaptar código de fork, PR ou commit externo, informe links de origem, commit, autoria, licença e dependências relevantes; explique modificações e valide a compatibilidade com a licença do projeto. Divida mudanças amplas por comportamento quando possível; quando não, explique a dependência e como revisá-las ou revertê-las.

## Comandos e validação

No Windows, siga os pré-requisitos e a inicialização das tabelas descritos no [README](README.md). Comandos documentados no projeto:

```powershell
powershell -ExecutionPolicy Bypass -File .\Initialize-FormatTables.ps1
cargo build --release --locked -p victory
cargo test --workspace
cargo xtask policy
```

O CI em [`.github/workflows/ci.yml`](.github/workflows/ci.yml) inicializa as tabelas e executa a política de dados e `cargo check --workspace --all-targets --locked` no Windows. Não executa o jogo. Alguns testes especializados podem exigir GPU, validador ou dados locais; registre quando forem ignorados ou indisponíveis. Os comandos de execução e diagnóstico estão no README e em [docs/README-fxceless.md](docs/README-fxceless.md). Não execute o jogo nem modifique saves de outra pessoa sem autorização.

Uma revisão deve ser proporcional ao risco: documentação não precisa dos mesmos benchmarks que uma mudança de renderização, e um check automático não substitui validação visual ou de hardware quando relevante.
