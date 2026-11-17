# Auditoria para o major release do mediaopti

Base: `main` em `507a2d1` (v3.0.12 + 2 commits só de testes/scripts), auditada em 2026-11-17.
Fases 0 a 2: nenhum código alterado; este relatório é a única escrita.

Convenções: severidade **crítico / alto / médio / baixo**; esforço **P / M / G**; "breaking" = muda
formato persistido, comportamento padrão ou interface que o usuário já conhece. Cada item do
checklist foi tratado como pergunta: o que está correto aparece como **OK** com a evidência.

## Fase 0: Baseline

### Versões e ferramentas

| Item | Valor |
|---|---|
| Versão do app | 3.0.12 (Cargo.toml:3); instalada em `C:\Programs\mediaopti`: 3.0.9 |
| Edition / MSRV | 2021 / **não declarada** (sem `rust-version`; ver 2.4-12) |
| Toolchain usado | rustc 1.99.1, cargo 1.99.1 |
| Slint | 1.18.1 (renderer de software, backend winit, `unstable-winit-030`) |
| Perfil release | `codegen-units = 1`, `lto = true`, `opt-level = 3`, `panic = "abort"`, `strip = true` (Cargo.toml:47-52) |
| CI | Não existe |
| `cargo audit` / `outdated` / `machete` | **Não instalados.** Sugestão: `cargo install cargo-audit cargo-outdated cargo-machete cargo-bloat` (e `cargo-deny` para licenças) |

### Build, testes e lint

| Comando | Resultado | Tempo |
|---|---|---|
| `cargo build --release` | OK | 167 s |
| `cargo test` | **96 passaram, 0 falharam, 11 ignorados** | 51 s |
| `cargo clippy --all-targets --locked -- -D warnings` (política do projeto) | Limpo | 10 s |
| `cargo clippy --all-targets -- -W clippy::pedantic -W clippy::unwrap_used -W clippy::expect_used` | 1.621 avisos (abaixo) | 18 s |

Avisos do pedantic, por tipo:
- **1.359 `unwrap`/`expect`**: 1.188 fora de `engine_tests.rs`, quase todos em blocos `#[cfg(test)]`; os 29 de produção estão classificados em 2.4;
- 42 itens de doc sem crase;
- 102 casts (perda de precisão, truncamento ou sinal; nenhum em caminho com risco, ver 2.4);
- 19 `match` sobre bool/Option;
- 14 imports com curinga (`use super::*` dos submódulos do engine, de propósito);
- 15 funções com mais de 100 linhas (2.4-13);
- 3 "possible infinite iteration" (`(1..)`/`(2..)`, invariantes);
- 2 comparações de extensão sensíveis a maiúsculas (archive.rs:609 e system.rs:939, ambas em testes).

### Desempenho (corpus de mídia)

**Corpus:** `D:\mediaopti-test\amostra-original` (montado em 2026-11-17 a partir de cópias da biblioteca; lista e origem de cada arquivo em `D:\mediaopti-test\LEIAME.txt`). São 41 arquivos (2,29 GB):
- imagens: PNG, JPEG, GIFs animado/parado, BMP, TGA, TIFF;
- vídeos: H.264, MOV, MKV AV1, WebM VP9, AVI DivX, um MP4 de 84×120 e um QuickTime VR;
- áudio: FLAC com/sem capa, MP3 com capa, WAV PCM/ADPCM, OGG, M4A;
- documentos: PDFs, DOCX, CBZ, CBR;
- casos problemáticos: JPEG e MP4 truncados, PNG de 0 byte, PNG com nome `.jpg`, somente leitura, caminho de 389 caracteres, pasta e nome acentuados, JPEG de 164 MP e o MP4 AV1 de 2,1 GB / 87 min.

**Método:** `real_folder_run` em release (`MEDIAOPTI_BENCH_JOBS=4`, AV1 NVENC, CRF 20, imagens e áudio visualmente sem perda, conversões ligadas, originais para pasta de backup, histórico vazio), sobre uma cópia nova do corpus. Memória amostrada a cada 1 s (commit do sistema e memória privada da árvore do processo de teste, filhos incluídos). Scripts e logs em `D:\mediaopti-test\baseline\` (`run-baseline.sh`, `monitor.ps1`). O vídeo de 2,1 GB roda **à parte**, porque o retry com VMAF sobre 87 min levaria horas e tornaria o baseline difícil de repetir.

| Métrica | Corpus (40 arquivos, 250 MB) | Vídeo de 2,1 GB |
|---|---|---|
| Tempo total | **877 s** | em andamento na entrega deste relatório |
| Arquivos | 38 entram na fila (o PNG de 0 byte e o TIFF não) | — |
| Resultado | 10 menores, 27 mantidos, 1 falha (o MP4 truncado, esperado) | — |
| Redução de tamanho | 262.553.817 → 251.471.551 bytes (**−11,1 MB, −4,2%**) | — |
| Pico de memória privada da árvore de processos | **23.218 MB** (sustentado entre 13:57 e 14:00) | — |
| Pico de commit do sistema | 37.829 MB (início: 15.387 MB) | — |
| Binário release | 15.068.672 bytes (14,4 MiB); PDB separado de 6,3 MB | — |

Leitura dos números:
- **Redução baixa** porque o corpus vem de uma biblioteca já processada: os vídeos grandes já são AV1 (batchviproc) e as imagens provavelmente passaram pelo imageopti. Todos os 16 arquivos de imagem saíram "already optimal", nenhum com o passe com perda. O caminho com perda é coberto e passou em `png_and_jpeg_lossless_and_lossy` (mod.rs:897). Para medir ganho, o próximo corpus deve ter um subconjunto nunca processado.
- **Pico de 23 GB de memória privada: ACHADO 0-1** (abaixo).

**Comportamento dos casos problemáticos (execução real):**

| Caso | Resultado | Avaliação |
|---|---|---|
| MP4 truncado em 50% | "Corrupt/unreadable file remembered in history (will not retry)" | OK |
| JPEG truncado em 55% | "Kept (already optimal)" | Seguro (não substituído), mas a mensagem engana: ver 2.5-10 |
| PNG de 0 byte | Não aparece em lugar nenhum | Confirma 2.5-4 |
| PNG com nome `.jpg` | Otimizado como PNG, −6% (correção de extensão desligada, que é o padrão) | OK |
| Somente leitura | Mantido (não ficou menor) | Substituição de read-only **não exercitada** aqui; coberta por teste (system.rs:875-969) |
| Caminho de 389 caracteres (JPEG) | Processado sem erro, mantido | Imagem OK; vídeo e áudio em caminho longo e a Lixeira **não exercitados** (2.5-3) |
| Pasta e nome acentuados (PNG) | −8%, substituído, original no backup | OK |
| JPEG de 164 MP | Mantido | Ver 0-1 |
| QuickTime VR (.mov) | "Skipped ... QuickTime VR panorama" | OK |
| M4A (AAC) | "Skipped: aac is not lossless" | OK |
| TIFF | Não entra na fila | OK (decisão registrada) |
| BMP / TGA | Convertidos para PNG, −77% / −87% | OK |
| GIF animado de 5,4 MB | Virou AV1, −95% | OK |
| MP4 de 84×120 | x264 + retry VMAF, −20% | OK |
| WAV ADPCM | Mantido (não é PCM inteiro) | OK |

**0-1. Pico de memória sem atribuição**
- Onde: admissão em images.rs:19 e 218-238 (2 GB fixos por arquivo de CPU), vmaf.rs:452, video.rs:728.
- O que aconteceu: a árvore de processos chegou a 23,2 GB de memória privada, e o commit do sistema subiu 22,4 GB acima do início. Hoje havia ~34 GB de commit livre. Com os ~8 GB livres registrados no CLAUDE.local para esta máquina, a falta de commit teria abortado o processo, matando NVENC à força.
- Suspeitos: o JPEG de 164 MP (ssimulacra2/decodificação em float de 164 MP × 3 canais) ou os retries VMAF simultâneos (3 vídeos com VMAF na mesma janela).
- **Não medi isoladamente.** Isso exigiria rodar enquanto o encode NVENC do vídeo grande está em curso, e um abort por falta de commit nessa hora seria exatamente o cenário proibido.
- Severidade alto · Esforço M · Breaking não.
- Correção: primeiro **medir isolado** (o JPEG de 164 MP sozinho com `bench_folder`; depois 3 VMAF simultâneos), e depois ajustar a admissão do culpado (2.5-9: estimativa pela área; ou VMAF contando a resolução). Entra como A0 no plano.


## Fase 1: Inventário

### 1.1 Mapa de funcionalidades

| Funcionalidade | O que faz | Onde (entrada) | Entrada → saída | Ferramentas externas |
|---|---|---|---|---|
| Encode de vídeo (NVENC AV1/HEVC/H.264, x264) | Um job por vídeo: probe, encode no preset, verificação, substituição | video.rs:49 `process_one`; ffmpeg.rs:249 `encode` (fallbacks); ffmpeg.rs:35 `run_attempt`; args.rs:534 `build_ffmpeg_args` | vídeo → `.mp4`/`.mkv` (args.rs:272) | ffmpeg (tokio::process), nvidia-smi |
| Preservação de faixas, tags e HDR | Probe antes e depois; perder faixa, capítulo, tag ou duração conta como falha | ffmpeg.rs:351 `probe_streams`; ffmpeg.rs:449 `streams_lost`; args.rs:237 `Streams::blocker`; args.rs:678 `tag_flags`; args.rs:691 `copy_audio`; ffmpeg.rs:382 `same_audio_track` | — | ffmpeg |
| Nova tentativa com VMAF | Resultado não menor → encodes mais fortes, aceitos só com VMAF ≥ 93 (até 1080p) | vmaf.rs:99 `quality_retry`; vmaf.rs:317 `measure_vmaf`; video.rs:402 | — | ffmpeg libvmaf |
| GIF → vídeo | GIF animado grande vira AV1, conferindo nº de quadros e duração | ffmpeg.rs:401 `video_timing`; ffmpeg.rs:503 `timing_mismatch`; video.rs:447 | `.gif` → vídeo | ffmpeg |
| GIF tratado como imagem (fallback) | Se o vídeo não ficou menor, o GIF vai para o gifsicle | video.rs:25 `gif_fallback` → images.rs:176 `gif_as_image`; video.rs:40 | GIF → GIF | gifsicle |
| Cadeia PNG | Sem perda + busca com perda (SSIMULACRA2 ≥ 93) | image/mod.rs:472 `optimize`; mod.rs:589 `png_chain`; mod.rs:750 `lossy_png`; mod.rs:698 `quality_search` | PNG → PNG | pingo, beeim, oxipng, ect, ssimulacra2, ffmpeg |
| Cadeia JPEG | Sem perda (jpegtran/pingo) e reencode com perda | mod.rs:646 `jpg_chain`; mod.rs:797 `lossy_jpeg`; jpeg.rs:106 `meta_ok`, jpeg.rs:128 | JPEG → JPEG | jpegtran, pingo, cjpegli, cjpeg, djpeg, ssimulacra2 |
| GIF (imagem) | `gifsicle -O3` | mod.rs:498-510 | GIF → GIF | gifsicle |
| Verificação de pixels | Decodifica na profundidade nativa e compara | verify.rs:107 `same_pixels`; verify.rs:144 `ok` | — | ffmpeg, djpeg |
| Dados após o fim da imagem | Pula motion photo, MPO e gain map | mod.rs:149 `has_data_after_end` (mod.rs:485) | — | — |
| Chunks PNG | Repõe cor e texto descartados pelo beeim | png.rs:140 `restore_chunks` | — | — |
| BMP/TGA → PNG | Pixels idênticos, depois a cadeia PNG | mod.rs:542 `to_png` | → PNG (`new_ext`) | ffmpeg + cadeia PNG |
| FLAC | Capa pela cadeia de imagem; reencode de encoders antigos; áudio conferido | audio.rs:680 `flac`; audio.rs:92 `optimize_art`; audio.rs:284 `same_audio` | FLAC → FLAC | flac, cadeia de imagem |
| MP3 | Capa APIC; frames de áudio conferidos | audio.rs:825 `mp3`; audio.rs:923 `mp3_frames_match` | MP3 → MP3 | cadeia de imagem |
| Ogg Vorbis | Repack sem perda | audio.rs:411 `ogg` | OGG → OGG | optivorbis |
| WAV → FLAC | Só PCM inteiro | audio.rs:643 `wav` | → FLAC | flac |
| ALAC/AIFF/APE/WV/TTA → FLAC | `Kind::LosslessAudio`; M4A com AAC fica como está | audio.rs:516 `lossless_to_flac` | → FLAC | ffmpeg, flac |
| PDF | Reescrita com qpdf, JPEG sem perda via JSON, passe com perda conferido por render das páginas | pdf.rs:85 `optimize`; pdf.rs:229; pdf.rs:150 `pages_look_same` | PDF → PDF | qpdf, mutool, jpegtran, cjpegli, ssimulacra2 |
| Office/ODF/EPUB/CBZ | Repack do zip com as imagens otimizadas; páginas em paralelo | archive.rs:28 `optimize`; archive.rs:212 `repack`; archive.rs:127; archive.rs:192 `verify` | zip → zip | 7z, cadeia de imagem |
| CBR → CBZ | Extrai com 7-Zip, grava CBZ | archive.rs:372 `cbr` | CBR → CBZ | 7z |
| Tipos e estágios | `Kind`, `kind_of`, estágios áudio/docs/imagens | mod.rs:30, 73, 87; queue.rs:6; images.rs:82 | — | — |
| Histórico por local | SHA-256 do caminho (imagens: caminho + modo) | history.rs:81-86, 124, 216; records.rs:49 | `processedFiles.txt` | — |
| Histórico por conteúdo | BLAKE2b-128, formato do imageopti | history.rs:95; records.rs:67, 77, 95 | `processed-content.tsv` | — |
| Importações batchviproc/imageopti | Uma vez, na inicialização | main.rs:27-29 → history.rs:317, 328 | — | — |
| Destino dos originais | Lixeira, pasta de backup ou apagar | queue.rs:80-87; system.rs:401 `recycle`; system.rs:554 `dispose`; system.rs:601 | — | Windows Shell |
| Substituição | Flush, nota de conteúdo, troca com retry, datas e atributos | replace.rs:83 `replace_original`; system.rs:630; system.rs:718 | — | — |
| Nome ocupado / mesmo conteúdo | Reserva o nome; com conteúdo igual fica o menor, senão "(2)" | system.rs:488 `claim_new_name`; system.rs:566; images.rs:565, 751; video.rs:620, 636 | — | — |
| Corrigir extensão errada | PNG/JPG/GIF/BMP | images.rs:595; mod.rs:162; args.rs:318 | — | — |
| Remover metadados | `Ctx::strip`, modos `-strip` no histórico | images.rs:162; mod.rs:363; queue.rs:60; history.rs:36 | — | — |
| Pausar / retomar | Encerra processos; vídeo retoma do último passo | batch.rs:325, 379; queue.rs:189-195; vmaf.rs:64; ffmpeg.rs:525 `ask_to_quit`; tools.rs:264 `Gate` | — | NtSuspendProcess (system.rs:154) |
| Parar / fechar | Cancela tokens; espera os encoders | batch.rs:298 `stop`; batch.rs:388 `shutdown` | — | — |
| Arrastar e soltar | winit `DroppedFile` + Timer de 250 ms | app.rs:1153-1190 → scan.rs:179, 186 | — | — |
| Atualizador | GitHub `releases/latest`, SHA-256 do zip, troca tudo ou nada, reinicia | update.rs:51, 104, 131, 219, 255, 277; app.rs:1274, 1300 | — | — |
| Versões das ferramentas, pingo/beeim | Lista versões; checagem semanal; instala pelo botão | toolver.rs:127, 171, 212; tools.rs:105 `download_missing`; app.rs:1229 | — | curl (tools.rs:160) / URLMoniker (tools.rs:184) |
| Descoberta e execução de ferramentas | tools.json → `tools\` → dados → `bin` → PATH; prioridade baixa, job, timeout, Pause/Stop, captura limitada | tools.rs:41 `discover`; tools.rs:26 `shim_target`; tools.rs:381 `run` | — | — |
| Decodificação na GPU | H.264/HEVC/AV1/VP9 4:2:0; falha refeita na CPU | ffmpeg.rs:332, 540; ffmpeg.rs:249 | — | NVDEC |
| Admissão de memória, VRAM e disco | Espera memória livre e encoders; `WorkDir` reserva ~4x | batch.rs:8, 18; video.rs:728; ffmpeg.rs:307; images.rs:218, 442, 473-520; vmaf.rs:452; system.rs:214, 324 | — | nvidia-smi |
| Limpeza de órfãos | Temporários `.mediaopti.*`, pastas de trabalho, placeholders | scan.rs:75, 103-107; replace.rs:17, 48; system.rs:275, 510, 524 | — | — |
| Segurança contra crash | Flush e nota antes de tirar o original; reescritas atômicas; job kill-on-exit | replace.rs:85-93; system.rs:581; history.rs:280, 305; system.rs:103 | — | — |
| Hard links / arquivo alterado | Deixados como estão | video.rs:76, 540; images.rs:600-603, 667; system.rs:453; scan.rs:32 | — | — |
| Instância única, PowerGuard, barra de tarefas, log | Mutex nomeado; `SetThreadExecutionState`; progresso no ícone; log com rotação | main.rs:34-48; system.rs:14-24, 346; taskbar.rs:22; logfile.rs:21, 58 | — | — |
| Configurações | `Prefs` com `serde(default)`; `normalize`; `apply_options` no meio do lote | app.rs:169, 186, 278; args.rs:411; batch.rs:53 | `ui-settings.json` | — |
| Parse de progresso | Progresso, watchdog de travamento, diagnósticos (OOM) | progress.rs:94 | — | — |

### 1.2 Fluxo de um lote, de ponta a ponta

1. **Seleção:** `on_choose_folder` (app.rs:986), arrastar e soltar (app.rs:1176) ou linha de comando (app.rs:1197-1205). Todos chegam a `load_items` (scan.rs:186).
2. **Scan:** `scan_folder` (scan.rs:64) em `spawn_blocking`. Inclui limpeza de órfãos (scan.rs:75, 103) e detecção de GIF animado (system.rs:796). Um ticker atualiza a UI a cada 300 ms (scan.rs:245).
3. **Reconhecimento por conteúdo:** `recognise_by_content` (records.rs:95, chamado em scan.rs:275).
4. **Fila:** `rebuild_queue` (queue.rs:141) aplica `place` (queue.rs:104) a cada arquivo e separa vídeo → `pending_queue`, CPU → `cpu_queues[stage]`, e o resto em Known/Off/Generated/Failed. Durante um lote, os arquivos novos são anexados (scan.rs:293-313).
5. **Início:** `on_start` (app.rs:1006) → `run_batch` (batch.rs:72): `normalize`, `rebuild_queue` (batch.rs:89), tokens `cancel`/`release`, `PowerGuard`, `spawn_workers` (batch.rs:229: N workers de vídeo e `cores-2` de CPU) e polling da GPU.
6. **Claim:** vídeo em `claim_next` (queue.rs:223); CPU em `claim_cpu` (images.rs:111), com estágios rolantes (`stage_limits`).
7. **Vídeo:** `process_one`, nesta ordem:
   - `file_state` e hard link (video.rs:76);
   - probe e `blocker` (video.rs:140-146);
   - resume (video.rs:181);
   - `encode` (video.rs:205);
   - `quality_retry` (video.rs:402) ou `gif_fallback` (video.rs:430).
8. **CPU:** `process_image` (images.rs:585) em `spawn_blocking`, nesta ordem:
   - `file_state` e hard link;
   - `WorkDir` (images.rs:622);
   - `image::optimize` (images.rs:640);
   - nova tentativa de OOM até 2x (images.rs:301-309).
9. **Validação:**
   - vídeo: `timing_mismatch`, `streams_lost`, `same_audio_track` e `file_state` de novo (video.rs:447-540);
   - CPU: `Verifier::ok`/SSIMULACRA2 em cada passo, `file_state` de novo (images.rs:667) e `claim_new_name` (images.rs:676).
10. **Substituição:** `replace_original` (replace.rs:83) faz `flush_to_disk` → `note_content_now` → `replace_file_with_retry` (Lixeira, backup ou apagar), depois atributos.
11. **Histórico:** `remember` (records.rs:77) ou `finish_image` (images.rs:322); gravação com atraso (records.rs:26) e `flush_history` no fim (batch.rs:174).
12. **Fim:** espera todos os handles (batch.rs:156-172), depois `finish_batch` (batch.rs:197).
13. **UI:** `send_state` (engine.rs:361, no máximo a cada 250 ms) → `SlintUi::state` (app.rs:342) → `Inbox` + `invoke_from_event_loop` (app.rs:327) → `Controller::drain` (app.rs:446) → `render` (app.rs:509).

### 1.3 Mapa de threads

| Onde | O que roda | Arquivo:linha |
|---|---|---|
| Thread principal (event loop do Slint) | `window.run()`, callbacks `on_*`, `Controller` em `thread_local`, eventos winit, Timer de drop, diálogos rfd modais | app.rs:784-1215, 348, 1159-1180 |
| Runtime tokio | `new_multi_thread`, **2 worker threads**, `shutdown_timeout(2s)` | app.rs:785-790, 1214 |
| Tarefas tokio | `run_batch`, um `run_worker` por vídeo, `run_image_worker`, polling da GPU, tickers (1 s; scan 300 ms), throttle de estado, gravação do histórico, leitor de stderr do VMAF | batch.rs:69, 136, 237, 266, 414; scan.rs:245; engine.rs:371; records.rs:35; vmaf.rs:383 |
| Pool blocking do tokio | Limite padrão (512). Scan, `apply_options_in_background`, `Engine::blocking`. **Cada job de CPU ocupa uma thread blocking durante o arquivo inteiro** | engine.rs:343; scan.rs:253; batch.rs:44; images.rs:190, 288 |
| Threads std | `PowerGuard`; downloads, versões e updater; **2 leitoras por chamada de `tools::run`** | system.rs:22; app.rs:849, 872, 885, 1276, 1309; tools.rs:417, 443; toolver.rs:79 |
| `std::thread::scope` | Hash paralelo, scan, páginas de quadrinhos, hash de áudio, versões | records.rs:13; scan.rs:135; archive.rs:140; audio.rs:291; toolver.rs:138 |
| Processos filhos | ffmpeg (tokio::process); nvidia-smi; ferramentas de CPU (std::process, job kill-on-exit, prioridade baixa); curl; relançamento | ffmpeg.rs:57, 342-402; vmaf.rs:368; batch.rs:19, 415; tools.rs:392; system.rs:301, 112; update.rs:289-296 |
| Comunicação | `Arc<Engine>` + `Mutex<Inner>` (tolerante a poison, engine.rs:338); atômicos; `watch` de pausa (engine.rs:247 → `Gate`); `CancellationToken` `cancel` → `release` (batch.rs:101-102); trait `Ui` → `Inbox` → `invoke_from_event_loop` + `Weak::upgrade` (app.rs:330, 857, 1231...); `Slots` (Mutex/Condvar, tools.rs:334-375) | — |

Não há `upgrade_in_event_loop` no código. O padrão usado, que é equivalente, é `invoke_from_event_loop` seguido de `weak.upgrade()`.

### 1.4 Cobertura de testes

São 107 testes, dos quais 11 são `#[ignore]` (pedem variáveis de ambiente, rede, a Lixeira ou GPU). Dois cuidados:

- Os testes de `engine_tests.rs` dão `return` em silêncio quando `bin/ffmpeg.exe` falta. Os de imagem, áudio, PDF e archive imprimem "skipping" quando falta uma ferramenta. Nos dois casos o teste **passa sem testar nada** (ver 2.6).
- 30 dos testes e2e usam `av1_nvenc` e exigem GPU NVIDIA.

| Funcionalidade | Testes | Ignorados |
|---|---|---|
| Lote inteiro | engine_tests.rs:93 `full_batch_handles_every_file_kind`, :967, :491 | — |
| Args, streams, tags, MKV | args.rs:722-973 (11 testes); ffmpeg.rs:569; e2e :711, :763, :808 | args.rs:815 (ARGS_DUMP) |
| VMAF / resume | vmaf.rs:474; e2e :919, :862 | — |
| GIF → vídeo / fallback | ffmpeg.rs:595; e2e :571, :611, :667; mod.rs:1062 | — |
| PNG/JPEG, strip, dados após o fim | mod.rs:897, 939, 1097, 1139; png.rs:232, 253; jpeg.rs:278-304; verify.rs:170 | mod.rs:1036 (MEDIAOPTI_TEST_IMAGES) |
| BMP/TGA → PNG | mod.rs:1151 | — |
| Áudio | audio.rs:973, 1013, 1056, 1097, 1139; e2e :1188 | — |
| PDF | pdf.rs:419 | pdf.rs:461 (MEDIAOPTI_TEST_PDFS) |
| Office/CBZ | archive.rs:525, 576 | — |
| Histórico / importações | history.rs:364-476 (6) | — |
| Backup, Lixeira, substituição, nomes | e2e :1130, :1259; system.rs:875, 912, 969; replace.rs:131, 156 | system.rs:898 (Lixeira) |
| Pausar / parar | e2e :238, :290, :349, :447; tools.rs:565 | e2e :196 (NVENC quit), :405 |
| Fila, estágios, drop | e2e :1356, :1408, :1462 | — |
| Memória, disco, slots | batch.rs:504; tools.rs:536, 610; progress.rs:201; images.rs:791 | — |
| Limpeza de órfãos | system.rs:930, 952; replace.rs:131 | — |
| GPU decoding | args.rs:795; ffmpeg.rs:613 | — |
| Updater | update.rs:317-436 (6) | update.rs:412 (rede) |
| Ferramentas, versões | tools.rs:549; toolver.rs:230, 271 | tools.rs:591, toolver.rs:252 (rede) |
| UI/Prefs, log, progresso, instância única | app.rs:1469-1510 (5); logfile.rs:81; progress.rs:174, 187; system.rs:990, 857, 865 | — |
| Benchmarks / execução real | — | e2e :1078 `bench_folder`, :1314 `real_folder_run` |

**Sem teste automatizado:**

- CBR → CBZ (archive.rs:372);
- `MEDIAOPTI_DIE_AT` (system.rs:89-93): só conferido manualmente;
- `toolver::install_newer`;
- `Engine::clear_history` (records.rs:138);
- a opção `drop_data` de ponta a ponta;
- o fallback GPU → CPU e a espera por OOM em `encode` (ffmpeg.rs:249-330);
- polling da GPU e `free_vram_mb`;
- `apply_options` no meio de um lote (batch.rs:53);
- `PowerGuard`, barra de tarefas, o handler winit de drop e `Prefs::from_window`/`apply_to_window`.

## Fase 2: Auditoria

### 2.1 UI Slint e threading

| Item | Status | Evidência |
|---|---|---|
| Worker → UI pelo event loop | OK | `SlintUi::log/state` gravam numa `Inbox` (Mutex) e chamam `schedule()`, um único `invoke_from_event_loop` coalescido por `AtomicBool` (app.rs:314-347); o `drain` roda na thread da UI (app.rs:446-465). toolver (app.rs:849-865, 1229-1271) e update (app.rs:1274-1330) usam thread + `invoke_from_event_loop`. |
| taskbar só na UI, sem COM repetido | OK | Chamado só em `render` (app.rs:702-710); `Taskbar::set` ignora valor repetido (taskbar.rs:43-45, 65). |
| Rede e processos fora da UI | OK | `update::check/install` em `std::thread::spawn` (app.rs:1276, 1309); `toolver::*` em threads (app.rs:872, 885). Diálogos `rfd` modais na UI (padrão). |
| Settings salvas sem I/O pesado | OK, com ressalva | `Prefs::save` grava um JSON pequeno na UI (app.rs:186-190); opções aplicadas em `spawn_blocking` com geração (batch.rs:40-49). Ver 2.1-5. |
| Callbacks `on_*` sem trabalho bloqueante | ACHADO | `on_forget_history` → `clear_history` síncrono (app.rs:1027-1043, records.rs:138-157); `on_clear_queue` (app.rs:1057, queue.rs:338-365). Ver 2.1-1 e 2.1-3. |
| Contenção do lock `Inner` vinda da UI | ACHADO | `stop`, `set_paused`, `clear_queue` e `queue_preview` travam `Inner` na UI (batch.rs:300, 327; queue.rs:369) enquanto o motor segura o lock em trabalho O(N) e I/O. Ver 2.1-3. |
| `slint::Timer` | OK | Um só SingleShot de 250 ms que junta drops (app.rs:1159, 1180-1185). |
| Throttling do estado | OK | `send_state(false)` no máximo a cada 250 ms, com reenvio adiado (engine.rs:37, 360-384); ticker do scan a cada 300 ms (scan.rs:243-251); a `Inbox` guarda só o último `StateDto` (app.rs:344, 448). |
| Log da UI limitado | OK | `MAX_LOG_LINES = 500` (app.rs:21, 455-458); progresso de CPU resumido a cada `PROGRESS_EVERY` (images.rs:426-435); log em arquivo limitado a 5 MB (logfile.rs:12). |
| Modelos atualizados de forma incremental | OK, com ressalva | `sync_model` atualiza linha a linha (app.rs:729-739). Exceção: `queue_counts` (2.1-4). |
| Prévia da fila não é refeita a cada tick | OK, com ressalva | Só refeita quando `(queue_revision, running)` muda, nunca durante a execução (app.rs:488-493); limite de 5000 itens (app.rs:1393) numa `ListView` virtualizada (app.slint:719-730). Custo O(N) em 2.1-2. |
| Componentização `.slint` | OK, com ressalva | 17 componentes reutilizáveis (app.slint:63-426), mas as 4 páginas ficam inline num `AppWindow` de ~1100 linhas (2.1-7). |
| Layout responsivo e escala | OK | `min-width/height` (app.slint:444-445); métricas em 1 ou 2 linhas abaixo de 1220 px (app.slint:738-753); px lógicos (DPI pelo Slint). |
| Estado visual separado da lógica | OK | `Controller::render` (app.rs:509-711) calcula textos; o `.slint` guarda só estado visual (app.slint:576, 810-822). |
| Lógica testável sem UI | OK | Trait `Ui` (engine.rs:61-65), `RecorderUi` nos testes (engine_tests.rs:11-23). |

**2.1-1. `clear_history` roda na thread da UI e percorre a biblioteca inteira**
- Onde: app.rs:1040-1042 → records.rs:138-157.
- O que acontece: depois da confirmação, trava `Inner` e chama `rebuild_queue()`. Para cada arquivo, `place()` calcula SHA-256 do caminho (até 4 por imagem: history.rs:156-168, queue.rs:104-172). Em seguida, `flush_history()` grava em disco ainda com o lock seguro (history.rs:224-229).
- Impacto: com ~1,3 mi de arquivos carregados, a janela congela por segundos (estimativa pelo código, não medida).
- Severidade médio · Esforço P · Breaking não.
- Correção: rodar em `spawn_blocking`, como `apply_options_in_background` (batch.rs:40-49).

**2.1-2. `queue_preview` varre todos os arquivos com o lock seguro, na UI**
- Onde: queue.rs:377-380, chamado de app.rs:494.
- O que acontece: para achar o tamanho de até 5000 itens, filtra `inner.files` inteiro (1,3 mi de lookups em `HashSet<PathBuf>`). Isso se repete a cada `queue_revision` novo: cada mudança de opção com pasta carregada (batch.rs:59-64) e cada scan.
- Impacto: pausa perceptível na UI e bloqueio dos workers a cada mudança de configuração (estimativa).
- Severidade médio · Esforço P · Breaking não.
- Correção: guardar o tamanho junto do caminho nas filas (`(PathBuf, u64)`, já conhecido em `rebuild_queue`), o que deixa a prévia O(limite).

**2.1-3. Callbacks da UI disputam o lock de `Inner` com seções O(N) e com I/O do motor**
- Onde:
  - Callbacks que travam `Inner`: Stop e Pause (batch.rs:300, 327), Clear queue (queue.rs:340), prévia (queue.rs:369).
  - Seções longas do motor:
    - `rebuild_queue` dentro de `run_batch`, num worker async (batch.rs:87-89);
    - merge de drop durante a execução, que clona todos os `inner.files` num `HashSet` sob o lock (scan.rs:283-315, 328-335);
    - `flush_history`, que grava em disco com o lock seguro (records.rs:41-47) e ocupa um dos 2 workers do runtime (app.rs:785-786).
- Impacto: Stop ou Pause logo depois de Start, ou durante o drop de uma pasta grande, congelam a janela até a seção terminar.
- Severidade médio · Esforço M · Breaking não.
- Correção:
  - (a) `rebuild_queue` e o merge em `spawn_blocking`.
  - (b) Índice `HashSet` persistente em `Inner`, em vez de reconstruí-lo a cada drop.
  - (c) `flush_history` tira os dados pendentes com `mem::take` e grava fora do lock.
  - (d) Stop e Pause sinalizam primeiro pelo token, sem lock.

**2.1-4. `queue_counts` recria o `ModelRc` a cada render**
- Onde: app.rs:477-487 (app.slint:697).
- O que acontece e impacto: os 4 `CountTile` são destruídos e recriados a cada ~250 ms durante o scan. Custo pequeno, mas inconsistente com `sync_model`.
- Severidade baixo · Esforço P · Breaking não.
- Correção: um `Rc<VecModel>` fixo no `Controller`, atualizado com `sync_model`.

**2.1-5. `Prefs::save` não é atômico e um JSON ilegível volta ao padrão em silêncio**
- Onde: app.rs:186-190 (`fs::write`) e 169-171 (erro de parse → `Self::default()`).
- O que acontece: um kill ou queda de energia durante a gravação trunca o JSON. No próximo start, todas as opções voltam ao padrão sem nenhum log, inclusive `originals` e `backup_dir`. Isso é relevante para a execução grande com pasta de backup.
- Severidade baixo · Esforço P · Breaking não.
- Correção: usar `write_atomically` (history.rs:226) e registrar no log "settings unreadable, defaults used".

**2.1-6. Corte do log com `remove(0)` em loop**
- Onde: app.rs:455-458.
- O que acontece: custo O(k·500) e k notificações para a `ListView` numa rajada de k linhas. Hoje é raro (o log já vem resumido).
- Severidade baixo · Esforço P · Breaking não.
- Correção: `set_vec` com as últimas 500 linhas quando o teto é passado.

**2.1-7. `app.slint` (1558 linhas) e `app.rs` (1530) monolíticos**
- Onde: páginas inline em app.slint:621, 859, 1214 e 1351; `run` com ~430 linhas e todos os `on_*` (app.rs:784-1216).
- Impacto: só manutenção.
- Severidade baixo · Esforço M · Breaking não.
- Correção: separar as páginas em arquivos `.slint` próprios e dividir `app.rs` em `prefs.rs`, `controller.rs` e `callbacks.rs`. É refatoração pura e fica na Fase C.

Observação: `shutdown()` espera até ~27 s depois de `window.run()` (batch.rs:388-408). É proposital, porque NVENC não pode ser morto à força (CLAUDE.md), e a janela já fechou nesse ponto.

### 2.2 Concorrência, processos externos e pipeline

| Item | Status | Evidência |
|---|---|---|
| Runtime tokio com 2 workers; trabalho pesado em `spawn_blocking` | OK | app.rs:785-790; engine.rs:343-348; images.rs:287 |
| Limites de paralelismo: vídeo ≤ `concurrency`, CPU = núcleos − 2, estágios | OK | batch.rs:232, 253-254; images.rs:82-85, 129 |
| Admissão de memória e VRAM | OK | batch.rs:8-14; video.rs:728-760; images.rs:218-237; ffmpeg.rs:68-131 |
| `claim_next`/`claim_cpu` por cursor, sem reordenar a cada claim | OK | queue.rs:223-236; images.rs:119-148 |
| Memória por item na fila; cópias inteiras de `files` sob o lock | ACHADO | 2.2-5 |
| `rebuild_queue`/`queue_preview` O(n) com o lock | ACHADO | 2.1-2 e 2.1-3 (mesmo achado) |
| Job object `KILL_ON_JOB_CLOSE` com o próprio app dentro (filhos e netos nascem no job) | OK | system.rs:47-87, 103-108; engine.rs:268 |
| Explorer e relançamento pelo updater saem do job (breakaway) | OK | system.rs:112-123; update.rs:283-294 |
| Stop e fechar janela: "q" ao ffmpeg, kill após 10 s, `shutdown` espera | OK (decisão registrada) | ffmpeg.rs:212-222, 519-535; batch.rs:388-408 |
| Crash (`panic = "abort"`) com encoders NVENC rodando | ACHADO | 2.2-1 |
| Stop/Pause chegam ao processo real atrás de um shim do Scoop | ACHADO (condicional) | 2.2-7 |
| `CREATE_NO_WINDOW` em todo spawn de produção | OK | system.rs:301-315 (todos os `tokio::Command`); tools.rs:172, 402; toolver.rs:75; scan.rs:59. Os outros `Command::new` estão em `mod tests`. |
| Pipes lidos em paralelo em `tools::run` | OK | tools.rs:417-449 (uma thread por pipe) |
| Pipes nas sondas tokio (`output()`) e no VMAF (stderr numa task) | OK | ffmpeg.rs:345-411; vmaf.rs:383-389 |
| Pipes em `toolver::output_of` | ACHADO | 2.2-8 |
| Saídas limitadas (`Hash`/`BytesUpTo`; `Bytes` só para saídas pequenas) | OK | tools.rs:212-222; usos de `Bytes`: archive.rs:374, pdf.rs:81, verify.rs:80, mod.rs:556 (limitado por `TO_PNG_MAX`), mod.rs:672 |
| Parse de progresso, VMAF e ssimulacra2 tolerante | OK | progress.rs:94-167 (carry limitado a 4 KiB); mod.rs:675; vmaf.rs:415-425 |
| Ferramenta ausente | OK | ffmpeg ausente vira `SpawnError`; ferramentas de imagem são opcionais (tools.rs:65-75) |
| Sondas ffmpeg com timeout e Stop | ACHADO | 2.2-3 |
| Falha transitória de `CreateProcess` | ACHADO | 2.2-4 |
| Envenenamento de Mutex | OK | Todos os `lock()` de produção usam `unwrap_or_else(PoisonError::into_inner)` (engine.rs:338-340; tools.rs:290-340; archive.rs:168; update.rs:278) |
| Panic num worker com unwind | ACHADO | 2.2-2 |

| ID | Onde | Descrição e impacto | Sev. | Esf. | Breaking | Correção proposta |
|---|---|---|---|---|---|---|
| 2.2-1 | Cargo.toml:51; logfile.rs:58-73; system.rs:68 | Com `panic = "abort"`, um panic em qualquer thread aborta o processo. Ao fechar o handle do job, `KILL_ON_JOB_CLOSE` faz TerminateProcess em **todos** os ffmpeg NVENC, sem o "q". É exatamente o que o CLAUDE.md proíbe (reset do driver, code 43). O histórico de local ainda não salvo se perde. | alto | M | não | Resolvido por 2.4-2 (unwind + isolamento por item). Como defesa extra, o panic hook manda "q" pelo stdin dos encoders registrados num `static` acessado só com `try_lock`, espera até 10 s e só então segue. |
| 2.2-2 | images.rs:254, 287-318; ffmpeg.rs:111, 228; batch.rs:262-266 | Com unwind, um panic durante um arquivo vaza contadores mantidos à mão: `images_active`, `in_progress`, `cpu_active[stage]`, o slot de `Slots`, `running_processes` e o id em `cpu_ids`. Com isso o lote pode esperar para sempre (`CpuClaim::Wait`) ou a admissão de encoders fica travada. | médio (pré-requisito de 2.4-2) | P | não | Guards RAII (`Drop`) para slot, contadores e ids; reaproveitar `RunningProcess` (vmaf.rs:433-447) em ffmpeg.rs:111/228. |
| 2.2-3 | ffmpeg.rs:342-416; video.rs:605; scan.rs:52-62 | `gpu_decodable`, `probe_streams` (com showinfo de HDR), `same_audio_track` (decodifica as duas faixas inteiras), `video_timing` e `media_readable` usam `output()` sem timeout, sem Stop/Pause e fora de `running_processes`. Um arquivo patológico prende o worker e o Stop fica em "Stopping"; `same_audio_track` num filme longo atrasa Stop/Pause em dezenas de segundos; `media_readable` pode travar o scan. | médio | P | não | Helper `probe_output(cmd, cancel, timeout)` com `tokio::select!`. Matar essas sondas é seguro: são decodificação na CPU, não NVENC. `media_readable` passa por `tools::run` com timeout. |
| 2.2-4 | ffmpeg.rs:101-108 → video.rs:311-324 | Qualquer erro no `spawn()` do ffmpeg vira `Outcome::Fatal` e **para todos os vídeos do lote**, inclusive falta momentânea de commit (erros 8 e 1455). `tools::run` já trata esses erros como falta de memória (tools.rs:404-410). | médio | P | não | Com erro 8 ou 1455, marcar `diag.out_of_memory` (espera e nova tentativa, como já existe em `encode`). Fatal só para NotFound ou acesso negado. |
| 2.2-5 | scan.rs:284, 329; queue.rs:141-162 | Cada arquivo é um `PathBuf` em `files` e outro, clonado, em `pending_queue`/`cpu_queues`. Estimativa: ~300-350 B por arquivo, ~400 MB com 1,2 mi (não medido). Um drop durante o lote copia `files` inteiro para um `HashSet` com o lock seguro. Pico de commit numa máquina com pouca folga (sem commit, o processo aborta). | médio | M | não | Filas como índices `u32` em `files` (ou `Arc<Path>`) e um índice `HashSet` persistente em `Inner`. Medir antes e depois com a execução real de 900k. |
| 2.2-6 | — | (incorporado a 2.1-2 e 2.1-3) | — | — | — | — |
| 2.2-7 | tools.rs:26-35, 471, 486-489 | Shim do Scoop com linha `args` não é resolvido: `kill()` mata só o shim, o exe real segue no job e `out_thread.join()` bloqueia até ele terminar; o Pause suspende só o shim. Condicional: depende de existir um shim assim. | baixo | M | não | Logar um aviso quando um shim não for resolvido. Se precisar de mais, um job por execução de `tools::run`, com `TerminateJobObject`. |
| 2.2-8 | toolver.rs:79-89 | Lê stdout e depois stderr na mesma thread. Um banner grande em stderr bloqueia os dois lados até o kill de 5 s. `try_wait().ok()?` retorna sem esperar o filho. Efeito: About > Tools mostra versão desconhecida ou demora. | baixo | P | não | Reusar `tools::run` com `Capture::BytesUpTo`. |
| 2.2-9 | Cargo.toml:47-52; logfile.rs:69 | `strip = true` sem `debug`: o backtrace do `crash-*.txt` em release só tem endereços. O `mediaopti.pdb` (6,3 MB) é gerado em `target/release`, mas não é guardado por versão. | baixo | P | não | `debug = "line-tables-only"`, guardar o PDB de cada release fora do zip e gravar o endereço base do módulo no crash. |

Observação de UX, sem achado: fechar a janela durante um lote deixa o processo invisível por até ~27 s. Reabrir nesse intervalo mostra "already running" (main.rs:42-47). Vale considerar um aviso "finalizando".

### 2.3 Sistema de arquivos e integridade (prioridade máxima)

Resumo: o fluxo principal está certo. O temp fica no mesmo volume, o conteúdo é verificado, o arquivo novo recebe flush e o hash vai para uma nota durável antes de o original sair do lugar. As brechas reais estão nos caminhos laterais: fallback, recuperação depois de crash, varredura concorrente e "mesmo conteúdo". Os achados 2.3-1, 2.3-2 e 2.3-4 **foram conferidos linha a linha pelo coordenador**.

| Item | Status | Evidência |
|---|---|---|
| Temp no mesmo diretório/volume, depois rename | OK | vídeo: args.rs:515-518 (`<stem>.mediaopti.w<id>.<ext>` ao lado do original); CPU: o resultado sai do `WorkDir` e é copiado para um irmão `.mediaopti.i<id>` (images.rs:717-732) |
| Flush do novo antes de mexer no original | OK | replace.rs:85 (`sync_all`, system.rs:581-599); nota durável em replace.rs:89 (`append_lines(..., true)`) |
| Ordem: original sai do lugar, depois rename | OK, com ressalva | system.rs:640-671: Lixeira/MoveTo tiram o original antes; Delete com o mesmo nome é `rename` com substituição (atômico). Ver 2.3-1 |
| Durabilidade da entrada de diretório | OK (aceitável) | Sem `MOVEFILE_WRITE_THROUGH`, mas o journal do NTFS é ordenado; o estado "original fora + temp" é recuperado no scan (scan.rs:103-115). Ver 2.3-2 |
| Fallback quando o rename falha | ACHADO | 2.3-1 |
| Recuperação de órfãos depois de crash | ACHADO | 2.3-2 |
| Varredura durante um lote em execução | ACHADO | 2.3-3 |
| `is_orphan_temp` casa só o padrão exato | OK | regex replace.rs:48-53; teste replace.rs:163-164 |
| `clean_stale_work` só apaga nomes exatos de PIDs mortos | OK | system.rs:275-296 |
| Journal de claims só remove placeholders vazios | OK | system.rs:524-540 |
| `claim_new_name` / `unused_name` | OK | `create_new` atômico (system.rs:488-492) |
| Backup: nome livre e depois rename | ACHADO | 2.3-6 |
| Backup entre drives | OK | cópia + flush + atributos, só então remove; em falha, remove a cópia (system.rs:609-621) |
| Lixeira indisponível | OK | `FOF_WANTNUKEWARNING` pergunta ao usuário; recusa vira `Interrupted`, sem nova tentativa; só dá sucesso se o arquivo saiu de fato (system.rs:401-449). Ressalva baixa: o diálogo sem hwnd pode ficar escondido e segura o `LOCK` global |
| Padrão do descarte | OK | Lixeira (args.rs:342; teste args.rs:760) |
| Validação antes de substituir: imagens | OK | Cada etapa precisa ser menor **e** passar no `check` (mod.rs:447-468); 0 byte falha na decodificação |
| Validação: vídeo | OK, com ressalva | `streams_lost` (ffmpeg.rs:449-499), PCM→FLAC decodificado, timing de GIF. Ver 2.3-7 |
| Validação: ZIP/CBZ/CBR | OK | Releitura com CRC de toda entrada, nomes e ordem (archive.rs:192-205) |
| Validação: PDF | OK, com ressalva | `qpdf --check` + nº de páginas; render só no caminho com perda. Ver 2.3-8 |
| Algum caminho confia só no exit code | Não encontrado | — |
| Arquivo alterado durante o processamento | OK | tamanho + mtime antes de substituir (images.rs:667, video.rs:540) |
| Hard links | OK | pulados (images.rs:603, video.rs:98) |
| Somente leitura | OK | limpo antes do rename e restaurado depois (system.rs:644, 656-659, 736-740) |
| Datas e flags | OK, com ressalva | system.rs:718-752; em imagens, o erro é ignorado (2.3-9) |
| ACLs e ADS (Zone.Identifier) | ACHADO | 2.3-5 |
| "Mesmo conteúdo" descarta o original | ACHADO | 2.3-4 |
| Erro 32 ao remover temp | OK (baixo) | Remoção sem nova tentativa (replace.rs:113-120), mas a sobra é limpa no próximo scan. A substituição em si tem 6 tentativas com backoff (system.rs:636-684) |
| Caminho longo na Lixeira | OK (sem perda) | `SHFileOperationW` falha acima de MAX_PATH → erro, nada muda (system.rs:436); só o trabalho se perde. Ver 2.5-3 |

| ID | Onde | Descrição, cenário e impacto | Sev. | Esf. | Breaking | Correção proposta |
|---|---|---|---|---|---|---|
| 2.3-1 | system.rs:660-664 | Quando `rename(tmp, alvo)` dá `PermissionDenied`, o código faz `fs::copy(tmp, alvo)` **por cima do alvo**, sem flush, e em seguida `remove_file(tmp)`. Cenário típico: modo Delete com o mesmo nome e o original aberto por um visualizador sem `FILE_SHARE_DELETE`. O original é truncado e reescrito no lugar. Uma queda ou abort no meio deixa o original parcial. No próximo scan, `settle_orphan_temp` vê "original existe" e apaga o temp: **nenhuma cópia boa**. Mesmo sem queda no meio, o temp é apagado antes de os dados do alvo irem para o disco. | **crítico** (Delete) / alto (Lixeira e backup) | P | não | Remover o fallback por cópia e só repetir o rename (o backoff já existe). Se for preciso copiar: para um temp novo no mesmo diretório, com flush e rename. Nunca apagar o temp antes de o alvo estar durável. Teste: alvo aberto sem share-delete. |
| 2.3-2 | replace.rs:17-43; scan.rs:52-62, 107 | Recuperação depois de crash na janela "original já na Lixeira/backup, temp ainda sem nome". (a) `complete` = `media_readable` (ffmpeg): para temps de PDF, DOCX, EPUB e CBZ o ffmpeg falha e o temp é **apagado**, mesmo com o original já fora do lugar. (b) `original_exists` aceita **qualquer arquivo com o mesmo stem**: `IMG_1234.MOV` + `IMG_1234.JPG` (Live Photo), `filme.mkv` + `filme.srt`, `album.wav` + `album.cue`. Com um irmão assim, o temp é apagado. Efeito: o arquivo some da biblioteca e só existe na Lixeira ou no backup, e o log diz apenas "Removed N leftover temporary file(s)". | alto | M | não | Um journal de substituições pendentes (nome exato do original + temp + destino), gravado junto com a nota antes do set-aside. A recuperação consulta o journal em vez do stem. Sem journal ou na dúvida, **nunca apagar**: renomear para `unused_name` e logar. Para tipos que não são mídia, verificar com o próprio verificador (qpdf, zip+CRC). Testes com `MEDIAOPTI_DIE_AT=set-aside` para PDF e para irmão de mesmo stem. |
| 2.3-3 | scan.rs:186-253, 103-107 | Uma pasta arrastada durante o lote é escaneada, e `settle_orphan_temp` roda sobre temps **vivos** dos workers (não há filtro por `in_progress`). Imagem: o irmão `x.mediaopti.i3.png` é apagado, o worker manda o original para a Lixeira e o rename falha 6 vezes; o log diz "new version is kept as ..." para um arquivo que não existe. Vídeo: o temp some durante o VMAF e o vídeo entra no histórico como falho. | médio (janela estreita, mas o fluxo é suportado) | P | não | Durante um lote, não chamar `settle_orphan_temp` (ou pular temps de arquivos em `in_progress`). Em `replace_file_with_retry`, conferir `tmp.exists()` imediatamente antes de tirar o original. |
| 2.3-4 | video.rs:620-660; images.rs:565-578, 774-777 | "Mesmo conteúdo" no nome de destino: `same_video` compara só duração (±2%) e VMAF. Se o `.mp4` existente for menor, ele fica e **o original é descartado**, mesmo que o existente não tenha a 2ª faixa de áudio ou as legendas. Isso viola "a video keeps everything besides its picture". O mesmo vale para imagem (EXIF) e áudio (tags) com conversão ou correção de extensão. | alto (Delete) / médio (Lixeira, padrão) | P | não | Só descartar o original quando `streams_lost(original, existente)` for `None` (vídeo) ou os metadados forem equivalentes; caso contrário, manter os dois (cair no caminho "name (2)"). |
| 2.3-5 | system.rs:690-752; images.rs:728 | ADS e ACLs não são copiados. Um `.docm/.xlsm/.pptm` ou PDF baixado perde o `Zone.Identifier` (Mark-of-the-Web): somem o bloqueio de macros do Office, o Modo Protegido e o SmartScreen. ACLs explícitas viram as herdadas da pasta. | médio | M | não | Copiar `:Zone.Identifier` (e os demais ADS via `FindFirstStreamW`) para o temp antes do flush; copiar o DACL com `GetFileSecurity`/`SetFileSecurity`. Alternativa barata: pular arquivos com ADS. |
| 2.3-6 | system.rs:605-610; queue.rs:93-98 | Backup: `unused_name` e depois `fs::rename`, que **substitui** o destino no Windows. Dois workers com o mesmo nome de backup (arquivos arrastados de pastas diferentes, que caem achatados em `<backup>\IMG_0001.JPG`) podem sobrescrever o backup um do outro. Perda de um original (probabilidade baixa). | médio | P | não | `MoveFileExW` sem `REPLACE_EXISTING`, em loop sobre `unused_name` quando der `AlreadyExists`; espelhar o caminho completo para arquivos fora de `dir`. |
| 2.3-7 | ffmpeg.rs:493-494 | A duração só falha se a saída for mais curta em mais de max(1%, 1 s). Num filme de 2 h, até 72 s finais perdidos (dano perto do fim que o ffmpeg pula com exit 0) passam. | baixo/médio | P | não | Medir pelos pacotes (`video_timing`, que já existe para GIF) com tolerância de ~1 s; manter 1% só para contêineres de cabeçalho duvidoso (TS/AVI). |
| 2.3-8 | pdf.rs:73-77, 99, 143 | Originais com `qpdf --check` ≠ 0 são reescritos; a reparação do qpdf gera saída "limpa" validada só pela estrutura e pelo nº de páginas (sem render no caminho sem perda). | baixo | P | não | Se `check_code != 0`, exigir `pages_look_same` ou pular. |
| 2.3-9 | images.rs:737, 767 | `let _ = restore_attributes(...)`: falha ao restaurar datas em imagens não é logada (no vídeo é, video.rs:591-593). | baixo | P | não | Logar como no vídeo. |

### 2.4 Qualidade do código Rust

**`unwrap`/`expect`.** O clippy pedantic acusa 1.359 ocorrências. Dessas, 1.188 estão fora de `engine_tests.rs`, quase todas em blocos `#[cfg(test)]` dos módulos. A classificação manual do código de produção encontrou **29**:

- **Classe (a), invariantes legítimas:**
  - `try_into().unwrap()` de fatias de tamanho já checado (audio.rs: 11 ocorrências);
  - `Regex::new` constante em `LazyLock` (progress.rs:8-33 com 10; replace.rs:50; tools.rs:513);
  - `take().expect("piped")` com `Stdio::piped()` (tools.rs:413-414; ffmpeg.rs:134);
  - `(2..).find` (system.rs:575);
  - criação do runtime (app.rs:790);
  - `unreachable!` de despacho (archive.rs:32, audio.rs:47, mod.rs:511, video.rs:327).
- **Classe (b)**, possíveis falhas em produção: tabela abaixo.

O problema maior **não é unwrap**: é indexação de slice em parser somada a `panic = "abort"`.

**Estilo de erro.** O código já usa enums próprios (`RunError` tools.rs:374, `AudioError` audio.rs:56, `archive::Error` archive.rs:40), `io::Error`/`String` nas bordas, e resultado tipado por item (`ImageResult`, `Outcome`). Isso já reporta a falha por item na UI. **Não proponho `thiserror`/`anyhow`**: adicionariam dependências sem resolver nada que o padrão atual não resolva.

| ID | Onde | Descrição e impacto | Sev. | Esf. | Breaking | Correção proposta |
|---|---|---|---|---|---|---|
| 2.4-1 | image/jpeg.rs:45-49 | Um segmento APPn/COM com campo de tamanho 0 ou 1 dá `end = i+2+len < i+4`, e `data[i+4..end]` entra em panic. **Conferido pelo coordenador.** `segments()` roda em **todo JPEG** verificado (verify.rs:47, 134; mod.rs:852) e nos JPEGs dentro de PDFs. Com `abort`, um único JPEG malformado entre os 183k derruba o app inteiro, encoders NVENC incluídos (2.2-1), e o arquivo será tentado de novo, derrubando o app a cada execução. | **alto** | P | não | `if len < 2 { return None }` (ou `data.get(i+4..end)?`); teste com APP1 de tamanho 0 e 1. |
| 2.4-2 | Cargo.toml:51 | `panic = "abort"`. O código não captura panics (não há `catch_unwind` nem `JoinError::is_panic`); batch.rs:166-169 loga o `JoinError`, mas em release o processo já morreu. Uma falha num arquivo derruba o lote e mata os NVENC à força. Trocar só para `unwind` **não basta**: um panic mataria o worker e vazaria contadores (2.2-2). | alto | M | não | `panic = "unwind"` mais: (CPU) `catch_unwind(AssertUnwindSafe(..))` em volta de `process_image` dentro do `blocking` (images.rs:288-314), mapeado para `ImageResult::Failed("internal error: ..")` e **não** gravado no histórico; (vídeo) cada `process_one` como `tokio::spawn` próprio, com `JoinError::is_panic()` virando falha do item; guards `Drop` de 2.2-2; wrapper que loga panics nas `std::thread::spawn` do app.rs. Medir o tamanho do binário antes e depois (estimativa: +5-10%). |
| 2.4-3 | engine/replace.rs:19-21 | `lower = name.to_lowercase()` e depois `&name[..cut]` com o índice achado em `lower`. `to_lowercase` muda o tamanho em bytes de alguns caracteres (ex.: `İ`), e o corte pode cair fora de fronteira UTF-8: **panic durante o scan**, que se repetiria a cada vez que a pasta fosse carregada. | médio | P | não | `to_ascii_lowercase()` (preserva os offsets); teste com `İ.mediaopti.w1.mp4`. |
| 2.4-4 | pdf.rs:189-191 | `sequential()` procura marcadores SOF progressivos (`0xC2/C6/CA/CE`) em `jpeg::segments`, que só guarda APPn/COM (jpeg.rs:48). A guarda é **sempre verdadeira** (conferido pelo coordenador). Hoje a saída é sequencial porque cjpegli roda com `-p 0` e mozjpeg com `-baseline` (pdf.rs:213, 216), mas a regra do CLAUDE.md "Re-encoded JPEGs must be sequential" não é verificada. | médio | P | não | Percorrer os marcadores até SOS e recusar SOF2/6/10/14; teste com JPEG progressivo. |
| 2.4-5 | engine.rs:347 | `.await.expect("blocking file task panicked")`: com unwind, repropaga o panic de um arquivo para o worker; também pega `JoinError::Cancelled` no desligamento. | baixo (médio após 2.4-2) | P | não | Devolver `Result<T, JoinError>` e quem chama mapeia para falha do item (parte de 2.4-2). |
| 2.4-6 | app.rs:907-909 | `row_data(index.max(0) as usize)` e depois `set_row_data(index as usize, ..)`: índices diferentes se `index < 0`. | baixo | P | não | `usize::try_from(index).ok()?` uma vez. |
| 2.4-7 | system.rs:570-573 | `unused_name` usa `to_string_lossy()` do stem: um nome não UTF-16 válido vira U+FFFD em "name (2)". | baixo | P | não | Montar com `OsString::push`. |
| 2.4-8 | queue.rs:141-160; history.rs:162-167 | `rebuild_queue`: por arquivo, um `PathBuf::clone` e até 4 SHA-256 (um por modo), tudo sob o lock (ver 2.1-3). | médio | M | não | Calcular o hash do caminho uma vez e clonar o estado `Sha256` parcial para cada modo; montar fora do lock. |
| 2.4-9 | images.rs:241-286, 256, 296; ffmpeg.rs:175-182; progress.rs:101-110 | Caminhos quentes: ~6 aquisições do lock `Inner` por arquivo de CPU; `job_opts()` faz `is_dir()` (syscall) sob o lock **por arquivo**; `Ctx` clonado por arquivo; o parse de progresso roda 7 regex **com o lock seguro**. Impacto real pequeno (o custo dominante são as ferramentas externas), mas soma contenção com 14 workers e 900k imagens pequenas. | baixo | P/M | não | `JobOpts` uma vez por lote; parse numa struct local e só copiar sob o lock; juntar seções críticas. |
| 2.4-10 | system.rs (≈20 blocos, 130-765); taskbar.rs:69-72; update.rs:295 | Stubs `cfg(not(windows))` que nunca foram compilados nem testados. | baixo | P | não | Remover, ou manter só se `cargo check --target x86_64-unknown-linux-gnu` entrar na CI. |
| 2.4-11 | Cargo.toml (windows-sys 0.59) | O lock tem 4 versões de `windows-sys` (0.48, 0.52, 0.59, 0.61) e 2 de `windows-targets`; `windows` (COM) é usado só em taskbar.rs. Outras duplicadas são transitivas de slint/winit/rfd. Nenhuma dependência sem uso: as 17 diretas foram conferidas. | baixo | P | não | Subir `windows-sys` para 0.61 (conferir API em system.rs). Rodar `cargo audit`, `cargo outdated -R` e `cargo machete` depois de instalados. |
| 2.4-12 | Cargo.toml | Sem `rust-version`. O código usa `LazyLock` (1.80), `Option::is_none_or` (1.82) e `is_multiple_of` (app.rs:1436, mais recente). | baixo | P | não | Declarar `rust-version` (confirmar com `cargo msrv`); `edition = "2024"` é opcional, e é breaking só para quem compila. |
| 2.4-13 | Funções longas (clippy `too_many_lines`) | `process_one` com 487 linhas (video.rs:49), `run` com 396 (app.rs:784), `render` 184 (app.rs:509), `run_attempt` 178 (ffmpeg.rs:35), `quality_retry` 171, `load_items` 170 e outras 9 acima de 100. Mais 4 `#[allow(clippy::too_many_arguments)]`. | baixo | M | não | Fase C: extrair as etapas de `process_one` (probe/admissão, encode, validação, substituição) e agrupar parâmetros em structs. Só com testes cobrindo antes. |

**Perfil de release** (avaliar com medição, não por padrão):

| Ajuste | Avaliação | O que medir |
|---|---|---|
| `lto = true` (fat) + `codegen-units = 1` | Build release de 167 s. O ganho em runtime deve ser pequeno, porque o trabalho pesado é das ferramentas externas. | `lto = "thin"` / `codegen-units = 16`: tempo de build, tamanho do binário, fluidez da UI (renderer de software) e `bench_folder` |
| `opt-level = 3` | Adequado: o renderer de software e os hashes (SHA-256/BLAKE2) se beneficiam. | `cargo bloat --release --crates` antes de pensar em `"s"` |
| `panic = "abort"` | Trocar para `"unwind"` junto com 2.4-2. | Tamanho do binário |
| `strip = true` | Inutiliza o backtrace do crash (2.2-9). | `debug = "line-tables-only"` + PDB guardado por versão |
| `overflow-checks` (ausente) | Com unwind e isolamento por item, um overflow viraria falha de um item em vez de um contador errado em silêncio. | Custo desprezível; avaliar junto com 2.4-2 |

### 2.5 Funcionalidades, UX e consistência

Casos de borda pela leitura do código. O comportamento em execução está na Fase 0 (corpus com esses mesmos casos).

| Item | Status | Evidência |
|---|---|---|
| Arquivo de 0 byte | ACHADO | 2.5-4 |
| Formato não suportado | OK | Filtrado por extensão (scan.rs:100; mod.rs:87-105); não entra na contagem, por decisão |
| Pasta ou arquivo sem permissão na varredura | OK | Contado como `unreadable` (scan.rs:93-95) e logado: "could not be read (permissions or broken links)" (scan.rs:269-270) |
| Somente leitura | OK | system.rs:377-395, 580-595 |
| Vídeo corrompido | OK | Histórico + `failed` + "Corrupt/unreadable ... will not retry" (video.rs:229-243) |
| Falha de ferramenta em imagem ou documento | ACHADO | 2.5-1 |
| Arquivo enorme: disco (CPU) | OK | `WorkDir` reserva 4x (PDF 16x), `RESERVED` compartilhado entre jobs, `DISK_RESERVE` de 4 GB, recurso em `<drive>\.mediaopti-temp` (images.rs:478-503) |
| Arquivo enorme: disco (vídeo) | OK, com ressalva | Pede 2x o tamanho + reserva (video.rs:78-97), mas fora do `RESERVED` (2.5-8) |
| Arquivo enorme: memória | OK, com ressalva | 2 GB de commit livre fixos por arquivo de CPU (images.rs:19, 218-238), seja qual for o tamanho; depois há `short_of_memory` com 2 novas tentativas (2.5-9) |
| Timeouts | OK | 900 s por ferramenta, sem contar a pausa (images.rs:166; tools.rs:485); watchdog de travamento no vídeo (ffmpeg.rs:194-208) |
| Milhares de arquivos | OK, com ressalva | GIFs analisados em threads (scan.rs:127-146), log limitado; custo O(n) com lock em 2.1-2, 2.1-3 e 2.2-5 |
| Caminho > 260 caracteres | ACHADO | 2.5-3 |
| Unicode | OK, com ressalva | Imagens são processadas numa cópia com nome ASCII (images.rs:627-636); o histórico usa hash de `to_string_lossy` (history.rs:81-91), e nomes com surrogates isolados podem colidir (raríssimo) |
| Contadores consistentes | OK | `completed` = compressed + skipped_larger + failed (guarda `Completion`, video.rs:7-18; images.rs:338) |
| Nomes no resumo | ACHADO | 2.5-5 |
| Idioma da UI e das mensagens | OK | Inglês em toda a UI, log e README |
| Settings: `#[serde(default)]`, campos desconhecidos ignorados, migração de valores antigos | OK | app.rs:83-127, 169-183 |
| Settings: gravação atômica e versão | ACHADO | 2.1-5 e 2.5-6 |
| Formatos de histórico | OK, com ressalva | Compatíveis com batchviproc e imageopti, carga tolerante, regravação atômica; linhas sem versão; um modo desconhecido é tratado como "imagem lossless" (history.rs:36-45) |
| Updater | OK | asset `-win64.zip`, digest SHA-256, troca tudo ou nada, entradas do zip limitadas à pasta do app (update.rs:84-97, 155-260) |
| README fiel ao comportamento | ACHADO | 2.5-7 |

| ID | Onde | Descrição e impacto | Sev. | Esf. | Breaking | Correção proposta |
|---|---|---|---|---|---|---|
| 2.5-1 | engine/images.rs:652; tools.rs:404-409, 485-493 | Todo `RunError::Failed` vira `ImageResult::Broken`, que é gravado no histórico e "não será tentado de novo". Isso inclui **falha ao iniciar a ferramenta** (antivírus em quarentena, DLL faltando, acesso negado) e **timeout** numa máquina carregada. Na execução de 900k imagens, uma ferramenta ausente marcaria milhares de arquivos bons como quebrados, para sempre. | **alto** (véspera da execução grande) | P | não | Separar `RunError::Spawn` e `RunError::Timeout`, mapeados para `ImageResult::Failed` (não gravado), como já acontece com `OutOfMemory`. Abortar o estágio quando a mesma ferramenta falhar ao iniciar N vezes seguidas. |
| 2.5-2 | image/tools.rs:60; engine/ffmpeg.rs:23 | `env!("CARGO_MANIFEST_DIR")/bin` vai embutido no executável de release: expõe `C:\Users\guilw\...` no binário público e, na máquina do autor, faz o release usar o `bin/` de desenvolvimento. Isso esconde pacote com ferramenta faltando e o download automático de pingo/beeim (provável motivo de `%APPDATA%\mediaopti\tools` nunca ter existido nesta máquina). | médio | P | não | `#[cfg(any(test, debug_assertions))]` nas duas linhas; atualizar o doc comment de `discover` (tools.rs:37-40). |
| 2.5-3 | build.rs (sem manifesto `longPathAware`); images.rs:617; vídeo e Lixeira | O Rust std aceita caminho longo, mas os caminhos vão crus para o ffmpeg, para o processamento de áudio no lugar (FLAC/MP3/OGG não usam a cópia ASCII) e para `SHFileOperationW` (MAX_PATH). O resultado esperado é falha, ou "quebrado para sempre" por causa de 2.5-1. **Fase 0:** um JPEG em caminho de 389 caracteres foi processado sem erro (o processamento usa uma cópia com nome ASCII). Faltam testar vídeo, áudio e a substituição pela Lixeira em caminho longo. | médio | M | não | Na varredura, pular caminhos > 259 com uma mensagem própria ("path too long") e não gravá-los no histórico. Ou passar `\\?\` ao ffmpeg, copiar o áudio para o `WorkDir` e mover para a Lixeira com `IFileOperation`. |
| 2.5-4 | scan.rs:117 | Arquivos de 0 byte são descartados sem log nem contador. | baixo | P | não | Contar e logar "N empty files ignored". |
| 2.5-5 | batch.rs:182-185; app.rs:1453-1456; engine.rs:84 | O mesmo número aparece como "kept as they were" no log e "skipped" na UI. O balde `skipped_larger` mistura "já estava ótimo" com hard link, só áudio, sem espaço, alterado durante o job e nome ocupado (estes serão tentados de novo). | baixo | P/M | não | Um termo só; de preferência separar `kept` (ótimo, gravado) de `skipped` (será tentado de novo). |
| 2.5-6 | app.rs:171, 186-190, 218 | Além de 2.1-5: um **campo com tipo inesperado** também zera todas as opções; não há campo de versão; `max_parts` é gravado inteiro, então uma mudança futura em `Max::chosen()` nunca chega a quem já tem o arquivo. | baixo | P | não (adicionar `version` com default é compatível) | `write_atomically`; `"version": 1`; log quando a leitura falhar; `max_parts: Option<Vec<_>>` gravado só se diferente do padrão. |
| 2.5-7 | README.md | (1) Está em **inglês**; a preferência global pede README em português formal, mas o CLAUDE.local diz só "short and professional": **decisão do usuário**. (2) Diz "Optional lossless conversions", mas `conv_wav`, `conv_img` e `conv_cbr` vêm **ligadas** (app.rs:145-147). (3) Omite cjpegli/mozjpeg, ssimulacra2, OptiVorbis, qpdf, mutool e 7-Zip. (4) Não cita as extensões jpe, jfif, oga, aif/aifc, docm/xlsm/pptm (mod.rs:90-99), nem a correção de extensão e a pasta temporária. CLAUDE.md: "engine.rs ~380 lines" (são 419). | baixo | P | não | Revisar o texto na Fase 4. |
| 2.5-8 | video.rs:81-82 | A reserva de disco do vídeo não entra no `RESERVED`: 4 jobs de vídeo grandes no mesmo disco veem o mesmo espaço livre. | baixo | P | não | Registrar 2x o tamanho em `RESERVED` (images.rs:460). |
| 2.5-9 | images.rs:19 | Admissão de memória fixa em 2 GB por arquivo de CPU. Uma imagem de 164 MP decodificada em 16 bits passa disso. Há a proteção posterior (`short_of_memory`), mas ela gasta a tentativa. | baixo | M | não | Estimar pela área (largura × altura × bytes × ~3) acima de ~50 MP. Depende da atribuição de 0-1. |
| 2.5-10 | engine/images.rs (resultado "Kept") | **Fase 0:** o JPEG truncado sai como "Kept ... (already optimal)" e é gravado no histórico como ótimo. É seguro (nada é substituído), mas a mensagem engana: o arquivo está danificado e o usuário não fica sabendo. | baixo | P | não | Quando a decodificação do original acusar dados truncados ou erro, registrar "damaged: left as it is" (contador próprio) em vez de "already optimal". |

### 2.6 Build e distribuição

| Item | Status | Evidência |
|---|---|---|
| Pacote e nome do asset | OK | `scripts/package.ps1` → `dist/mediaopti-<ver>-win64.zip`; o updater procura `-win64.zip` (update.rs:84); v3.0.12 publicada com esse nome |
| `versions.json` e licenças por ferramenta | OK, com ressalva | 2.6-2 |
| `release.ps1` (árvore limpa, caracteres de controle, check-tools, clippy, testes, pacote, digest) | OK | Roda o clippy sem `--locked`, o que é coerente porque o bump muda o `Cargo.lock` |
| Download de pingo/beeim | OK | HTTPS + checagem de PE "MZ" ≥ 64 KB (tools.rs:117-147); sem hash (o upstream não publica) |
| Busca de ferramentas | OK, com ressalva | tools.json → `<exe>\tools\*` → `%APPDATA%\mediaopti\tools` → `bin` de dev → PATH, com shims do Scoop resolvidos (tools.rs:24-75). Ver 2.5-2 |
| Linux | **Não suportado** | Só Windows: NVENC via ffmpeg.exe, Lixeira, job objects, `%APPDATA%`, `.exe`, Scoop; stubs `cfg(not(windows))` parciais e nunca compilados. **Os itens de Linux pedidos não se aplicam** (grupos de processo, empacotamento). Suporte real seria um projeto à parte (XDG, crate `trash`, cgroups/prlimit, `which`, AppImage): esforço G, "Fica para depois". |
| CI | ACHADO | 2.6-1 |

| ID | Onde | Descrição e impacto | Sev. | Esf. | Breaking | Correção proposta |
|---|---|---|---|---|---|---|
| 2.6-1 | (não existe `.github/`) | Sem CI. Além disso, os testes **passam em silêncio** quando falta `bin/ffmpeg.exe` ou uma ferramenta (engine_tests.rs:95 e outros), então um verde não garante cobertura. | médio | P | não | Workflow mínimo em `windows-latest` (repo público, runner gratuito): `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked` e a checagem de caracteres de controle. Num ambiente de desenvolvimento, `MEDIAOPTI_REQUIRE_TOOLS=1` deveria transformar o "skipping" em falha. |
| 2.6-2 | scripts/package.ps1; fetch-tools.ps1:56 | (1) `ffmpeg-LICENSE.txt` copiado com `-ErrorAction SilentlyContinue`: o zip pode sair sem a licença de um binário GPLv3. (2) A versão do OptiVorbis está fixada em dois scripts. (3) Para GPL/AGPL (FFmpeg, gifsicle, flac, mupdf), o THIRD-PARTY.txt só aponta a URL do upstream; a GPL pede o fonte correspondente ou uma oferta escrita. | baixo (jurídico) | P | não | `throw` se a licença faltar; uma fonte única para a versão; acrescentar ao THIRD-PARTY links para as tags/fontes exatas de cada versão distribuída. |

## Plano de Major Release

Ordem A → B → C. Para cada bug, primeiro um teste que o reproduz, depois a correção, em commits pequenos (Conventional Commits) na branch `release/v4.0.0`. Build, testes e clippy limpos a cada grupo de mudanças.

### Fase A: correções críticas (integridade, concorrência, processos, panics)

| # | Achado | Mudança | Esf. | Teste que reproduz antes |
|---|---|---|---|---|
| A0 | 0-1, 2.5-9 | Medir isolado o pico de 23 GB (JPEG de 164 MP com `bench_folder`; VMAF simultâneos) e ajustar a admissão do responsável | M | Execução isolada com monitor de memória, antes e depois |
| A1 | 2.3-1 | Remover o fallback por cópia em `replace_file_with_retry`; só novas tentativas de rename. Nunca apagar o temp antes de o alvo estar durável | P | Alvo aberto sem `FILE_SHARE_DELETE` em modo Delete: o original continua íntegro |
| A2 | 2.3-2 | Journal de substituições pendentes; recuperação pelo nome exato; na dúvida, renomear em vez de apagar; verificador por tipo para não-mídia | M | `MEDIAOPTI_DIE_AT=set-aside` com PDF e com irmão de mesmo stem (`x.mov` + `x.jpg`): nada some da biblioteca |
| A3 | 2.3-3 | Não acomodar órfãos durante um lote; checar `tmp.exists()` antes de tirar o original | P | Drop da mesma pasta durante um lote: nenhum temp vivo é apagado |
| A4 | 2.3-4 | "Mesmo conteúdo" só descarta o original se nada se perde (`streams_lost`, metadados); senão, mantém os dois | P | `.avi` com 2 faixas + `.mp4` existente menor com 1 faixa: o original fica |
| A5 | 2.4-1 | `jpeg::parse_segments` sem panic com tamanho < 2 | P | JPEG com APP1 de tamanho 0 e 1 |
| A6 | 2.4-3 | `to_ascii_lowercase` em `settle_orphan_temp` | P | `İ.mediaopti.w1.mp4` |
| A7 | 2.2-2, 2.4-2, 2.4-5 | `panic = "unwind"` + `catch_unwind` por arquivo de CPU, uma task por vídeo, guards `Drop` para slot/contadores/ids; panic vira falha do item, sem histórico | M | Panic injetado (hook só de teste) num arquivo: o lote termina, contadores zerados, sem travar |
| A8 | 2.5-1 | Falha ao iniciar a ferramenta e timeout não entram no histórico; abortar o estágio após N falhas de spawn seguidas | P | Ferramenta apontada para um exe inexistente: arquivos não marcados como quebrados |
| A9 | 2.2-4 | Erro 8/1455 no spawn do ffmpeg → espera e nova tentativa, não Fatal | P | Teste unitário da classificação do erro |
| A10 | 2.2-3 | Sondas ffmpeg com timeout e Stop | P | Sonda sobre um pipe que nunca termina: Stop encerra em < 2 s |
| A11 | 2.3-6 | Backup com move sem substituir, em loop sobre `unused_name` | P | Dois originais com o mesmo nome de backup: os dois backups existem |
| A12 | 2.3-7, 2.3-8 | Duração pelos pacotes (tolerância ~1 s, 1% só em TS/AVI); PDF danificado exige render ou é pulado | P | Vídeo com fim cortado; PDF com xref quebrada |
| A13 | 2.4-4 | Verificação real de JPEG sequencial no PDF | P | JPEG progressivo é recusado |
| A14 | 2.5-3 | Caminhos > 259: pulados com mensagem própria, não gravados no histórico (ou `\\?\` em todos os caminhos externos, conforme o resultado da Fase 0) | M | Arquivo no fim de um caminho de 389 caracteres |
| A15 | 2.3-5 | Copiar `:Zone.Identifier` (e os demais ADS) do original para o novo antes do flush | M | Arquivo com Zone.Identifier mantém o fluxo |
| A16 | 2.5-2 | `CARGO_MANIFEST_DIR/bin` só em testes e debug | P | Teste de `discover` em build não-debug (ou checagem de string no binário pela release.ps1) |
| A17 | 2.3-9 | Logar falha ao restaurar datas em imagens | P | — |

### Fase B: desempenho e fluidez da UI

| # | Achado | Mudança | Esf. | Medição |
|---|---|---|---|---|
| B1 | 2.1-1 | `clear_history` em `spawn_blocking` | P | Tempo do callback com 1 mi de arquivos sintéticos carregados |
| B2 | 2.1-2 | Tamanho junto da entrada da fila; prévia O(limite) | P | Idem |
| B3 | 2.1-3, 2.4-8 | `rebuild_queue` e merge de drop fora do lock (montar e trocar); hash do caminho uma vez por arquivo; `flush_history` grava fora do lock; Stop/Pause sinalizam pelo token antes do lock | M | Tempo de lock máximo (instrumentado em teste) com 1 mi de entradas |
| B4 | 2.2-5 | Filas como índices em `files`; índice `HashSet` persistente | M | Commit do processo com 1 mi de arquivos, antes e depois |
| B5 | 2.4-9 | `JobOpts` uma vez por lote; parse de progresso fora do lock; menos aquisições por arquivo | P | `bench_folder` com 1.000 PNGs pequenos |
| B6 | 2.1-4, 2.1-6 | `queue_counts` com `sync_model`; corte do log com `set_vec` | P | — |
| B7 | 2.5-8 | Reserva de disco do vídeo no `RESERVED` (a admissão de memória foi para A0) | P | — |
| B8 | 2.2-9, perfil | `debug = "line-tables-only"` + PDB por versão; medir `lto = "thin"`/`codegen-units` e adotar só com ganho | P | Tempo de build, tamanho do binário, `bench_folder` |

### Fase C: arquitetura, UX e polimento

| # | Achado | Mudança | Esf. |
|---|---|---|---|
| C1 | 2.1-5, 2.5-6 | Settings com gravação atômica, `version`, log de leitura falha, `max_parts` só quando diferente do padrão | P |
| C2 | 2.5-4, 2.5-5, 2.5-10 | Contar e logar arquivos vazios e danificados; termos únicos para kept/skipped/failed no log e na UI | P/M |
| C3 | 2.1-7, 2.4-13 | Páginas `.slint` em arquivos próprios; `app.rs` em prefs/controller/callbacks; `process_one` dividido em etapas | M |
| C4 | 2.2-7, 2.2-8 | Aviso para shim não resolvido; `toolver` via `tools::run` | P |
| C5 | 2.4-6, 2.4-7, 2.4-10, 2.4-11, 2.4-12 | Índice da UI, `OsString` em `unused_name`, remover stubs não-Windows, `windows-sys` 0.61, `rust-version` | P |
| C6 | 2.6-1, 2.6-2 | CI mínima (clippy, testes, caracteres de controle) e `MEDIAOPTI_REQUIRE_TOOLS`; licenças obrigatórias no pacote e links para os fontes GPL | P |
| C7 | 2.2-1 (observação) | Aviso "finalizando" ao fechar a janela durante um lote | P |
| C8 | 2.5-7 | README e CLAUDE.md fiéis ao comportamento (Fase 4) | P |

### Fica para depois

- **Suporte a Linux** (esforço G; projeto à parte).
- **Panic hook que manda "q" aos encoders** (2.2-1, defesa extra): com A7, um panic deixa de abortar o processo; só valeria para panics fora dos workers.
- **Job object por execução de ferramenta** (2.2-7 completo): só se aparecer um shim não resolvido.
- **Cópia de ACLs explícitas** (parte de 2.3-5): a biblioteca usa as permissões herdadas da pasta.
- **`thiserror`/`anyhow`**: não adotados; o padrão atual (enums próprios + resultado por item) já cumpre o papel.
- **Refazer arquivos marcados como "quebrados" por falha de ferramenta** antes de A8: as entradas antigas não podem ser distinguidas. Pode virar uma opção "tentar de novo os que falharam".
- **`edition = "2024"`**: sem ganho concreto agora.

### Breaking changes propostas

A auditoria **não encontrou nenhuma mudança que precise quebrar compatibilidade**. Settings, histórico e asset do updater continuam compatíveis: o campo `version` dos settings tem default, e os formatos de histórico não mudam.

Mudanças de comportamento padrão que o usuário vai notar:

1. **Arquivos com caminho > 259 caracteres** passam a ser pulados com aviso (A14), se for essa a correção escolhida. Justificativa: hoje falham ou ficam marcados como quebrados para sempre.
2. **"Mesmo conteúdo" mantém os dois arquivos** quando o existente perde faixas ou metadados (A4). Justificativa: a regra "o vídeo mantém tudo além da imagem".
3. **Falha ao iniciar uma ferramenta e timeout deixam de ser permanentes** (A8). Justificativa: são falhas do ambiente, não do arquivo.
4. **Contadores renomeados ou separados** (C2). Justificativa: o mesmo número aparece com nomes diferentes no log e na UI.

**Decisões do usuário (2026-11-17):** plano aprovado na íntegra; versão **3.1.0** (minor:
nada quebra compatibilidade), branch `release/v3.1.0`; README continua em **inglês** (uma
documentação detalhada em pt-BR e inglês fica para depois); caminhos longos com **suporte
completo** (A14 com `\\?\` em todos os caminhos externos, não pular).

Perguntas que estavam pendentes:
- (a) Major 4.0.0 mesmo sem quebra de formato, pelo volume de mudanças de comportamento, ou 3.1.0?
- (b) README em português formal (preferência global) ou em inglês (como está, e como a UI)?
- (c) Caminho longo: pular com aviso, ou suportar com `\\?\` (esforço M)?
