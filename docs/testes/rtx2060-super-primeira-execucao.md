# RTX 2060 SUPER: diagnóstico exploratório de gameplay

Este registro resume uma execução local para orientar a issue [#3](https://github.com/P1nkmanVI/P1nkman/issues/3). Logs completos, caches, saves, dumps de shaders e imagens permanecem fora do repositório, conforme `docs/asset-policy.md`.

## Ambiente e reprodução

- Windows 11, Ryzen 5 5600, RTX 2060 SUPER 8 GiB, 16 GiB RAM.
- Build release, perfil Xbox Series S, 1920×1080; pagefile ativo.
- Executável e dados do jogo fornecidos pelo usuário; save/cache usados em cópias locais.
- Captura do runtime por aproximadamente 12,5 minutos. O guardião de parada por RAM foi desativado para esta gravação a pedido do usuário; a sessão chegou a menos de 1 MiB de RAM física disponível. **Não reproduzir com a proteção desativada.**
- Para uma nova execução protegida, use `scripts/capture-session.ps1` com `-Build release -Console series-s -MaxMinutes 15 -MinFreeGiB 5` e caminhos para uma cópia dos dados/cache via `VICTORY_DATA_DIR` e `VICTORY_CACHE_DIR`.

## Evidência observada

Na cena noturna da cidade, o usuário caminhou e entrou em veículos. A captura de tela registra 15,5/15,9 GiB de RAM (97%), 436 MiB disponíveis, CPU 98%, GPU 88%, 8 FPS, 114,4 ms/quadro (pior 147 ms), fila de GPU 93%, 4.113 draws e 3.047 shaders novos. O gráfico de memória do Gerenciador de Tarefas permanece no topo durante o intervalo mostrado. A folhagem aparece rosa/roxa; sem referência visual, não se pode afirmar que seja defeito.

Na série de processo (672 amostras), memória privada chegou a 22,93 GiB e terminou em 21,74 GiB; working set chegou a 7,1 GiB e terminou em 4,9 GiB; commit do sistema terminou em 39,28 GiB. São medidas diferentes e não devem ser somadas. RAM física disponível chegou a menos de 1 MiB em amostras e estava em 0,95 GiB na última.

Os contadores GPU produziram 53 amostras e três erros de amostra inválida. Nelas, memória dedicada por processo chegou a 5,97 GiB e compartilhada a 6,96 GiB; não são cobertura contínua. Os contadores de disco ficaram em zero, mas a captura não valida que a leitura/gravação foi zero durante toda a sessão.

## Problemas observados no log

- Cinco shaders compute não traduziram: quatro falharam ao espelhar VGPR de lane-read sem valor dword suportado; um usou descriptor somente como dado sem lowering.
- 29 avisos de métodos de criação de pipeline sem implementação.
- 123 avisos de textura alterada após upload sem atualização de descritor.

Esses erros são candidatos concretos para falhas visuais e custo de pipeline/recópia, mas não provam isoladamente a causa da cor da folhagem, da RAM alta ou dos 8 FPS. O caminho de shader Wave64 alterado neste branch fez três programas de uma captura anterior serem emitidos e aceitos pelo DXIL validator e criação de PSO na RTX 2060 SUPER; a validação visual desses três e dos cinco shaders atuais ainda está pendente.

## Build e validação

- `cargo build --release --locked -p victory -j 2` concluiu.
- Binário medido na sessão: SHA-256 `5BBEC23C80186F512569A03D1BD9BC636085F1E798F2B61C9BB2D2FB27DA1DB4`.
- `cargo test -p victory-shader --locked emit::wave64_tests -- --nocapture --test-threads=1`: 11 passaram, 0 falharam; casos não suportados pelo hardware foram marcados como não executados.
- Não há comparação antes/depois sob a mesma cena. Nenhuma redução de RAM/VRAM ou melhoria de FPS foi demonstrada.

## Próxima etapa

Capturar externamente os cinco shaders compute que ainda falham; implementar e testar cada lowering sem aproximação global. Depois investigar as trocas de textura e os métodos de pipeline. Repetir na mesma cena/save, com proteção de memória ativa, medindo RAM física, memória privada, VRAM dedicada/compartilhada, disco e tempos de quadro. Confirmar visualmente personagens, vegetação e materiais antes de declarar correção.
