# Account & Authentication — Design Spec

**Ветка:** `feature/account-auth`
**База:** `feature/node-3198-fetch-order-book-api`
**Статус:** Draft — на ревью перед реализацией

Документ описывает первый слой инфраструктуры под `USER_DATA` / `TRADE` ручки
из `docs/api-spec.md`: модель аккаунта, custody-хранилище, HMAC-аутентификация,
provisioning. Цель — закрыть всё, что разделяет несколько последующих ручек
(`/account`, `/openOrders`, `/allOrders`, `/order`, `/batchOrders`,
`/openOrders DELETE`), чтобы каждая из них была маленьким additive PR'ом
поверх этой основы.

## 1. Контекст и мотивация

### 1.1. Откуда взялась задача

`docs/api-spec.md` описывает три уровня security: `NONE`, `USER_DATA`, `TRADE`,
все приватные ручки используют Binance-style HMAC-подпись поверх
`X-DODEX-APIKEY`. Сейчас в проекте нет ни модели аккаунта, ни таблицы ключей,
ни middleware. Все приватные ручки (`/account`, все `/order*`, `/openOrders`,
`/allOrders`) от этой основы зависят.

Custody-модель прямо в спеке не прописана, но подразумевается формой контракта:
секрет "issued by the Dodex backend", серверная валидация баланса, никаких
полей под клиентскую on-chain подпись. Минимальные правки в публичной
спеке зафиксируем описательной секцией (см. §11).

### 1.2. Почему именно три таблицы (`accounts` / `private_notes` / `api_keys`)

Альтернативы, которые мы рассмотрели и отвергли:

| Альтернатива | Почему не подходит |
|---|---|
| `Account === PrivateNote` (1:1) | Спека `/account.balances[]` показывает NACKL и USDC одновременно под одним `accountId` — невозможно при 1:1, т.к. PN мультиассетная (см. §4). |
| Inline PN-поля в `accounts` | При rotation trading PN либо UPDATE на месте (теряем audit), либо новая строка `accounts` (ломаем "accountId стабилен"). Концептуально PN — не свойство аккаунта, а связанная сущность. |
| `account_pns` через `(account_id, token_type)` | Излишне: trading PN одна и держит все токены сразу через `init_transfer`. Усложняет routing без бенефита. |
| Inline `api_key`-поля в `accounts` | Невозможно иметь несколько ключей с разными permissions на одного юзера (типичный кейс — bot read-write + dashboard read-only). |

Выбранная схема даёт:
- **Стабильный `accountId`** через любые ротации ключей и trading PN.
- **PN как first-class сущность**: `TRADING` / `ARCHIVED` роли, чистая
  атомарная ротация (`INSERT new TRADING + UPDATE old → ARCHIVED`),
  сохраняемый audit-trail.
- **N api_keys с независимыми permissions** на один account.
- **Custody-разделение**: backend хранит только TRADING/ARCHIVED PN, про
  юзерские deposit/withdraw PN не знает — clean boundary.

### 1.3. Что эта ветка разблокирует

Каждая последующая ветка становится маленьким additive-PR'ом поверх этого
фундамента — без него любая из них требовала бы тащить весь auth-стек:

| Ветка | Что использует из этой |
|---|---|
| `feature/get-account` | `AuthContext.trading_pn` → 1 вызов `bee_dex.get_private_note_details` + 1 вызов `get_stakes`, всё. |
| `feature/owner-tracking-in-live-orders` | Ничего, может идти параллельно. Но `pn_dih_dec` в `private_notes` уже есть, бэкфилл потом не нужен. |
| `feature/get-open-orders` | `AuthContext.trading_pn.pn_dih_dec` для фильтра `live_orders WHERE owner_dih = $1`. |
| `feature/get-all-orders` | То же. |
| `feature/post-order` | `AuthContext.trading_pn` (pn_address + pn_seckey decrypted) → `bee_dex.place_order`. Permission check на `Trade`. |
| `feature/delete-order` / `feature/batch-orders` | То же что post-order. |

В каждой из них API-layer кода — десятки строк, потому что вся проверка
авторизации, расшифровка ключей, лукап trading PN, custody storage —
уже сделаны и закрыты тестами здесь.

### 1.4. Что эта ветка НЕ должна решить

- **Хранение балансов** — балансы лежат on-chain в PN (см. §4), мы их
  не кешируем и в БД не пишем.
- **Идентификация владельца ордера в нашей read-model** — это про колонку
  `live_orders.owner_dih`, решается в `feature/owner-tracking-in-live-orders`.
- **Provisioning UI** — только CLI, всё остальное снаружи.
- **KEK rotation** — `key_version` колонка заложена; процесс ротации
  оформляется отдельным runbook'ом.

## 2. Скоуп ветки

### 2.1. Что входит

- Миграция `0013_accounts_and_api_keys.sql` с тремя таблицами: `accounts`,
  `private_notes`, `api_keys`.
- Модуль `crypto.rs` для envelope-шифрования секретов под master KEK.
- Модуль `auth.rs` — Salvo middleware с HMAC-проверкой и permission check.
- Тип `AuthContext` в `crates/application`, прокидывается через Salvo Depot.
- Три CLI-binary в `crates/infrastructure/src/bin/`: `create_account`,
  `attach_trading_pn`, `create_api_key`. Плюс минимальные `disable_api_key`,
  `list_accounts`, `list_api_keys` для эксплуатации.
- Расширение `ApiConfig` секцией `auth`.
- Smoke-эндпоинт `GET /api/v1/_authcheck` (USER_DATA) для интеграторов
  и smoke-теста middleware.
- Изменения в `docs/api-spec.md` (новая секция `Account Model` и уточнение
  про `canonicalQueryString`).

### 2.2. Что вне скоупа

- Сами публичные ручки `/account`, `/order*`, `/openOrders`, `/allOrders` —
  каждая отдельным PR'ом.
- Любая интеграция с `bee-dex` / on-chain submit — приходит в ветку
  `feature/post-order`.
- Owner-tracking в `live_orders` (колонки `owner_dih`, `flags`, `orig_amount`,
  заполнение `client_order_id` в projector'е) — отдельная ветка
  `feature/owner-tracking-in-live-orders`.
- Пополнение/вывод средств юзера — это user-side flow между его PN
  (deposit/withdraw) и нашей trading PN через `init_transfer`, backend в нём
  не участвует.
- Provisioning UI/HTTP-API — только CLI в этой ветке.
- KEK rotation — `key_version` колонка заложена, сам процесс rotation —
  отдельная операционная задача.

## 3. Модель аккаунта

### 3.1. Концепция

- **Account** — логическая сущность, идентифицируется UUID (`accountId`
  в публичном API). Не знает про chain. Стабилен через rotation API-ключей
  и торговой PN.
- **PrivateNote (PN)** — chain-сущность под кастоди backend'а.
  PN мультиассетная (может держать NACKL + USDC + SHELL + outcome-токены
  одновременно), `tokenType` фиксируется только при deploy, далее
  пополняется любыми трансферами.
- **Trading PN** — одна PN на аккаунт, помеченная `role = 'TRADING'`. Через
  неё backend отправляет `placeOrder` / `cancelOrder` от имени аккаунта.
- **Archived PN** — бывшая trading PN после rotation. Хранится с seckey
  для возможного refund/withdraw/audit. Не используется для активной
  торговли.
- **External PN** (deposit/withdraw ноты пользователя) — backend о них ничего
  не знает, не хранит и не оперирует.
- **API key** — credential (`api_key` + `api_secret`), выдаётся под аккаунт.
  N штук на один account_id, у каждой свой набор permissions
  (`USER_DATA`, `TRADE`).

### 3.2. Доказательство мультиассетности trading PN

`bee_dex/README.md` говорит "One PN = one token type" — это про deploy,
не про runtime. Тесты `bee_dex/tests/integration/pn_basic.rs`
показывают `init_transfer` с произвольным `token_type` между PN.
`PrivateNote.sol` хранит `_balance[tokenType]` как mapping. Соответственно
одна PN агрегирует балансы всех токенов, которые в неё пришли через
`init_transfer` / `splitFullSet` / `claim`.

`/api/v1/account.balances[]` (пример в спеке содержит NACKL и USDC под одним
`accountId`) ровно эту мультиассетность и отражает: один лукап
`dex.get_private_note_details(account.trading_pn.address)` даёт все балансы.

### 3.3. Связи

```
account (UUID)
   │
   ├── private_notes (N: 1 TRADING + 0..* ARCHIVED)
   │        ↑
   │        │ pn_dih_dec → используется как owner ID в OrderBook
   │        │ (см. getOrdersByOwner — owner identity на chain это dih, не address)
   │
   └── api_keys (N штук, с разными permissions)
```

## 4. Модель балансов (где и как хранятся)

**Балансы НЕ хранятся в нашей БД.** Source of truth — chain-state PrivateNote
контракта. Backend читает балансы on-demand при обслуживании `/account`.
Это даёт два жирных бенефита:
- Нет проблемы консистентности между нашей read-model и chain.
- Нет нужды проектить N разных PN-событий (`TransferReceived`,
  `StakeConfirmed`, `ClaimAccepted`, `SplitProcessed`, `MergeProcessed`,
  плюс OB callbacks) в read-model — это полтора десятка проекторов,
  которые мы бы поддерживали и тестировали впустую.

Цена решения — один-два дополнительных on-chain getter call на каждый
`/account` запрос. На фоне любого `place_order` (десятки секунд waiting
for event) это пренебрежимо.

### 4.1. Что в PN контракте (`contracts/PrivateNote.sol`)

| Storage | Тип | Что |
|---|---|---|
| `_balance[tokenType]` | `mapping(uint32 => uint128)` | Свободный fungible-баланс по token type. NACKL/USDC/SHELL — всё лежит здесь. |
| `_lockedInOrders[tokenType]` | `mapping(uint32 => uint128)` | Collateral, залоченный в открытых BUY-ордерах (sell-ордера локают outcome-токены, не collateral). |
| `_stakes[hash]` | `mapping(uint256 => StakeInfo)` | Стейки по хешу `tvm.hash(eventId, oracleListHash, tokenType)`. Внутри `StakeInfo.amount[outcomeId]` — свободный баланс outcome-токенов по конкретной позиции. |
| `_orderLocks[ob][orderId]` | `mapping(address => mapping(uint128 => uint128))` | Per-order fee reserve. Не баланс в смысле спеки. |
| `_coupons_value` | `uint128` | Купонная стоимость. Не входит в `/account.balances`. |

### 4.2. Что доступно через bee-dex

| Bee-dex метод | Возвращает | Что из этого нам нужно для `/account` |
|---|---|---|
| `dex.get_private_note_details(pn_address)` → `PrivateNoteDetails` | `balance: HashMap<tokenType, u128>`, `coupons_value`, `has_withdrawn`, `busy_address`, etc. | `balance` map → `balances[].free` (с дополнительным шкалированием по token decimals). |
| `dex.get_stakes(pn_address)` → `HashMap<stakeHash, StakeInfo>` | Сырой JSON `StakeInfo` со включённым `amount: u128[]`, `oracleListHash`, `tokenType` | `amount[outcomeId]` → `outcome_balances[].free` (после маппинга stake → market). |
| `dex.get_aggregated_balance(&[dih])` | Та же `balance` map агрегированная по нескольким PN | Не нужно — у нас одна trading PN на аккаунт. |

### 4.3. Где взять `locked`

Тут ключевая тонкость: **`_lockedInOrders` mapping публичным getter'ом
не экспозится**. Из bee-dex его не достать. Варианты:

- **Считать в SQL из `live_orders`** (рекомендую). После того как
  `feature/owner-tracking-in-live-orders` добавит `owner_dih` колонку,
  `balances[].locked` за token type N:
  ```sql
  SELECT SUM(amount_remaining * price)::numeric / 10^(scale)
    FROM live_orders lo
    JOIN market_outcomes mo ON ...
    JOIN markets m ON m.orderbook_address = lo.orderbook_address
   WHERE lo.owner_dih = $1
     AND lo.is_buy = true
     AND lo.status = 'OPEN'
     AND m.token_type = $2
  ```
  Это согласуется с on-chain `_lockedInOrders` because contract тоже
  накапливает по тому же закону: lock на placement, release на
  fill/cancel.
- **Альтернатива**: добавить getter `getLockedInOrders(uint32 tokenType)`
  в PN контракт. Cleaner separation of concerns, но это уже patch контракта —
  по нашим компетенциям сейчас не делаем.

Для `outcome_balances[].lockedInOrders` (sell-ордера, локают outcome-токены):
такой же SQL с `is_buy = false`, GROUP BY `(orderbook_address, outcome_id)`.

### 4.4. Маппинг stake → market

`get_stakes` возвращает map по `stakeHash = tvm.hash(eventId, oracleListHash,
tokenType)`. Чтобы отдать `outcome_balances[].marketAddress` и `.symbol`,
нужно сопоставить stake hash с рыночными метаданными. Два варианта:

- Считать stake hash off-chain для каждого нашего маркета (есть
  `event_id`, `token_type` в `markets`; `oracle_list_hash` уже в схеме —
  надо убедиться что indexer его пишет, иначе доп. задача), сматчить
  с ключами `get_stakes`.
- Достать `oracleListHash` из JSON `StakeInfo` (он там есть как поле),
  плюс token_type, и матчить через `(oracleListHash, tokenType) →
  markets`. **Простой путь, рекомендую.**

Эта реализация — забота ветки `feature/get-account`, не этой. Здесь
важно зафиксировать что инфраструктура (хранение `pn_address` и
`pn_dih_dec` в `private_notes`) этому не мешает.

### 4.5. Резюме контракта на балансы

| Поле `/account` | Источник |
|---|---|
| `balances[].free` (collateral free) | `get_private_note_details().balance[tokenType]` |
| `balances[].locked` | SQL по `live_orders` (after owner-tracking ветки) |
| `outcome_balances[].free` | `get_stakes()[stakeHash].amount[outcomeId]` |
| `outcome_balances[].lockedInOrders` | SQL по `live_orders WHERE is_buy=false` |

Всё что наша часть БД хранит про балансы — **ничего**. В `private_notes`
лежат идентификаторы для adress'ации (pn_address, pn_dih_dec, pn_pubkey,
encrypted seckey), не балансы.

## 5. Схема БД

### 5.1. Миграция `0013_accounts_and_api_keys.sql`

```sql
-- Logical user entity. Stable across api-key rotation and trading-PN rotation.
create table accounts (
    id              uuid        primary key default gen_random_uuid(),
    label           text,                                    -- audit only, не показывается клиенту
    disabled_at     timestamptz,
    created_at      timestamptz not null default now()
);

-- PNs we custody. One active TRADING per account; rotated-out становится ARCHIVED.
create table private_notes (
    id              bigserial   primary key,
    account_id      uuid        not null references accounts(id) on delete cascade,
    pn_address      text        not null unique,
    pn_pubkey_dec   text        not null,                    -- decimal, как в bee-dex
    pn_seckey_enc   bytea       not null,                    -- AES-256-GCM(seckey, KEK)
    pn_dih_dec      text        not null,                    -- deposit_identifier_hash, для OB owner-lookup
    role            text        not null check (role in ('TRADING','ARCHIVED')),
    key_version     int         not null default 1,          -- forward-compat для KEK rotation
    disabled_at     timestamptz,
    created_at      timestamptz not null default now()
);

-- Ровно одна активная TRADING PN на аккаунт.
create unique index private_notes_one_trading_per_account_idx
    on private_notes(account_id)
    where role = 'TRADING' and disabled_at is null;

create index private_notes_account_id_idx on private_notes(account_id);

-- API credentials.
create table api_keys (
    id              bigserial   primary key,
    account_id      uuid        not null references accounts(id) on delete cascade,
    api_key         text        not null unique,
    api_secret_enc  bytea       not null,                    -- AES-256-GCM(secret, KEK)
    permissions     text[]      not null default '{"USER_DATA"}',  -- subset of {USER_DATA,TRADE}
    key_version     int         not null default 1,
    disabled_at     timestamptz,
    last_used_at    timestamptz,                             -- обновляется fire-and-forget
    created_at      timestamptz not null default now()
);

create unique index api_keys_api_key_active_idx
    on api_keys(api_key) where disabled_at is null;
create index api_keys_account_id_idx on api_keys(account_id);
```

### 5.2. Ключевые инварианты

- **Constraint:** `(account_id) where role='TRADING' and disabled_at is null`
  unique — на любом моменте у аккаунта максимум одна активная trading PN.
- **`pn_seckey_enc` всегда NOT NULL** — в этой ветке регистрируются только
  custody PN. Внешние PN (если когда-то понадобятся) — отдельной миграцией
  с nullable seckey и новой ролью.
- **`accounts.id` стабилен** через все ротации. Внутренний `private_notes.id`
  и `api_keys.id` для аудита; наружу UUID не отдаётся, отдаётся только
  `accounts.id`.
- **Каскадный delete** через `on delete cascade` для `private_notes` и
  `api_keys` — упрощает тестирование, но в проде `disabled_at`-флаг
  предпочтительнее физического удаления.

## 6. Криптография

### 6.1. KEK (Key Encryption Key)

- Алгоритм: **AES-256-GCM**, реализация — RustCrypto crate `aes-gcm`.
- KEK — 32 байта, **читается из env-переменной `DODEX_KEK_HEX`** при старте
  процесса. В YAML не кладём. Fail-fast при старте если переменная не
  задана / не валидный hex / не 32 байта.
- Тот же KEK читают и API-сервис, и все CLI-binary.

### 6.2. Envelope-формат в БД

`bytea` колонка хранит:

```
version(1) || nonce(12) || tag(16) || ciphertext(...)
```

- `version` сейчас всегда `1`, соответствует `key_version = 1` строки.
- `nonce` — 96 бит, генерируется через `OsRng` на каждое шифрование.
- `tag` — GCM authentication tag (16 байт).
- `ciphertext` — длиной с plaintext (для seckey: ~32 байта; для api_secret:
  32 байта).

### 6.3. API модуля

```rust
// crates/infrastructure/src/crypto.rs

pub struct Kek([u8; 32]);

impl Kek {
    pub fn from_env(var: &str) -> anyhow::Result<Self> { ... }
}

pub fn seal(kek: &Kek, version: u8, plaintext: &[u8]) -> Vec<u8>;
pub fn open(kek: &Kek, blob: &[u8]) -> anyhow::Result<Vec<u8>>;
```

`open` валидирует версию и GCM tag — любое искажение ciphertext'а / nonce'а /
tag'а отдаёт ошибку, никогда не возвращает мусорный plaintext.

## 7. HMAC-аутентификация

### 7.1. Контракт (как в `docs/api-spec.md`)

| Местоположение | Имя | Тип | Обязательно | Значение |
|---|---|---|---|---|
| Header | `X-DODEX-APIKEY` | STRING | Да | значение `api_keys.api_key` |
| Query | `timestamp` | LONG (ms) | Да | Unix-time в миллисекундах |
| Query | `recvWindow` | LONG (ms) | Нет | Default `5000`, max `60000` |
| Query | `signature` | STRING (hex) | Да | HMAC-SHA256 hex lowercase |

### 7.2. Алгоритм подписи

```text
signature = HMAC_SHA256(canonicalQueryString + canonicalRequestBody, apiSecret)
```

- `canonicalQueryString` — строится так: берётся raw query из URL, разбивается
  по `&`, удаляется пара с ключом `signature`, оставшиеся пары сортируются
  лексикографически по ключу, склеиваются обратно через `&`. **Значения
  не пере-кодируются** — оставляются как пришли по wire.
- `canonicalRequestBody` — байты тела запроса как они пришли. Для запросов
  без тела — пустая строка. Парсинг JSON не выполняется, ключи не
  пересортировываются.

### 7.3. Pipeline middleware (Salvo)

1. Достать `X-DODEX-APIKEY` из headers → если пусто, `-1002`.
2. Достать `signature`, `timestamp`, опц. `recvWindow` из query → если
   чего-то нет, `-1002`.
3. Прочитать raw body байты, положить копию в Depot для downstream-handler'а.
4. Построить `canonicalQueryString` из raw query (см. §7.2).
5. Лукап `api_keys WHERE api_key = $1 AND disabled_at IS NULL` →
   при пустом результате `-1002` без различения "не существует / disabled".
6. Расшифровать `api_secret_enc` под KEK.
7. Проверить временное окно:
   - `recv = min(recvWindow_from_query.unwrap_or(5000), 60000)`
   - `now_ms - timestamp_ms in [-1000 .. recv]` (1000ms tolerance вперёд для
     clock skew, любое более серьёзное отставание в будущем — `-1021`).
   - Иначе `-1021`.
8. Вычислить `expected = HMAC_SHA256(canonicalQuery + body, secret)`.
9. Сравнить `hex(signature)` и `hex(expected)` через `subtle::ConstantTimeEq`.
   Несовпадение → `-1022`.
10. Лукап активной trading PN: `private_notes WHERE account_id = $1 AND
    role = 'TRADING' AND disabled_at IS NULL`.
    - Если строки нет → `-1002` ("account not configured for trading").
      Альтернатива — отдельный код, но из существующих в спеке `-1002`
      ближе всего по смыслу.
11. Расшифровать `pn_seckey_enc`.
12. Положить `AuthContext` в Depot.
13. Запустить fire-and-forget update `last_used_at = now()` через
    `tokio::spawn`.

### 7.4. `AuthContext`

```rust
// crates/application/src/lib.rs

pub struct AuthContext {
    pub account_id: Uuid,
    pub api_key_id: i64,
    pub trading_pn: TradingPn,
    pub permissions: PermissionSet,
}

pub struct TradingPn {
    pub pn_address: String,
    pub pn_pubkey_dec: String,
    pub pn_dih_dec: String,
    pub pn_seckey: SensitiveBytes,        // zeroize on drop
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission { UserData, Trade }

pub struct PermissionSet(BitFlags);

impl PermissionSet {
    pub fn contains(&self, p: Permission) -> bool;
}
```

`SensitiveBytes` — newtype над `Vec<u8>` с `impl Drop` зерующим память
(использует `zeroize` crate).

### 7.5. Permission check на уровне handler'а

```rust
pub fn require(auth: &AuthContext, perm: Permission) -> Result<(), DomainError> {
    if auth.permissions.contains(perm) { Ok(()) } else { Err(DomainError::AuthRequired) }
}
```

`NONE` ручки middleware не задевают. `USER_DATA` ручки требуют валидного
auth (наличие `AuthContext` достаточно). `TRADE` ручки дополнительно
вызывают `require(&auth, Permission::Trade)` в начале handler'а.

## 8. Provisioning (CLI)

Три binary-цели в `crates/infrastructure/src/bin/`. Все читают
`DODEX_KEK_HEX` и `DATABASE_URL` из env.

### 8.1. `create_account`

```sh
DODEX_KEK_HEX=... DATABASE_URL=... create_account --label "user-1234"
# stdout:
# account_id: 7e4c3a90-1f23-4abc-9876-deadbeef0001
```

### 8.2. `attach_trading_pn`

```sh
DODEX_KEK_HEX=... DATABASE_URL=... attach_trading_pn \
    --account-id 7e4c3a90-... \
    --pn-address 0:abc... \
    --pn-seckey-hex 0123...ef \
    --pn-dih-dec 12345...
```

Действия:
- Деривит `pn_pubkey_dec` из `pn_seckey_hex` (ed25519, та же кривая что
  в TVM SDK).
- Шифрует seckey под KEK.
- Транзакционно: если у `account_id` уже есть `role='TRADING' AND
  disabled_at IS NULL` строка — fail. Использовать `rotate_trading_pn`
  (приходит отдельной задачей).
- INSERT `private_notes(role='TRADING')`.

### 8.3. `create_api_key`

```sh
DODEX_KEK_HEX=... DATABASE_URL=... create_api_key \
    --account-id 7e4c3a90-... \
    --permissions trade,user_data
# stdout:
# api_key:    dk_live_e8a4f2c1...     (32 hex символов после префикса)
# api_secret: 9f3c1b2a...              (64 hex символов, показано ОДИН раз)
```

Действия:
- Валидирует что `account_id` существует и не disabled.
- Генерит `api_key` (`dk_live_` + 32 hex) и `api_secret` (32 random bytes hex).
- Шифрует secret под KEK.
- INSERT в `api_keys`.
- Печатает оба значения. Сам secret потом не достать.

### 8.4. Минимально необходимые ops-команды

- `disable_api_key --api-key dk_live_...` → `UPDATE SET disabled_at=now()`.
- `list_accounts` → `id`, `label`, `created_at`, активная trading PN
  (адрес), счётчик активных api_keys. Без секретов.
- `list_api_keys [--account-id ...]` → `account_id`, `api_key (masked,
  показываем только prefix+suffix)`, `permissions`, `last_used_at`,
  `disabled_at`. Без секретов.

### 8.5. Что не делаем в этой ветке

- `rotate_trading_pn` — атомарная замена TRADING на ARCHIVED + новый
  TRADING. Схема позволяет, команда — отдельной задачей.
- `disable_account` — каскадный disable api_keys + PN.
- `enable_*` — реверс disable. Сейчас disable — терминал в рамках MVP.

## 9. Конфиг

Расширение `crates/infrastructure/src/config.rs`. В YAML добавляется:

```yaml
auth:
  default_recv_window_ms: 5000
  max_recv_window_ms: 60000
```

Валидация на старте: `default_recv_window_ms <= max_recv_window_ms <=
60000`. `DODEX_KEK_HEX` читается отдельно из env (см. §6.1), в YAML не
кладётся.

## 10. Зависимости (workspace `Cargo.toml`)

Добавляются:

```toml
aes-gcm  = "0.10"
hmac     = "0.12"
sha2     = "0.10"           # уже может быть транзитивно, явно объявляем
subtle   = "2.6"
hex      = "0.4"             # уже может быть транзитивно
rand     = "0.8"
zeroize  = "1.8"
uuid     = { version = "1", features = ["v4", "serde"] }
ed25519-dalek = "2"          # derive pubkey из seckey для attach_trading_pn
```

`bee-dex` и `ackinacki-kit` в этой ветке **не** добавляются — они придут
в `feature/post-order`.

## 11. Публичная спека — правки в `docs/api-spec.md`

### 11.1. Новая секция `## Account Model`

Вставляется после `## Symbol Model`.

> An account is the trading entity addressed by `accountId` (RFC-4122
> UUID). It owns one custodial trading position which holds balances
> in any number of assets. Deposit and withdrawal flows are outside
> the API: they are performed by the account holder against their
> own deposit notes, which then transfer funds into the trading
> position via the chain's private-transfer mechanism.
>
> - `accountId` is stable across API-key rotation and trading-position
>   rotation.
> - Multiple API keys can be issued under one `accountId`, each with
>   its own permission subset.
> - The API secret is generated by the Dodex backend at API-key
>   creation time and returned exactly once. A lost secret requires
>   creating a new API key.

### 11.2. Уточнение `## Security Types > Signature Formation`

Добавляется параграф после формулы подписи:

> The `canonicalQueryString` is built from the raw query string of
> the URL by removing the `signature` parameter, splitting on `&`,
> sorting the remaining `key=value` pairs lexicographically by key,
> and rejoining with `&`. Values are not re-encoded — they are taken
> exactly as sent on the wire. The `canonicalRequestBody` is the
> request body byte sequence as transmitted; the server does not
> re-serialize JSON or reorder keys.

### 11.3. Что НЕ меняем

- Все таблицы Endpoint Summary / Market Data Endpoints / Trading
  Endpoints — нетронуты.
- Error codes таблица — все три (`-1002`, `-1021`, `-1022`) уже там.

## 12. Internal endpoint `_authcheck`

Не входит в публичную спеку. Используется для smoke-тестов и для
интеграторов на этапе настройки клиента подписи.

```http
GET /api/v1/_authcheck   (USER_DATA)
```

Ответ:

```json
{
  "accountId": "7e4c3a90-1f23-4abc-9876-deadbeef0001",
  "permissions": ["USER_DATA", "TRADE"]
}
```

Любая 401 ошибка ходит через тот же `ApiError`-маппинг с кодами
`-1002 / -1021 / -1022`.

Это удобный "первый запрос" интегратора, чтобы убедиться что HMAC
посчитан правильно до отправки реальных запросов. После релиза
`/account` смысл `_authcheck` уменьшается — оставляем под фичефлагом
или убираем (решим в `feature/get-account`).

## 13. Маппинг ошибок

Все коды уже описаны в `DomainError` и `ApiError`. Маппинг:

| Сценарий | Код | HTTP |
|---|---|---|
| Нет `X-DODEX-APIKEY` | `-1002` | 401 |
| Нет `signature` / `timestamp` | `-1002` | 401 |
| Неизвестный api_key | `-1002` | 401 |
| api_key.disabled_at not null | `-1002` | 401 |
| У аккаунта нет TRADING PN | `-1002` | 401 |
| `recvWindow` exceeded / clock skew | `-1021` | 401 |
| Подпись не совпала | `-1022` | 401 |
| TRADE-ручка с USER_DATA-only ключом | `-1002` | 401 |

`-1002` намеренно перегружен — не leak'аем интегратору причину
(существует ключ / правильные ли permissions / configured ли account).
В логах backend различает каждый случай отдельно (с `tracing` уровнем
`info` / `warn`).

## 14. Файлы

```
migrations/0013_accounts_and_api_keys.sql              [new]

crates/infrastructure/src/crypto.rs                    [new]
crates/infrastructure/src/auth.rs                      [new]
crates/infrastructure/src/lib.rs                       [edit: pub mod]
crates/infrastructure/src/config.rs                    [edit: AuthSection]
crates/infrastructure/Cargo.toml                       [edit: deps]

crates/infrastructure/src/bin/create_account.rs        [new]
crates/infrastructure/src/bin/attach_trading_pn.rs     [new]
crates/infrastructure/src/bin/create_api_key.rs        [new]
crates/infrastructure/src/bin/disable_api_key.rs       [new]
crates/infrastructure/src/bin/list_accounts.rs         [new]
crates/infrastructure/src/bin/list_api_keys.rs         [new]

crates/application/src/lib.rs                          [edit: AuthContext, Permission]
crates/domain/src/lib.rs                               [edit: SensitiveBytes optional]

services/api/src/main.rs                               [edit: hoop(auth), _authcheck route]
services/api/Cargo.toml                                [edit: deps]

config/api.local.yaml                                  [edit: auth section]

Cargo.toml                                             [edit: workspace deps]

crates/infrastructure/tests/auth.rs                    [new]
crates/infrastructure/tests/crypto.rs                  [new]

docs/api-spec.md                                       [edit: Account Model, signature note]
services/api/README.md                                 [edit: про auth и provisioning]
```

Объём — порядка 1.5–2k строк, одна PR.

## 15. План тестов

### 15.1. Unit (`crates/infrastructure/src/auth.rs`, `crypto.rs`)

- `crypto::seal/open` round-trip.
- `crypto::open` отвергает искажённый ciphertext / nonce / tag.
- `crypto::open` отвергает неизвестный `version`.
- `canonical_query_string`:
  - пустой query
  - один параметр
  - три параметра, отсортированы
  - `signature` удалена
  - значения с URL-encoded символами сохраняются как есть
- `recv_window_check`: внутри, на границе, за границей в прошлое,
  допустимый clock skew вперёд (≤1s), серьёзное расхождение вперёд.
- `hmac_compute` против вектора из `docs/api-spec.md §Signature Formation`
  (тот самый пример с `POST /api/v1/order` body).
- `permissions.contains` для каждого варианта enum'а.

### 15.2. Integration против реального Postgres (`crates/infrastructure/tests/auth.rs`)

Гейтятся на `TEST_DATABASE_URL`, та же инфра что в `tests/depth.rs`.

Сценарии (каждый — отдельный `#[tokio::test]`):

- `valid_signature_returns_200` — создаём account+pn+api_key, делаем
  signed запрос на `/_authcheck`, ответ содержит правильный `accountId`.
- `wrong_signature_returns_1022`.
- `missing_apikey_returns_1002`.
- `disabled_api_key_returns_1002`.
- `stale_timestamp_returns_1021`.
- `recv_window_overflow_clamped_to_60000` — recvWindow=999999 принимается
  но эффективно 60000.
- `account_without_trading_pn_returns_1002` — создаём account+api_key,
  не attach'аем PN.
- `user_data_key_on_trade_route_returns_1002` — заведём тестовый
  `/_trade_only` route, проверим что USER_DATA-only ключ его не пройдёт.
- `body_canonicalization_byte_exact` — два запроса с одинаковым parsed
  JSON но разным форматированием → разные подписи, оба должны
  проходить если signature посчитан над теми же байтами что
  отправлены.

### 15.3. End-to-end smoke

`tests/_authcheck.rest` — несколько REST-Client запросов с предзаведённым
test api_key (тестовый KEK + seed данные через CLI в test-fixtures).

## 16. Решения, принятые в ходе обсуждения

1. ✅ Custody-модель — backend хранит pn_seckey зашифрованным под KEK.
2. ✅ `accountId` — голый UUID без префикса.
3. ✅ Отдельная таблица `private_notes` с ролями `TRADING` / `ARCHIVED`.
4. ✅ Одна активная TRADING PN на аккаунт (unique partial index).
5. ✅ `pn_dih_dec` сохраняем сейчас (нужно для будущей owner-tracking
   ветки, не хочется обратной миграции с бэкфиллом).
6. ✅ `accounts.label` — для аудита, остаётся.
7. ✅ KEK через env `DODEX_KEK_HEX`, не через YAML.
8. ✅ Provisioning через CLI-binary, без HTTP admin API в этой ветке.
9. ✅ Smoke-эндпоинт `_authcheck` — да, для интеграторов.

## 17. Открытые вопросы для ревьюера

1. **Tolerance вперёд для clock skew** — заложено 1000ms (§7.3 шаг 7).
   Норм или строже / мягче?
2. **`disable_*` ops** — реверсивные `enable_*` нужны сразу или ок без них?
3. **`last_used_at` update** — fire-and-forget `tokio::spawn`. Альтернатива:
   обновлять только если прошло > N секунд с прошлого update'а (защита
   от write-amplification). Приемлемо как есть для MVP?
4. **`accounts.label`** — храним plain text или должен быть в любом
   виде сенситивный? Я полагаю plain — это для админ-аудита, не
   PII.
5. **Маскинг api_key в `list_api_keys`** — `dk_live_e8a4...0001`?
   Сколько символов в начале/конце оставлять?
6. **Логирование auth-failure** — на каком уровне (`info` / `warn`)?
   `warn` для bad signature, `info` для отсутствия header'а?

## 18. После мерджа этой ветки

Получаем infrastructure-фундамент. Дальше каждая ветка маленькая и additive:

- `feature/owner-tracking-in-live-orders` — расширение `live_orders`
  (можно делать параллельно, не зависит от auth).
- `feature/get-account` — `/api/v1/account` поверх `AuthContext` + один
  `bee_dex.get_private_note_details` call.
- `feature/get-open-orders` — поверх owner-tracking + `AuthContext`.
- `feature/get-all-orders` — поверх owner-tracking + retention решение.
- `feature/post-order` — поверх всего, добавляет `bee-dex` dep и
  `PlaceOrderUseCase`.
- `feature/delete-order` / `feature/batch-orders` — после place-order.
