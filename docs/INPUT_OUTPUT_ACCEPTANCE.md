# Приёмка рефакторинга ввода и вывода

Статус: R0–R6 приняты 2026-09-13 на текущем рабочем дереве.

Область: R0–R6 и требования 3–7 из `drafts/PLAN_REFACTORING_INPUT_OUTPUT.md`.
Проверяется текущая реализация на JSON transport; SSE в эту приёмку не входит.
Среда проверки: macOS arm64, Rust 1.96.0, debug test profile, реальные PTY и
локальные HTTP/MCP peers. Ниже приведены конкретные доказательства; количество
прошедших тестов само по себе не заменяет проверку требований.

## Требования архитектуры

| Раздел плана | Реализация и проверяемое свойство | Доказательства |
| --- | --- | --- |
| 3.1 — execution state и ввод | `ExecutionState` в [state.rs](../src/application/state.rs) выводится из operation/cancellation, shell lease и exit. Он отделён от `Interaction` редактора/picker. Busy Enter/Ctrl+D не отправляет новый запрос; paste сохраняется как draft. | [ApplicationPty](../tests/application_pty.rs): `managed_request_keeps_pasted_draft_and_requires_explicit_submit_after_completion`, `managed_cancel_keeps_next_draft_and_clear_does_not_cancel_request`, multiline history. [managed UI](../src/application/managed_ui_tests.rs): extended keyboard, model picker, reset acknowledgement. API-key editor маскирует текст и не записывает ключ как history entry. |
| 3.2 — backend worker | [AgentWorker](../src/agent/worker.rs) единолично владеет Agent, выполняет одну операцию и отдаёт status snapshot. UI не вызывает сетевой код через editor handler. Configure подтверждает применение, а не enqueue. | Удерживаемый JSON body в ApplicationPty; удерживаемые Configure/ModelCatalog в managed UI; `/reset` + `/status` одним пакетом; compact success/error/cancel в [lifecycle tests](../src/application/managed_ui_tests/lifecycle_tests.rs). |
| 3.3 — события и backpressure | [EventSink](../src/common/events.rs): operation/block ids, sequence, один ingress для producers, 64 события по ≤16 КиБ. Большой append делится, oversized replacement возвращает ошибку. Cancellation — отдельный atomic; terminal outcome идёт после output. | `cancelling_a_full_queue_releases_the_producer_without_losing_queued_data`, `concurrent_producers_have_one_order_and_completion_follows_all_bytes`; real bash spool test; plain/document проверяют одинаковые типы payload. Late/disconnected fixtures проверяют metadata-only логирование и невозможность скрыть protocol failure поздним успехом. |
| 3.4 — документ и независимость состояний | [OutputDocument](../src/application/output_document.rs) хранит source, block lifecycle и revisions; строки/layout — производные кеши. История ввода, trajectory, документ и spool разделены. Clear хранит source boundary и не отменяет backend. | Unit-тесты clear/replacement anchors, unchanged source после Markdown render, stale/late rejection. PTY: shrink длинного preview, clear во время append/replace, сохранение частичного ответа и draft после ошибок. Resume/compact сохраняют старый контекст при ошибке/отмене. |
| 3.5 — живой Markdown | [ansi.rs](../src/application/ansi.rs), [markdown_references.rs](../src/application/markdown_references.rs), [markdown_source.rs](../src/application/markdown_source.rs): full-source rendering, termimad styling, CommonMark reference resolution, provenance отображённых символов. [Layout worker](../src/application/document_layout.rs) готовит версии; stale width/generation не применяется. | Open/closed fence, growing nested list, table rows/repeated words, late reference definitions, code/escaped references, clear с сохранением reference context. Ready-result tests удерживают старые кадры через resize/clear/replace; старый open snapshot не становится stable по состоянию нового. |
| 3.6 — viewport и native history | [ManagedRenderer](../src/terminal_renderer/managed.rs) разделяет mutable viewport и stable prefix. [PartialPublication](../src/terminal_renderer/publication.rs) отслеживает исходные символы, включая часть одного большого блока. Не более 128 физических строк публикации за кадр. | Unit/PTY проверяют рост, shrink, finish, browsing, resize, частичную публикацию абзаца и таблицы. Pending-resize test сохраняет кеш/history/cursor публикации при 30→12→80→20, затем проверяет единственность 1200 маркеров. |
| 3.7 — ANSI инструментов | [AnsiDecoder](../src/application/ansi_decoder.rs) хранит UTF-8, escape и style отдельно для stdout/stderr; CR и поддержанные erase controls меняют текущий документ. Управляющие байты не передаются physical writer напрямую. | Все byte boundaries UTF-8/SGR/OSC/CRLF, interleaved streams, incomplete UTF-8 и clear. Mixed PTY: split ANSI, progress overwrite, stdout/stderr, затем Markdown при success/error/cancel. |
| 3.8 — terminal lease | [TerminalController](../src/application/terminal.rs) владеет raw mode, paste, cursor и writer; [SharedInput](../src/common/terminal_input.rs) передаёт непрочитанные байты shell. [PTY session](../src/shell/pty/session.rs) присоединяет forwarder на sentinel и возвращает source/display metadata. | Command Enter + stdin одним пакетом, post-sentinel paste на sh/bash/zsh, unwind cleanup. Повторные leases, resize внутри quiet/output shell, source boundary через wrapped line, primary/alternate/clear. Настоящий Vim: ввод и сохранение файла, resize, возврат на основной экран и следующая shell-команда без утечки TUI в history. |
| 3.9 — cancel и cleanup | JSON future/runtime имеют scope; MCP startup/discovery/call выбирают cancellation и закрываются с deadline; bash использует отдельную process group и присоединяет readers. Application освобождает event receiver до AgentWorker при teardown. Layout worker проверяет stop между этапами и строками. | HTTP cancellation до headers и внутри body; interrupted retry backoff; MCP phase markers + отсутствие дочернего PID после cancel. Реальный MCP через PTY проверяет responsive resize/draft/cancel. Bash descendant удерживает pipe после завершения родителя — cancel закрывает всё. Channel disconnect, worker panic, BrokenPipe, exit/raw flags и layout shutdown проверены отдельно. |
| 3.10 — plain и API | [PlainFrontend](../src/application/plain.rs) пишет в порядке блоков, отдаёт завершённые tool lines постепенно, commit текста выполняет один раз; diagnostics идут в stderr. Чужой Started не сбрасывает pending output. Legacy Agent/String и stdout adapter сохранены. | CLI `headless_and_piped_commands_emit_answer_once_without_ansi`, `headless_sigint_cancels_waiting_http_and_exits_130`, `closing_headless_stdout_cancels_tool_producer_without_hanging`; plain ordering/type/stale-event tests; legacy [stdout_lock](../tests/stdout_lock.rs). |

## Проверка producers и владения терминалом

Аудит ветвей записи выполнен для `agent/loops.rs`, `llm.rs`, `mcp/mod.rs` и
`tools/{registry,bash}.rs`. В managed path используется EventSink; оставшиеся
`with_stdout`, spinner и warning stderr вызываются только legacy adapter-ом при
отсутствии sink. Стандартный stderr stdio MCP-child направлен в null, его stdout
принадлежит протоколу. В `common/terminal_output.rs` больше нет capture и replay.
`Application::run_agent` лишь запускает операцию. Terminal writer передаётся только
UI или эксклюзивному shell lease; agent tools не получают такой writer.

`ActiveOperation` расположен до `AgentWorker` в полях Application: при ошибке
consumer сначала отменяется и закрывает receiver, затем worker присоединяется.
Это важно для terminal outcome в заполненной очереди; реальный BrokenPipe CLI-тест
проверяет отсутствие зависания producer-а на этом пути.

## Производительность и границы доказательств

- PTY-предел реакции ввода/resize/cancel — 250 мс. Для открытого абзаца 890 009 байт
  и code fence 614 020 байт, 18 строк, resize 60→90: последний focused debug-прогон
  дал 35–36 мс на resize/input и 37–42 мс на cancel.
- При ожидании нового layout UI не пересчитывает весь документ синхронно. Он
  обрезает/дополняет готовые видимые строки и отдельно форматирует footer.
  Публикация и её watermark на временном кадре не меняются.
- Handshake останавливает реальное Markdown formatting после парсинга. Join занял
  2,4 мс при тестовом пределе 1 с; queued job не запускается после shutdown.
- Выход после cancel того же большого ответа завершает необходимую публикацию
  примерно за 1,46 с при пределе 2 с и восстанавливает ICANON/ECHO/ISIG.
- Это измерения указанных размеров на текущей машине, не гарантия одинакового
  времени для любого объёма. Source хранится полностью; очередь событий, pending
  layouts и UI tool preview ограничены отдельно. Полный tool output сохраняется
  в spool. Отдельные вызовы стороннего parser-а и системного filesystem read
  не прерываются внутри себя; cancellation проверяется между этапами/порциями.

## Карта этапов и артефактов

R0 подтверждён VT/PTY boundary tests; R1 — reducer/ingress; R2 — worker и responsive
loop; R3 — аудит producers и real JSON/bash/MCP tests; R4 — Markdown, viewport,
source publication и background layout; R5 — lease, cancel, plain и восстановление
терминала. R6 объединяет эти доказательства и финальные команды ниже.

Имена в первоначальной карте файлов были предложением разбиения: вместо отдельного
`output_render.rs` адаптеры расположены в `ansi.rs`, `ansi_decoder.rs`,
`markdown_source.rs` и `markdown_references.rs`. `state.rs` содержит ExecutionState;
существующий UnifiedEditor сохранён. Косметической замены всего editor API нет.
Архитектура и пользовательское поведение описаны в [INPUT_OUTPUT.md](INPUT_OUTPUT.md)
и [README](../README.md). Версия приложения остаётся 0.2.6; Cargo.lock синхронизирован
с зависимостями. Streaming configuration/SSE decoder не добавлялись.

## Финальные команды

```sh
cargo fmt --all -- --check
cargo test --locked --offline
git diff --check
```

Все три команды завершились успешно. Полный набор: **398 unit + 14 PTY/CLI +
1 stdout = 413 passed**, 0 failed, 2 ignored. `--locked --offline` подтвердил
согласованность manifest/lock без обновления зависимостей во время проверки.
Критерии раздела 7 выполнены в описанной среде и пределах измерений; следующий
этап — реализация PLAN_STREAMING через существующий событийный контракт.
Два default ignored-теста различаются: managed fixture child запускается PTY-тестами
как subprocess; старый `ignored_vim_smoke_starts_and_exits` проверяет только version
и не используется как доказательство интерактивного Vim. Новая real-Vim проверка
в текущей среде выполнилась, не была пропущена.
