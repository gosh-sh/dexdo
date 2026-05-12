# DODEX — контекст проекта

DODEX — бэкенд децентрализованной биржи на Acki Nacki. Два Rust-сервиса разделяют общий Postgres read-model:

- `services/api` — HTTP-сервис публичного REST API
- `services/indexer` — ингестор on-chain событий, который наполняет read-model

On-chain контракты DODEX лежат в `contracts/` (TVM/Solidity-подобный).

## Источники истины

- [`AGENT_REQUIREMENTS.md`](AGENT_REQUIREMENTS.md) — правила для любого агента, делающего изменения в репо (включая обязательный pre-commit doc-sweep по `docs/` и затронутым README)
- [`docs/README.md`](docs/README.md) — карта документации с владельцами файлов
- [`docs/api-spec.md`](docs/api-spec.md) — **публичный REST-контракт. Единый источник истины для HTTP-поведения. SACRED — не редактируется без явного разрешения владельца контракта.**
- [`docs/tech-specs/read-api.md`](docs/tech-specs/read-api.md) — implementation spec для всех GET-эндпоинтов (markets, depth, account, openOrders, allOrders)
- [`docs/tech-specs/write-api.md`](docs/tech-specs/write-api.md) — implementation spec для write-эндпоинтов (order, batchOrders, openOrders DELETE)
- [`docs/tech-specs/indexer.md`](docs/tech-specs/indexer.md) — ингестия chain-событий, проекторы, реконсайлеры (`services/indexer`)
- [`docs/tech-specs/auth.md`](docs/tech-specs/auth.md) — аутентификация, авторизация, жизненный цикл аккаунта/api_key
- [`docs/tech-specs/data-schema.md`](docs/tech-specs/data-schema.md) — Postgres-таблицы, индексы, миграции
- [`docs/contract-specs/`](docs/contract-specs/) — on-chain контракты DODEX: `dex-events-routing.md` + HTML/drawio-диаграммы

Конфигурация:

- `config/api.<env>.yaml`, `config/indexer.<env>.yaml` — конфиг-файлы сервисов. `config/api.local.yaml` и `config/indexer.local.yaml` — локальные дефолты, в репо
- `.env.example` — шаблон секретов (в репо). Реальный `.env` — gitignored, копируется на первом checkout'е
- `config/*.stage.supabase.yaml` — gitignored, локально для stage-окружения

Спеку и публичный API не менять без явного разрешения владельца.

## Структура проекта

- `crates/` — библиотечные крейты:
  - `crates/domain/` — доменные типы
  - `crates/application/` — use cases
  - `crates/infrastructure/` — адаптеры (Postgres, TVM runner, GraphQL gateway)
- `services/` — исполняемые сервисы:
  - `services/api/` — HTTP REST API
  - `services/indexer/` — chain-event ингестор
- `contracts/` — on-chain DODEX-контракты (TVM)
- `migrations/` — SQL-миграции (`NNNN_*.sql`), применяются `sqlx::migrate!` при старте сервисов
- `config/` — конфиг-файлы (`api.<env>.yaml`, `indexer.<env>.yaml`)
- `tests/` — корневые integration-фикстуры (`.rest`-файлы, `tests/e2e/`)
- `crates/*/tests/`, `services/*/tests/` — per-crate integration-тесты
- Inline unit-тесты — `#[cfg(test)] mod tests` внутри `crates/*/src/*.rs` и `services/*/src/*.rs`
- `docs/` — документация (см. «Источники истины» выше)
- `CHANGELOG.md` (в корне) — по датам
- `scripts/` — пайплайн-скрипты валидации (`validate-prompts.sh`, `validate-implementation.sh`)
- `prompts/` — задания агентам:
  - `prompts/coder/` — задания кодеру
  - `prompts/tester/` — задания тестировщику
  - `prompts/context/` — context-файлы по задачам (фаза 0a–0d)
  - `prompts/{coder,tester}/done/` — выполненные задания
- `.claude/roles/` — описания ролей агентов
- `.claude/commands.txt` — команды запуска агентов
- `docker-compose.yml`, `docker-compose.test.yml`, `docker-compose.stage.yml` — Docker compose файлы (test compose поднимает disposable Postgres для тестов)

## Стек

- Rust 2024 edition, workspace из 5 крейтов (3 библиотечных + 2 исполняемых)
- Tokio async runtime
- Salvo как HTTP-фреймворк (`services/api`)
- sqlx (PostgreSQL, runtime-tokio-rustls, миграции)
- reqwest для исходящих HTTP (`rustls-tls`)
- tvm-sdk (`tvm_abi`, `tvm_block`, `tvm_executor`, `tvm_types`, `tvm_vm`) для off-chain TVM-эмуляции
- serde / serde_json / serde_yaml для (де)сериализации
- tracing / tracing-subscriber для observability
- zeroize для безопасного хранения секретов в памяти
- num-bigint для арифметики на uint256/uint128

Тестирование:

- `cargo test --workspace` для unit + integration
- `sqlx::test` для DB-зависимых тестов (требует поднятый test Postgres из `docker-compose.test.yml`)
- `wiremock-rs` для моков HTTP-границы
- `mockall` для моков доменных трейтов (если действительно нужно мокать)

## Агенты и роли

В проекте одна триада агентов на бэкенд (`services/`, `crates/`):

- **Ревьюер** ([`.claude/roles/reviewer.md`](.claude/roles/reviewer.md)) — анализирует код, управляет кодером и тестировщиком, принимает шаги
  - Расширенная версия для cross-model-валидации: [`.claude/roles/reviewer-validated.md`](.claude/roles/reviewer-validated.md)
  - Фазовые файлы ревьюера:
    - [`.claude/roles/reviewer-prompts-tester.md`](.claude/roles/reviewer-prompts-tester.md) — правила написания промптов тестировщику, матрица сценариев
    - [`.claude/roles/reviewer-prompts-coder.md`](.claude/roles/reviewer-prompts-coder.md) — правила написания промптов кодеру, трассировка тестов
    - [`.claude/roles/reviewer-review-tester.md`](.claude/roles/reviewer-review-tester.md) — ревью коммита тестировщика
    - [`.claude/roles/reviewer-review-coder.md`](.claude/roles/reviewer-review-coder.md) — ревью коммита кодера
    - [`.claude/roles/reviewer-acceptance.md`](.claude/roles/reviewer-acceptance.md) — чеклист финальной приёмки шага
- **Кодер** ([`.claude/roles/coder.md`](.claude/roles/coder.md)) — пишет код в `crates/*/src/` и `services/*/src/`, не трогает тестовые локации
- **Тестировщик** ([`.claude/roles/tester.md`](.claude/roles/tester.md)) — пишет тесты в `tests/`, `crates/*/tests/`, `services/*/tests/`, не правит продакшен-код

При запуске каждому агенту даётся команда: «Прочитай `.claude/roles/<роль>.md` — это твоя роль.» См. `.claude/commands.txt`.

Префиксы коммитов: `[coder]`, `[tester]`, `[reviewer]` — ревьюер по ним проверяет границы.

Skill для пайплайна: [`.claude/skills/reviewer-core/SKILL.md`](.claude/skills/reviewer-core/SKILL.md).

Cross-model gate выполняется через `scripts/validate-prompts.sh` (после написания промптов) и `scripts/validate-implementation.sh` (после ревью коммитов и финально на ветку).

## Git / Merge

- **Main-ветка проекта — `dev`** (не `main`). Вся работа — в feature/fix-ветках от `dev`. В `dev` попадает только через squash merge PR.
- **В `dev` можно только squash merge** (`gh pr merge --squash`). Обычный merge и rebase запрещены.
- **🔴 ЗАПРЕЩЕНО мержить PR без явной команды владельца «мержи»/«merge».** Ревьюер создаёт PR → ждёт. Мерж ТОЛЬКО по команде владельца.
- **🔴 ЗАПРЕЩЕНО использовать `--admin` флаг** в `gh pr merge`. Branch protection существует намеренно. Обход = нарушение.

Для `gh` CLI экспортируй `GH_TOKEN` из `.env`:

```bash
export GH_TOKEN=$(awk -F' *= *' '/^GH_TOKEN/{print $2}' .env)
```

## CI/CD

CI на GitHub Actions в DODEX пока **нет** — проверки гоняются локально:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
docker compose -f docker-compose.test.yml up -d --wait
cargo test --workspace
docker compose -f docker-compose.test.yml down
```

`docker-compose.test.yml` поднимает disposable Postgres на `localhost:55432` — нужен для DB-зависимых интеграционных тестов (`sqlx::test`).

Деплой — отдельная история (TBD на момент написания).

## Конфигурация

- `config/{api,indexer}.local.yaml` — локальные дефолты, в репо
- `config/{api,indexer}.stage.supabase.yaml` — gitignored, локально для stage-окружения
- `.env.example` — шаблон для секретов; `.env` (рабочий) — gitignored
- Переключение конфига при запуске: `APP_CONFIG=/path/to/file.yaml cargo run -p <service>`
- `auth.kek_hex` (мастер-ключ для шифрования `api_secret` и `pn_seckey`) — из `config/api.<env>.yaml`. В `config/api.local.yaml` shared dev-значение; stage/prod собираются CI из секрет-стора

## Конвенции

- `cargo fmt --check` — формат без расхождений
- `cargo clippy --workspace --all-targets -- -D warnings` — clippy без warnings
- `cargo build --workspace` — компилируется без ошибок
- `cargo test --workspace` — все тесты зелёные после любых изменений (test Postgres поднят для DB-тестов)
- Все четыре проверки обязательны перед коммитом
- Pre-commit doc-sweep (`AGENT_REQUIREMENTS.md:25-31`) — перечитай **каждый** файл под `docs/`, корневой `README.md`, README каждого затронутого компонента (`services/*/README.md`, `crates/*/README.md`); обнови всё, что инвалидируется staged-дифом
- Коммит после каждого принятого шага
- Префикс коммита: `[coder]`, `[tester]`, `[reviewer]` или conventional-стиль (`docs:`, `chore:`, `feat:` и т.п.)
- Промпты: файлы `.md`, строчные латиницей через дефис (`func-<task>-<P>-<role>.md`)
- E2E тесты — реальные внешние сервисы (chain RPC, GraphQL gateway), не моки; HTTP-границы внутри сервисов — `wiremock-rs`
- **Чистая архитектура без костылей.** Никаких полей/таблиц/функций «на всякий случай», никакой дупликации одной информации в нескольких местах, никаких shims/fallbacks/wrappers «для совместимости» со старым кодом. Каждое поле, функция, файл имеет один live use case. Если выглядит как «может пригодится» или «дубль для удобства» — дроп.

### Нейминг

При именовании чего угодно (struct, fn, колонка БД, имя файла, модуль, enum-вариант):

1. **Публичный API** — сверься с [`docs/api-spec.md`](docs/api-spec.md). Имена полей в JSON-ответах, параметры запросов, значения enum (Order Side, Order Type, Time In Force, Order Status, Market Status, Terminal Kind, Cancel Reason) — оттуда. **Менять только с разрешения владельца контракта.**
2. **Domain / Postgres schema** — сверься с [`docs/tech-specs/data-schema.md`](docs/tech-specs/data-schema.md) и [`docs/tech-specs/auth.md`](docs/tech-specs/auth.md). Имена таблиц, колонок, FK, индексов — оттуда. Если домен расширяется — отдельный шаг (миграция + обновление data-schema.md).
3. **On-chain события** — [`docs/contract-specs/dex-events-routing.md`](docs/contract-specs/dex-events-routing.md). Имена событий, аргументы, `dst`-адреса — оттуда.
4. **Implementation idioms** — общие Rust-конвенции: `snake_case` для fn/var/module, `CamelCase` для type/trait/enum, `SCREAMING_SNAKE_CASE` для const/static. Глаголы функций — императив (`reconcile_market`, `assemble_market`, `validate_invariants`).
5. Если нужно новое имя, которого нет ни в одном из документов выше — подозрительно. Спроси владельца прежде чем вводить.
