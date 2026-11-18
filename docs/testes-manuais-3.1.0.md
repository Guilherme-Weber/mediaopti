# Testes manuais da versão 3.1.0

Testes que só uma pessoa na janela real consegue fazer: diálogos do Windows, arrastar e soltar,
Lixeira, Gerenciador de Tarefas, atualização pelo próprio app. O resto (substituição, verificação,
diário de substituições, caminhos longos, panics, falhas de ferramentas, Pause/Stop nos processos)
é coberto pelos 135 testes automáticos e pela execução em arquivos reais.

## Preparação

1. Extraia o zip candidato (`dist/mediaopti-3.1.0-win64.zip`, gerado pelo ensaio do `release.ps1`)
   em `D:\mediaopti-test\candidato-3.1.0`.
2. Refaça a amostra de trabalho e limpe o histórico de teste:
   `powershell -File D:\mediaopti-test\resetar-amostra.ps1`
3. Abra o candidato com a pasta de dados de teste (o histórico real não é tocado):
   `powershell -File D:\mediaopti-test\abrir-mediaopti-teste.ps1 -Exe D:\mediaopti-test\candidato-3.1.0\mediaopti.exe`

Nos testes que usam a Lixeira, troque em **Settings** o destino dos originais para
**Recycle Bin** e volte depois para a pasta `substituidos`.

## Testes

| # | Teste | Como fazer | Resultado esperado | OK |
|---|---|---|---|---|
| 1 | Escolher pasta | **Queue** > escolher pasta > `D:\mediaopti-test\amostra` | Diálogo do Windows abre; a fila mostra os arquivos e o resumo por tipo | ok |
| 2 | Arrastar e soltar | Arraste 2 arquivos soltos e uma subpasta da amostra para a janela | Entram na fila; nada duplicado | ok |
| 3 | Soltar durante um lote | Com um lote rodando, arraste outra pasta | O log diz "Added to the queue"; os novos arquivos são processados no mesmo lote | ok |
| 4 | Lixeira | Destino **Recycle Bin**, rode a amostra | Os originais substituídos aparecem na Lixeira; **Restaurar** devolve um deles ao lugar | ok |
| 5 | Pausa | Durante um vídeo longo, **Pause** | No Gerenciador de Tarefas, nenhum `ffmpeg.exe` fica rodando; a GPU volta ao repouso | ok |
| 6 | Retomar | Depois do teste 5, **Resume** | O vídeo recomeça do último passo concluído (se o encode com o preset já tinha terminado, o log diz "continuing with the quality retry") e termina | ok |
| 7 | Stop | **Stop** no meio de um vídeo | O lote para em segundos; o original fica intacto; no Visualizador de Eventos (Sistema) não aparece `nvlddmkm` | ok |
| 8 | Fechar durante um lote | Feche a janela com um lote rodando | A janela mostra "Stopping…" e fecha sozinha quando tudo termina | ok |
| 9 | About > Tools | Abra **About** | Cada ferramenta tem versão e origem; a linha de pingo/beeim aparece | ok |
| 10 | Advanced | **About** > **Advanced options** | Os cartões Remove data tracks, Remove metadata (e a opção de refazer) e as partes do esforço máximo aparecem com o aviso | ok |
| 11 | Configurações persistem | Mude 2 configurações, feche e reabra | As mudanças continuam; **Restore defaults** volta ao padrão | ok |
| 12 | Barra de tarefas | Rode um lote | O ícone na barra de tarefas mostra o progresso | ok |
| 13 | Sem suspensão | Rode um lote longo com o PC configurado para suspender cedo | O PC não suspende durante o lote | ok |
| 14 | Linha de comando | `mediaopti.exe --start "D:\mediaopti-test\amostra"` | Abre já processando a pasta, com as últimas configurações | ok |
| 15 | Uma instância só | Com o app aberto, abra-o de novo | Não abre uma segunda janela | ok |
| 16 | Pendrive exFAT | Rode uma cópia da amostra num pendrive exFAT | Arquivos substituídos normalmente; nenhum erro "could not copy its alternate data streams" | feito (abaixo) |
| 17 | Atualização 3.0.9 → 3.1.0 | **Só depois de publicar.** Abra o app instalado (3.0.9) > **About** > **Update and restart** | Baixa, confere o SHA-256, reinicia na 3.1.0; configurações e histórico mantidos; os `*.mediaopti-old` somem no início seguinte | [ ] |

## Resultados (2026-11-18, candidato 3.1.0)

- **1 a 4:** feitos na janela pelo autor, sem problemas.
- **5 a 15:** feitos por script, sem cliques reais: os botões foram acionados pelo UI
  Automation do Windows (o Slint expõe os controles pela acessibilidade) e a janela capturada
  com PrintWindow.
  - 5: nenhum `ffmpeg.exe` 2 s depois do Pause; GPU, encoder e decoder em 0%.
  - 6: Pause durante a retentativa com VMAF do vídeo de 87 min; no Resume o log disse
    "continuing with the quality retry" e o encode com o preset (22 min) não foi refeito. O
    resultado depois da retomada é conferido byte a byte pelo teste automático
    `pause_during_quality_retry_resumes_from_the_last_step`.
  - 7: parou em cerca de 1 s; o SHA-256 do original não mudou; nenhum arquivo temporário
    ficou para trás; nenhum evento `nvlddmkm` ou de vídeo no log Sistema.
  - 8: título "Stopping… · mediaopti", o encoder recebeu o pedido para sair e a janela fechou
    sozinha em 0,4 s.
  - 13: `CallNtPowerInformation(SystemExecutionState)` mostra `ES_SYSTEM_REQUIRED` durante o
    lote e 0 depois do Stop.
  - 15: a segunda instância só mostra "mediaopti is already running.".

## Já conferido automaticamente

- **16, pendrive exFAT** (2026-11-18): no exFAT, `FindFirstStreamW` devolve o erro 87, não o 38
  que o código anterior esperava, então toda substituição ali falhava. Depois da correção
  `ec73074`, a execução real (`real_folder_run`) numa cópia da amostra em `G:` (exFAT, sem o
  vídeo de 2,1 GB e o JPEG de 164 MP) deu o mesmo resultado que no NTFS: 37 arquivos, 10
  menores (vídeos, GIF, PDF, DOCX, PNGs e as conversões BMP/TGA → PNG), 26 mantidos, 1 falha
  esperada (o MP4 cortado pela metade), 10,6 MB a menos, nenhum erro de streams. Registro:
  `D:\mediaopti-test\baseline\exfat\`.
