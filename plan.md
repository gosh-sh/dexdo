# План адаптации TDD-пайплайна (`.claude/roles/`) под DODEX

## 1. Цель

Файлы в `.claude/roles/` сейчас — это калька процесса из проекта Marshall
(`/Users/ekaterinapantaz/elasticlove-bot/marshall/marshall/.claude/roles/`).
Они описывают TDD-цикл «ревьюер → тестировщик → кодер → ревью → приёмка», но
по тексту привязаны к Node/TypeScript-стеку: `npm run lint`, `npm run build`,
`npm test`, `apps/worker/src/`, `vi.mock`, `package.json`-версионирование,
`docs/worker/technical-spec.md` и т.д.

Задача — переписать роли так, чтобы тот же процесс работал на DODEX
(Rust-workspace, Postgres-интеграционные тесты, ветка `dev` как main),
**не меняя суть процесса**. Это не разовая правка `sed`’ом: половина текста
содержит длинные методички (`reviewer-prompts-tester.md` ≈ 19 КБ,
`reviewer-acceptance.md` ≈ 13 КБ), часть рекомендаций бессмысленна вне
Node-мира (DST/CET, keep-alive в http-агенте), часть DODEX-инфры ещё нужно
завести (`prompts/`, mocking-стратегия, README).

План разбит на 8 фаз, чтобы каждую можно было утвердить и закоммитить
отдельно.

---

## 2. Карта различий Marshall → DODEX

Это сжатая «легенда замен», которая применяется почти в каждом файле.

| Аспект | Marshall (как написано в ролях) | DODEX (как должно стать) |
|---|---|---|
| Стек | Node + TypeScript + npm | Rust + cargo (workspace) |
| Build/lint/test | `npm run lint`, `npm run build`, `npm test` | `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace`, `cargo test --workspace` |
| Источник кода | `apps/worker/src/` | `crates/{domain,application,infrastructure}/src/` + `services/{api,indexer}/src/` |
| Источник тестов | `test/` (одна папка) | inline `#[cfg(test)] mod tests` + `crates/*/tests/` + `services/*/tests/` + корневая `tests/` (`.rest`-фикстуры + `tests/e2e/`) |
| Мокинг | `vi.mock`, `mockResolvedValueOnce` | **открытый вопрос** — кандидаты: trait-based DI + hand-written doubles, `mockall`, `wiremock-rs` для HTTP, `sqlx::test` для БД |
| Внешние зависимости | Discord, Linear, OpenAI | Postgres, EVM/TVM RPC, биржевые контракты (`contracts/`) |
| Спека | `docs/product-spec.md` + `docs/worker/technical-spec.md` + `docs/worker/plan.md` + `docs/domain-glossary.md` + `docs/naming-conventions.md` | `docs/api-spec.md` + `docs/tech-specs/*` + `docs/contract-specs/*` + `internal-docs/*` (см. фазу 8 — реструктуризация) |
| Версионирование | `package.json` patch/minor bump на шаг | `Cargo.toml` → `workspace.package.version` — один на всю репу; **открытый вопрос**: ввести per-step bump или дропнуть |
| Changelog | `CHANGELOG.md` обязателен | в DODEX отсутствует; **открытый вопрос**: завести или дропнуть из пайплайна |
| Branch main | `main` | `dev` (см. `git remote show origin`) |
| CI | GitHub Actions: `test`, `docker-build`, `secrets-scan` | в DODEX **нет** `.github/workflows/`; есть только `scripts/validate-*.sh` (codex CLI) |
| Pre-commit doc-sweep | не упомянуто | **обязателен** по [`AGENT_REQUIREMENTS.md:25-31`](AGENT_REQUIREMENTS.md) — перечитать все `docs/`, корневой `README.md` и READMEs затронутых компонентов перед каждым `git commit` |
| Postgres для тестов | sqlite, поднимается сам | Disposable test Postgres — поднимать вручную как описано в `README.md#test-postgres` (см. [`AGENT_REQUIREMENTS.md:35`](AGENT_REQUIREMENTS.md)) |
| Конфиги | `config.json`, `user_map.csv` | `config/*` (нужно сверить); секреты в `.env` |
| GH_TOKEN flow | `.env` парсится `awk`’ом перед каждым `gh` | оставить как есть, если у владельца тот же сетап |

Эти замены — основа всех правок. Дальше — по фазам.

---

## 3. Открытые вопросы (решить ДО старта)

Эти ответы определяют форму итоговых ролей. Прошу подтвердить/выбрать перед
переходом к фазе 1.

1. **CHANGELOG.md** — заводим в DODEX или дропаем из пайплайна?
   Рекомендация: дропнуть. Doc-sweep по `AGENT_REQUIREMENTS.md` уже покрывает
   синхронизацию документации; история живёт в PR body + git log. Меньше
   шума в репе, меньше пунктов «забыл обновить CHANGELOG» на приёмке.

2. **Per-step version bump** — нужен или дропаем?
   Рекомендация: дропнуть. `workspace.package.version = "0.1.0"` — единый на
   репу, до релиза смысла поднимать его поэтапно мало. Версионирование
   ввести позже, когда понадобится релизный канал.

3. **Mocking-стратегия для Rust-тестов**. Без выбора пайплайн нельзя
   написать конкретно — секции «моки только на границе HTTP-вызова» должны
   ссылаться на конкретный инструмент. Кандидаты:
   - HTTP — `wiremock-rs` (мок-сервер, ловит реальные `reqwest`-запросы)
   - Trait-объекты приложения — `mockall` или ручные test-doubles в
     отдельном модуле под `#[cfg(test)]`
   - Postgres — `sqlx::test` + миграции на реальной disposable БД
   Рекомендация: HTTP = `wiremock-rs`, доменные трейты = ручные doubles
   (компактнее, явнее), БД = `sqlx::test` без моков.

4. **Запускаем ли CI на GitHub Actions** или оставляем только локальные
   `scripts/validate-*.sh`? От ответа зависит, что писать в
   `reviewer-acceptance.md` § «CI» — целую секцию или просто «не
   применимо».

5. **`docs/plan.md`** — нужна ли многошаговая дорожная карта (как
   `docs/worker/plan.md` в Marshall)? Если да — она становится опорой
   фазы 0a discovery (см. `reviewer.md`); если нет — упоминания плана из
   ролей и validator-скриптов нужно вычистить.

6. **`prompts/` структура** — подтверждаем структуру:
   ```
   prompts/
     coder/
       done/                    # архив завершённых циклов
     tester/
       done/
     context/
       _template.md             # шаблон 0a–0d артефактов
       <task>.md                # context-файл активной задачи
   ```
   ?

7. **Реструктуризация `docs/`** — делаем в фазе 8 (см. ниже) или
   откладываем? Если откладываем — роли пишем под текущую структуру и
   потом ещё раз правим.

8. **DODEX `README.md`** — сейчас он содержит контент **Marshall**
   (упоминает Discord/Linear, npm, sqlite). Это похоже на оставшийся
   артефакт копирования. Переписываем под DODEX как часть фазы 1 или
   отдельно?

---

## 4. Фазы (каждая = один аппрув-цикл)

### Фаза 0. Подготовка дерева (структурная, без редактирования контента ролей)

**Цель:** завести каркас, на который опираются роли, чтобы пайплайн вообще
мог запуститься.

Шаги:
1. Создать `prompts/coder/done/.gitkeep`, `prompts/tester/done/.gitkeep`,
   `prompts/context/.gitkeep`.
2. Написать `prompts/context/_template.md` со скелетом секций 0a–0d
   (спека / трассировка / дизайн / скоуп), который требует `reviewer.md`.
3. Решить вопрос CHANGELOG (см. §3.1). Если «завести» — создать пустой
   `CHANGELOG.md` со заголовком и `Unreleased`-секцией.
4. Решить вопрос DODEX-овский `README.md` (см. §3.8). Если «переписать» —
   сделать в этой фазе.
5. Проверить, что `dev` действительно ветка-цель: `git remote show
   origin | grep HEAD`. Зафиксировать в плане.
6. Создать раздел `## Test Postgres` в `README.md`, если его нет:
   `AGENT_REQUIREMENTS.md:35` ссылается на `README.md#test-postgres`, а
   текущий `README.md` Marshall-овский.

**Артефакт:** структура `prompts/` + актуальный `README.md` + (опц.)
`CHANGELOG.md`. Без правок `.claude/roles/`.

---

### Фаза 1. Адаптация ядра — `reviewer.md`, `coder.md`, `tester.md`, `commands.txt`

**Цель:** минимально-достаточный набор, чтобы запустить один полный цикл
вручную.

Файлы:
- `.claude/roles/reviewer.md`
- `.claude/roles/coder.md`
- `.claude/roles/tester.md`
- `.claude/commands.txt`

Правки в `reviewer.md` (детально):

| Строка | Что | Чем заменить |
|---|---|---|
| 1 | «проекта Marshall» | «проекта DODEX» |
| 8 | `npm run lint`, `npm run build`, `npm test` | `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace`, `cargo test --workspace`. Добавить памятку про disposable test Postgres из `README.md#test-postgres`. |
| 17 | список «можешь править» (`docs/worker/technical-spec.md`, `docs/saas/saas-plan.md`) | `docs/api-spec.md`, `docs/tech-specs/`, `docs/contract-specs/`, `internal-docs/`, `README.md`, `.claude/roles/`, `prompts/`, `scripts/`. (Если фаза 8 принята — обновить под новые пути.) |
| 32 | «никогда в main» | «никогда в `dev`»; «ветка от main» → «ветка от `dev`» |
| 38 | «Кодер не трогает test/, тестировщик может писать только в test/» | Адаптировать под Rust: «Кодер не трогает `tests/` (root) и `crates/*/tests/`, `services/*/tests/`, **и `#[cfg(test)] mod tests` внутри `*.rs`**. Тестировщик пишет ТОЛЬКО в эти места + может править существующие inline-test-модули в продакшен-файлах **только когда это явно указано в промпте**.» (Это нетривиальное решение — inline-test-модули физически лежат в коде кодера. Нужна явная норма.) |
| 116–118 | «Каждый PR поднимает версию в package.json» | По решению из §3.2: либо адаптировать под `Cargo.toml` `workspace.package.version`, либо удалить секцию. |
| 125–138 | GH_TOKEN из `.env` | Сохранить, если у владельца тот же flow в DODEX |
| 143, 152 | `main` | `dev` |
| Добавить новую секцию | — | «Pre-commit doc-sweep» — выдержка из `AGENT_REQUIREMENTS.md:25-31` с явным правилом: перед каждым `git commit` ревьюер перечитывает все `docs/` + корневой `README.md` + README затронутых компонентов и обновляет всё, что инвалидируется staged-дифом. |
| Добавить новую секцию | — | «Test Postgres» — перед `cargo test` обязательно поднять disposable Postgres по `README.md#test-postgres` (см. `AGENT_REQUIREMENTS.md:35`). |

Правки в `coder.md`:
- `apps/worker/src/` → `crates/*/src/`, `services/*/src/` (с пояснением, что
  трогать можно и доменные крейты, и сервисные)
- `npm run lint`/`npm run build` → `cargo fmt --check`, `cargo clippy`,
  `cargo build`
- «`npm test` запускает ревьюер» → `cargo test` запускает ревьюер. Но: тут
  важный нюанс — `cargo test` запускает И inline `#[cfg(test)]`. Это
  пересекается с правилом «кодер не трогает тесты». Решение: «кодер
  компилирует и не запускает `cargo test`; запуск — задача ревьюера на
  ревью коммита»
- `test/` → `tests/` + список inline-test-локаций
- Не править: добавить `docs/api-spec.md`, `docs/tech-specs/`, `migrations/`
  (поскольку schema-changes — это часть промпта, миграции пишет ревьюер
  или явно делегирует кодеру по промпту)

Правки в `tester.md`:
- Все упоминания `apps/worker/src/` → `crates/*/src/`, `services/*/src/`
- `docs/worker/technical-spec.md`, `docs/product-spec.md` → `docs/api-spec.md`,
  `docs/tech-specs/`
- `test/` → `tests/`, `crates/*/tests/`, `services/*/tests/`, плюс
  inline-test-модули (см. §1 выше про норму)
- Секция «keep-alive HTTP» (`agent: false`) → удалить или заменить на
  Rust-эквивалент: при тестах сервера на одном порту использовать
  `SO_REUSEADDR` / получать порт через `127.0.0.1:0` (биндить нулевой
  порт, читать назначенный). Это разумный аналог.
- Секция «`vi.mock` config isolation» → заменить на стратегию из §3.3:
  `wiremock-rs` для внешнего HTTP, hand-written test-doubles для
  доменных трейтов, `sqlx::test` для БД. Дать конкретный snippet.
- «Не удаляй файлы вне test/» → правило сохраняем, пути обновляем
- `npm test` → `cargo test`
- Уровни тестирования (unit / integration / e2e / fault tolerance /
  observability) — оставить как есть, это framework-agnostic

Правки в `commands.txt`:
- Файлы и пути не меняются, но проверить, что `--system-prompt` принимает
  multibyte / правильно подхватывает русский текст в свежем `claude`.

**Артефакт:** обновлённые `reviewer.md`, `coder.md`, `tester.md`,
`commands.txt`. После этой фазы пайплайн уже способен запуститься на
DODEX, хотя промпт-генерация (фаза 2) ещё неполная.

---

### Фаза 2. Адаптация промпт-методичек — `reviewer-prompts-tester.md`, `reviewer-prompts-coder.md`

**Цель:** обновить две самые длинные методички, которые ревьюер
перечитывает перед каждым промптом.

Объём: ~19 КБ + ~9 КБ. Адаптация двухслойная — пути/команды +
содержательная (риск-таксономия, мокинг, чек-листы grep’ом).

`reviewer-prompts-tester.md` — что меняется:

1. **Все пути** (`apps/worker/src/`, `test/`, `db/`,
   `config.example.json`, `package.json`, `CHANGELOG.md`) → DODEX-эквиваленты
   (см. §2). Особое внимание: targeted `rg` корни в чек-листах (строки
   151–153, 156–168) — там список путей, его надо переписать целиком.

2. **Внешние сервисы** — `Discord, Linear, OpenAI` (строки 86, 184) →
   реальные внешние границы DODEX. По коду (см. discovery-репорт):
   blockchain RPC (TVM), Postgres (внутренняя, мокать НЕ нужно по
   §3.3), любые внешние REST/индексер-точки. Перечислить явно.

3. **Mocking idioms** — `vi.mock`, `mockResolvedValueOnce`,
   `as Record<string, unknown>` (строки 134–143) → Rust-идиомы:
   `wiremock::Mock::given(...).mount(&server).await`, hand-written
   test-doubles, `sqlx::PgPool` через `sqlx::test`. Дать 1–2 примера
   кода прямо в методичке.

4. **DST/CET секция** (строки 110–116) — это специфика Marshall
   (Belgrade-таймзона, polling по слотам). В DODEX биржа работает 24/7,
   DST-релевантности нет. **Удалить**, заменить общим правилом про
   «time-zone-зависимая логика — тестируй UTC и одну non-UTC».

5. **Риск-таксономия P0/P1/P2/P3** (строки 30–40) — текст про «блокировка
   потока, пропуск/дубль уведомлений» (Marshall-specific). Переписать
   под DODEX-риски:
   - P0: рассогласование стакана/лимитов, потеря/двойное исполнение
     ордера, рассинхрон on-chain ↔ индексер, проигнорированный реорг
   - P1: неверные переходы статуса ордера, ретраи к RPC без
     идемпотентности, потеря событий при рестарте индексера, ошибки
     миграций
   - P2/P3 — оставить общими
   Запросить у владельца список реальных P0/P1 — он лучше знает домен.

6. **State machine / lifecycle / scheduler** секции (строки 92–127) —
   оставить структуру, но примеры заменить с «scheduler/tick» на
   «order-state-machine», «indexer-tick», «reconciliation-loop». Без
   подтверждения владельца — пометить как `TBD`.

7. **«Существующие тесты НЕ ТРОГАТЬ»** + классификация A/B/C — это
   framework-agnostic, переносится дословно.

`reviewer-prompts-coder.md` — что меняется:
- Все пути и команды (как выше)
- «🔴 Rename = полное вытирание старого» (строки 30–34): grep-чек
  переписать под Rust-корни: `rg "<old>" crates/ services/ tests/ docs/
  config/ migrations/`
- «Grep по всем call sites перед промптом» (строки 36–46) — список
  корней (`apps/worker/src`, `test`, `db`, `docs`, `config.example.json`,
  `package.json`, `CHANGELOG.md`) → `crates/`, `services/`, `tests/`,
  `docs/`, `internal-docs/`, `migrations/`, `Cargo.toml`,
  (опц.) `CHANGELOG.md`
- «Принятые тесты неприкосновенны» + классификация — переносится
  дословно

**Артефакт:** две методички адаптированы и валидируются на одном
тестовом цикле (можно прогнать `scripts/validate-prompts.sh` на
синтетическом промпте).

---

### Фаза 3. Адаптация ревью коммитов — `reviewer-review-coder.md`, `reviewer-review-tester.md`

**Цель:** обновить чек-листы, по которым ревьюер принимает коммит
кодера и тестировщика.

Правки в `reviewer-review-coder.md`:
- §1 «Границы»: `git show --name-only <коммит>` → правило «только файлы
  ВНЕ `tests/`, `crates/*/tests/`, `services/*/tests/`. inline
  `#[cfg(test)]`-блок внутри `crates/*/src/foo.rs` — допустим
  **только**, если промпт явно разрешает». Дать пример.
- §3 «Прочитай diff»: `git diff HEAD~1..HEAD -- apps/worker/src/` →
  `git diff HEAD~1..HEAD -- crates/ services/ migrations/` (исключая
  тесты — добавить `:!tests/ :!**/tests/`)
- §3 discovery-pass: корни для targeted `rg` обновить (как в фазе 2)
- §4 «Запусти проверки»: команды → cargo (см. §2)
- §5 классификация A/B/C — оставить дословно, обновить пути
- §7 «Cross-model валидация»:
  `scripts/validate-implementation.sh "prompts/coder/..." ".claude/roles/reviewer-review-coder.md" ":!test/"` →
  `":!tests/ :!**/tests/"`. **Внимание:** сам скрипт уже захардкодил
  Marshall-пути в промпт-текст (см. `validate-implementation.sh:34`
  упоминание `docs/worker/plan.md`, и `:43` — `apps/worker/src test db
  docs config.example.json package.json CHANGELOG.md`). Фаза 6
  адаптирует скрипты — но в `reviewer-review-coder.md` нужно
  одновременно прописать новый `DIFF_SCOPE`.

Правки в `reviewer-review-tester.md`:
- §1 «Границы»: `test/` → `tests/` (+ список разрешённых путей)
- §7 validation: scope `"test/"` → `"tests/ **/tests/"`
- Остальное — переносится с обновлением путей

**Артефакт:** два файла адаптированы. После этой фазы можно полноценно
ревьюить коммиты тестировщика и кодера.

---

### Фаза 4. Адаптация приёмки — `reviewer-acceptance.md`

**Цель:** перепрошить финальный чек-лист.

Этот файл наиболее завязан на open questions §3 — порядок правок зависит
от решений по CHANGELOG, версионированию, CI.

Правки:
- **§0 «Версия и CHANGELOG»** — переписать по решению из §3.1–3.2. Если
  обе фичи дропаются — секцию удалить полностью. Если только одна —
  оставить ту, что выбрана.
- **§1 проверки**: команды → cargo
- **§1 docs-sweep** — усилить ссылкой на `AGENT_REQUIREMENTS.md` (это
  обязательное правило, не «опционально»)
- **§1.1 финальный discovery-pass**: корни targeted `rg` → DODEX
  (`crates`, `services`, `tests`, `docs`, `internal-docs`, `migrations`,
  `Cargo.toml`, опц. `CHANGELOG.md`)
- **§1.2 финальный cross-model gate** — оставить, обновить пример
  команды. Базовый ref `main` → `dev`.
- **§2 CI** — по решению из §3.4: переписать под GitHub Actions
  DODEX, либо удалить, либо заменить на «локально прогнать
  `scripts/validate-implementation.sh` дважды (на коммит + на ветку)»
- **§3 уборка** — `main` → `dev` везде. `prompts/` flow остаётся.

**Артефакт:** обновлённая методичка приёмки.

---

### Фаза 5. Адаптация `reviewer-validated.md`

Файл — расширенная версия `reviewer.md` (23 КБ, я прочитал первые ~120
строк). Дублирует базовое содержание + добавляет cross-model-валидацию,
запуск агентов через Agent tool, секцию о валидаторе. Содержит те же
project-specific reference, что и `reviewer.md`.

Подход: после правок `reviewer.md` (фаза 1) — пройти `reviewer-validated.md`
и применить ту же `s/Marshall/DODEX/`, `s/npm/cargo/`, `s/main/dev/`,
`s/test\//tests\//`, `s/apps\/worker\/src/crates,services/g` логику +
сверить совпадающие секции с `reviewer.md` (они должны остаться
совместимы).

Доп. внимание:
- §17 «НЕ указывай параметр `model`» — DODEX-агностично, переносится
- Ссылка на `docs/worker/technical-spec.md` в описании cross-model
  валидации (строки 57, 61) → актуальная спека DODEX
- Ссылка на `docs/worker/plan.md` (если решено НЕ заводить — удалить;
  если заводить — путь обновить)

**Артефакт:** `reviewer-validated.md` синхронизирован с DODEX.

---

### Фаза 6. Фикс `scripts/validate-prompts.sh` и `scripts/validate-implementation.sh`

Оба скрипта **уже существуют** в DODEX, но содержат Marshall-пути в
promp-тексте, который скармливается codex’у:

- `scripts/validate-implementation.sh:34` — `docs/worker/plan.md`
- `scripts/validate-implementation.sh:43` — `apps/worker/src test db
  docs config.example.json package.json CHANGELOG.md`
- `scripts/validate-prompts.sh:28-35` — длинный список несуществующих
  файлов: `docs/product-spec.md`, `docs/worker/technical-spec.md`,
  `docs/worker/conversation-lifecycle.md`,
  `docs/worker/coordination-flows.md`, `docs/worker/db-schema.md`,
  `docs/domain-glossary.md`, `docs/naming-conventions.md`,
  `docs/worker/plan.md`
- `scripts/validate-prompts.sh:38` — `apps/worker/src/ или test/`
- `scripts/validate-prompts.sh:40` — те же корни поиска

Правки:
1. Заменить список «Specs (read ALL of these)» на актуальные DODEX-файлы
   (после фазы 8 — на новую структуру). До решения §3.5 и §3.7 — не
   ссылаться на `plan.md` и не упоминать несуществующие спеки.
2. Заменить все списки корней targeted `rg` на DODEX-эквиваленты.
3. Заменить упоминания `apps/worker/src/` в инструкциях валидатору.
4. Проверить `mkdir -p prompts/context` — это уже создаст структуру в
   первой же прогонке (нужно сверить с фазой 0).

**Артефакт:** оба скрипта прогонимы на синтетических промптах без
ругани на отсутствующие файлы.

---

### Фаза 7. Прогон одного полного цикла на тестовой задаче

**Цель:** валидировать процесс на безопасной мелкой задаче (например,
правка одного хелпера или добавление одного эндпоинта), не сломав
основной поток разработки.

Шаги:
1. Выбрать задачу совместно с владельцем — мелкую, изолированную, со
   ссылкой на конкретную секцию `docs/`.
2. Ревьюер по `reviewer.md` проходит 0a–0d, пишет промпты, прогоняет
   `validate-prompts.sh`.
3. Прогоняется тестировщик → ревью коммита.
4. Прогоняется кодер → ревью коммита → `cargo test`.
5. Приёмка по `reviewer-acceptance.md`.
6. Записать всё, где роли работали неточно или мешали → составить
   список патчей для фазы 7-а (back-fix ролей).

**Артефакт:** один PR в `dev`, прошедший весь цикл; список расхождений
между ролями и реальностью.

---

### Фаза 8 (опциональная). Реструктуризация `docs/` под нужды пайплайна

Эта фаза отвечает на твою просьбу про «лучшую структуру папок и
названий».

#### 8.1 Что не так с текущей структурой

Сейчас:
```
docs/
  api-spec.md
  tech-specs/
    auth.md
    data-schema.md
    market-data-api.md
    market-data-indexer.md
    trading-api/
      read-api.md
      write-api.md
  contract-specs/
    dex-events-routing.md
    dex-contracts-external-flows.html
    dex-contracts-object-diagram.drawio
    dex-contracts-system.html
internal-docs/
  MMflow.md
  TODO.md
  blch-integration.md
```

Что показал pre-commit sweep (важно для фазы 8):
- `docs/tech-specs/trading-api/read-api.md` и `write-api.md` — **пустые файлы (0 строк)**. Это заглушки, а не спеки. До фазы 8 нужно либо заполнить их, либо удалить.
- `internal-docs/blch-integration.md` назван как «blockchain integration», но по содержимому это **параллельная API-спека** (`# DOXEX API Specification`, описывает `PrivateNote.placeOrder/cancelOrder/placeBatch` и т.д.). Перекликается с `docs/api-spec.md`. Перед фазой 8 надо решить: это (a) актуальная on-chain RPC-спецификация (тогда название честное → `tech-specs/contracts/onchain-rpc.md` или подобное), (b) устаревший дубль `api-spec.md` (тогда удалить), (c) черновик нового контракта (оставить в internal-docs/ как есть).

Проблемы относительно процесса:
1. **Несимметричная вложенность.** `tech-specs/trading-api/` —
   подпапка с двумя файлами; `market-data-api.md` и
   `market-data-indexer.md` — плоские, хотя это та же логика «папка на
   компонент».
2. **Граница `docs/` vs `internal-docs/` нечёткая.** `MMflow.md` и
   `blch-integration.md` — это содержательные спеки (market-making
   flow, blockchain integration), но лежат как «черновики». Ревьюер по
   процессу не имеет права на них опираться — а они нужны.
3. **Нет «продуктовой» спеки.** В Marshall было `docs/product-spec.md`
   — что мы обещаем пользователю. В DODEX явного эквивалента нет;
   `api-spec.md` описывает HTTP-контракт, но не инварианты
   пользовательского поведения (например, «order не может исполниться
   дважды», «частичное исполнение возвращает остаток в стакан»).
4. **Нет глоссария.** `validate-prompts.sh:34` ссылается на
   `docs/domain-glossary.md`. В DODEX его нет.
5. **Нет naming-conventions.** Ссылка из того же скрипта:35.
6. **Нет активного плана.** В Marshall был `docs/worker/plan.md` —
   многошаговый план фичи с REQ-нумерованными инвариантами. Ревьюер
   `reviewer.md` §0a опирается на «REQ из спеки» — без нумерованных
   опор будет хуже.
7. **HTML/drawio в контракт-спеках.** Графические артефакты
   (`*.html`, `*.drawio`) — норм, но они должны иметь рядом
   текстовый «human-readable summary» с инвариантами, иначе ревьюер
   не сможет процитировать `file:line`.

#### 8.2 Предлагаемая структура

```
docs/
  README.md                         # nav-индекс: что в какой спеке
  product-spec.md                   # NEW — пользовательские инварианты DEX
  api-spec.md                       # как есть — публичный REST-контракт
  domain-glossary.md                # NEW — бизнес-термины (order, lot, depth, settlement...)
  naming-conventions.md             # NEW — правила именования в Rust-коде
  plan.md                           # NEW (опц.) — активный многошаговый план
  tech-specs/
    auth.md
    data-schema.md
    market-data/                    # был market-data-api.md + market-data-indexer.md
      api.md
      indexer.md
    trading-api/                    # как есть
      read-api.md
      write-api.md
    contracts/                      # был docs/contract-specs/
      events-routing.md             # был dex-events-routing.md
      external-flows.html
      object-diagram.drawio
      system.html
      invariants.md                 # NEW — текстовые инварианты, на которые опираются HTML/drawio
    integrations/                   # NEW — продвинутые internal-docs
      blockchain.md                 # был internal-docs/blch-integration.md
      market-making.md              # был internal-docs/MMflow.md
internal-docs/
  TODO.md                           # остаётся как scratch — пайплайн его игнорирует
```

#### 8.3 Принципы

1. **Один canonical корень = `docs/`.** Всё, на что опирается ревьюер,
   живёт здесь. `internal-docs/` — только для черновых заметок,
   которые пайплайн не читает.
2. **Папка на компонент.** Каждая логическая часть системы получает
   подпапку в `tech-specs/<component>/`. Это даёт симметрию и масштаб:
   когда появится `liquidation/` или `oracle/`, форма уже готова.
3. **Граф + текст рядом.** HTML/drawio остаются в `contracts/`, но
   рядом всегда есть `.md`-файл, в котором инварианты сформулированы
   текстом со ссылками на код. Ревьюер цитирует `.md`, не HTML.
4. **Spec ≠ README.** Корневой `README.md` — entry point + ссылки.
   `docs/README.md` — навигация по спекам. Спеки = тех-документы с
   нумерованными инвариантами.
5. **Inline ID-инварианты.** В каждом `.md` под `tech-specs/` ввести
   простую нумерацию `### INV-MD-001` (например, `INV-MD-001` =
   `market-data invariant 001`). Это даёт стабильные ссылки для
   контекст-файлов и cross-model валидатора.
6. **`plan.md` — опционально.** Если решаем НЕ вести многошаговый
   план — `plan.md` не создаём, упоминания из ролей убираем.

#### 8.4 Шаги фазы 8

1. Подтвердить структуру выше с владельцем (могут быть возражения по
   именованиям — `integrations/` vs `flows/`, `contracts/` внутри
   `tech-specs/` vs отдельно).
2. Создать новые файлы: `README.md`, `product-spec.md`,
   `domain-glossary.md`, `naming-conventions.md`, `plan.md` (если
   решено).
3. Переместить `internal-docs/{MMflow,blch-integration}.md` в
   `docs/tech-specs/integrations/`. Обновить все ссылки в репе.
4. Переместить `docs/contract-specs/*` в `docs/tech-specs/contracts/`,
   создать `invariants.md`.
5. Разбить `market-data-api.md`/`-indexer.md` на подпапку.
6. В **тот же коммит** обновить:
   - `AGENT_REQUIREMENTS.md` (там захардкожены пути:23 — `docs/api-spec.md`,
     `docs/tech-specs/...`, `data-schema.md`)
   - все ссылки в `README.md`
   - все ссылки в `.claude/roles/*.md`
   - все ссылки в `scripts/validate-*.sh`
7. Прогнать pre-commit doc-sweep — это сама фаза 8 и есть на практике.

**Артефакт:** новая структура `docs/` + все ссылки в репе обновлены за
один атомарный коммит (требование `AGENT_REQUIREMENTS.md`).

---

## 5. Порядок выполнения

```
Фаза 0  (подготовка дерева)        — без блокеров, можно стартовать сейчас
Фаза 1  (ядро ролей)               — нужны ответы на §3.1, 3.2, 3.6
Фаза 2  (промпт-методички)         — нужны ответы на §3.3, 3.5
Фаза 3  (ревью коммитов)           — после фазы 1
Фаза 4  (приёмка)                  — нужны ответы на §3.1, 3.2, 3.4
Фаза 5  (reviewer-validated.md)    — после фазы 1
Фаза 6  (validate-*.sh)            — нужны ответы на §3.5, 3.7
Фаза 7  (пилотный цикл)            — после фаз 1–6
Фаза 8  (реструктуризация docs/)   — опционально; либо до фазы 1 (тогда
                                      роли сразу пишутся под новые пути),
                                      либо после фазы 7 (тогда роли
                                      адаптируются дважды)
```

Рекомендуемый порядок: ответить на §3, потом фаза 0, потом подумать о
порядке 8 ↔ 1 (если адаптировать дважды лень — сначала 8, потом 1–6
сразу под новые пути).

---

## 6. Что план НЕ покрывает

- Не пишет конкретный текст ролей — это работа фаз 1–5.
- Не выбирает за владельца ответы на §3 — это блокирующие решения.
- Не вносит изменений в репу. Это документ с планом, ничего больше.
- Не покрывает обучение/онбординг новых агентов — это отдельная задача
  после стабилизации ролей.
