# Roteiro: 20 FPS com qualidade consistente

Meta experimental para cenas de referência e equipamento documentado; não é garantia para toda a campanha. A barra de 80% no README é uma estimativa subjetiva, separada do checklist.

## Referência atual

Há relato de ao menos 10 FPS correndo e capturas próximas de 12 FPS, mas faltam medições repetidas e padronizadas. A build local testada tem SHA-256 `F5F37738C01F587B417A7C962D3018C19B09CBBE216B71A17145C2F7E6E6DBE5`; não presumir equivalência com binários deste repositório. Testes recentes usaram Series S e `pareto-residency`, com BVH desligada. Registrar hardware, driver, resolução, escala, versão e cache.

Sombras em blocos e deformações do personagem foram observadas. Há avisos de memória, texturas e descritores, sem relação causal comprovada com os defeitos ou o desempenho. Desligar `card steps` reduziu a fluidez nos testes relatados.

## Medição e conclusão

Definir três cenas (parado, correndo e área externa), com três rodadas aquecidas de pelo menos dois minutos por cena e cache equivalente. Registrar mediana, percentis 95/99 de tempo de quadro, FPS pelo tempo total e 1% low. Comparar o custo da instrumentação. Não somar tempos sobrepostos entre filas ou threads.

Critério proposto para cada rodada e cena: média de pelo menos 20 FPS e 95% dos quadros em até 50 ms. Acompanhar 1% low, com 15 FPS como meta inicial sujeita à validação do método. Exigir ausência dos defeitos-alvo e de regressões funcionais graves. Documentar condições dinâmicas e exceções.

## Etapas

1. Fixar referência, cenas, configuração e medições.
2. Medir gargalos e a sobrecarga das ferramentas.
3. Diagnosticar separadamente sombras e personagem.
4. Testar otimizações uma hipótese por vez.
5. Integrar e repetir testes de imagem, desempenho, saves, controles, áudio e estabilidade.

As [issues do projeto](https://github.com/P1nkmanVI/P1nkman/issues) detalham as tarefas para colaboradores e agentes. Atualize o checklist do README apenas quando os critérios forem cumpridos com evidências.

## Privacidade

Não publique jogos, saves, dumps, shaders extraídos, caminhos locais, dados pessoais ou logs sem revisão. Use cópias para testes e não afirme causa ou ganho sem evidência.
