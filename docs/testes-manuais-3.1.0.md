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
| 1 | Escolher pasta | **Queue** > escolher pasta > `D:\mediaopti-test\amostra` | Diálogo do Windows abre; a fila mostra os arquivos e o resumo por tipo | [ ] |
| 2 | Arrastar e soltar | Arraste 2 arquivos soltos e uma subpasta da amostra para a janela | Entram na fila; nada duplicado | [ ] |
| 3 | Soltar durante um lote | Com um lote rodando, arraste outra pasta | O log diz "Added to the queue"; os novos arquivos são processados no mesmo lote | [ ] |
| 4 | Lixeira | Destino **Recycle Bin**, rode a amostra | Os originais substituídos aparecem na Lixeira; **Restaurar** devolve um deles ao lugar | [ ] |
| 5 | Pausa | Durante um vídeo longo, **Pause** | No Gerenciador de Tarefas, nenhum `ffmpeg.exe` fica rodando; a GPU volta ao repouso | [ ] |
| 6 | Retomar | Depois do teste 5, **Resume** | O vídeo recomeça do último passo concluído (se o encode com o preset já tinha terminado, o log diz "continuing with the quality retry") e termina | [ ] |
| 7 | Stop | **Stop** no meio de um vídeo | O lote para em segundos; o original fica intacto; no Visualizador de Eventos (Sistema) não aparece `nvlddmkm` | [ ] |
| 8 | Fechar durante um lote | Feche a janela com um lote rodando | A janela mostra "Stopping…" e fecha sozinha quando tudo termina | [ ] |
| 9 | About > Tools | Abra **About** | Cada ferramenta tem versão e origem; a linha de pingo/beeim aparece | [ ] |
| 10 | Advanced | **About** > **Advanced options** | Os cartões Remove data tracks, Remove metadata (e a opção de refazer) e as partes do esforço máximo aparecem com o aviso | [ ] |
| 11 | Configurações persistem | Mude 2 configurações, feche e reabra | As mudanças continuam; **Restore defaults** volta ao padrão | [ ] |
| 12 | Barra de tarefas | Rode um lote | O ícone na barra de tarefas mostra o progresso | [ ] |
| 13 | Sem suspensão | Rode um lote longo com o PC configurado para suspender cedo | O PC não suspende durante o lote | [ ] |
| 14 | Linha de comando | `mediaopti.exe --start "D:\mediaopti-test\amostra"` | Abre já processando a pasta, com as últimas configurações | [ ] |
| 15 | Uma instância só | Com o app aberto, abra-o de novo | Não abre uma segunda janela | [ ] |
| 16 | Pendrive exFAT | Rode uma cópia da amostra num pendrive exFAT | Arquivos substituídos normalmente; nenhum erro "could not copy its alternate data streams" | feito (abaixo) |
| 17 | Atualização 3.0.9 → 3.1.0 | **Só depois de publicar.** Abra o app instalado (3.0.9) > **About** > **Update and restart** | Baixa, confere o SHA-256, reinicia na 3.1.0; configurações e histórico mantidos; os `*.mediaopti-old` somem no início seguinte | [ ] |

## Já conferido automaticamente

- **16, pendrive exFAT** (2026-11-18): no exFAT, `FindFirstStreamW` devolve o erro 87, não o 38
  que o código anterior esperava, então toda substituição ali falhava. Depois da correção
  `ec73074`, a execução real (`real_folder_run`) numa cópia da amostra em `G:` (exFAT, sem o
  vídeo de 2,1 GB e o JPEG de 164 MP) deu o mesmo resultado que no NTFS: 37 arquivos, 10
  menores (vídeos, GIF, PDF, DOCX, PNGs e as conversões BMP/TGA → PNG), 26 mantidos, 1 falha
  esperada (o MP4 cortado pela metade), 10,6 MB a menos, nenhum erro de streams. Registro:
  `D:\mediaopti-test\baseline\exfat\`.
