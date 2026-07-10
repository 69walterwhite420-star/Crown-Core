# Crown Core — План сборки

Единственный источник **стадии проекта**. В начале каждой сессии Claude Code читает этот файл и определяет текущий этап — первый не-`DONE`.

Шесть этапов. Больше нет, потому что ядро — сплиттер и индексатор.

## Статус

| Этап | Название | Статус |
|---|---|---|
| S0 | Скелет: два крейта, две границы, CI | DONE |
| S1 | `reduce` — чистая свёртка | DONE |
| S2 | Сплиттер на Solana (devnet) | DONE |
| S3 | `crown-index` + Solana source | DONE |
| S4 | Сплиттер на EVM + EVM source (Sepolia) | DONE |
| S5 | Заморозка и mainnet | DEFERRED |

Статусы: `TODO` · `IN-PROGRESS` · `DONE` · `DEFERRED`.

**Решение от 2026-07-10:** ядро функционально завершено в объёме S0–S4 и работает на тестовых сетях (Solana devnet + Ethereum Sepolia) с локальной канистрой. S5 (mainnet-деплой, reproducible build, blackhole, бюджет циклов) отложен до решения о выходе в прод; его DoD не менялся. Следующая работа — пост-корные продукты (игра с эскроу-донатом, затем фабрика) — идёт в отдельных репозиториях по правилам `post-core.md`.

**Дополнение от 2026-07-10:** реализован шаг 6 карты проекта — единственное запланированное изменение ядра: `factories[]` в конфиге и атрибуция эскроу-сеттлментов донору через внешний крейт `crown-derive` (core-spec §4). `REDUCE_VERSION` не менялся, `.did` не менялся, e2e против обеих тестовых сетей зелёный (`scripts/e2e-attribution.sh`).

Всё, что не в таблице, — в `post-core.md`. Не строим. Не готовим место. Не оставляем хуков.

## Принцип

Снизу вверх. Один этап = один связный проверяемый кусок. Не переходи дальше, пока DoD не выполнен и тесты зелёные. Меньше кода — меньше ошибок: у каждой строки есть текущий потребитель.

---

### S0 — Скелет: два крейта, две границы, CI

**Выход.** Workspace: `reduce/` и `index/`. Anchor-воркспейс в `contracts/solana/`. `config/testnet.toml`, `config/mainnet.toml`. CI: fmt, clippy, test, и три структурных линта ниже.

**Структурные линты (это и есть суть этапа).**
1. `cargo tree -p crown-reduce --edges normal` печатает **только сам крейт**. Ноль зависимостей.
2. `grep -rE 'ic_cdk|std::(fs|net|time)|reqwest' reduce/src/` → пусто.
3. Mainnet-профиль конфига не содержит `Custom` источников.

**DoD.** Дерево совпадает с картой в CLAUDE.md. Три линта зелёные на пустом репозитории. Ни одного сетевого значения в коде.

---

### S1 — `reduce` — чистая свёртка

**Вход.** core-spec §2.

**Выход.** Крейт `crown-reduce`, zero deps. В нём: `ChainId`, `Address`, `Settled`, `Book`, `reduce`, `REDUCE_VERSION`.

Заголовок `lib.rs`:
```rust
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic,
        clippy::arithmetic_side_effects, clippy::indexing_slicing)]
```

Вся арифметика через `checked_add`. Переполнение — `Err`, не паника.

**DoD.** Property-тесты (`proptest`, dev-dependency) доказывают:
- **монотонность** — после любого `reduce` значение не убывает;
- **аддитивность** — `fold` над любой перестановкой сеттлментов даёт побитово ту же книгу;
- **пересчёт** — `book[(c,p,s)] == Σ gross`;
- **изоляция ключа** — сеттлмент пары `(c,p,s)` не трогает никакую другую пару;
- **детерминизм** — два прогона на одном входе дают одинаковый результат.

Плюс: `REDUCE_VERSION` определён и покрыт тестом на неизменность при пересборке.

---

### S2 — Сплиттер на Solana (devnet)

**Вход.** core-spec §3.

**Выход.** Anchor-программа. Одна инструкция `donate(gross)`. Два CPI `transfer_checked` из ATA донора: в ATA стримера и в ATA казны. Событие через event-CPI (не `msg!`). `FEE_BPS` и казна — константы программы. Деплой на devnet, затем `set-upgrade-authority --final`.

**DoD.**
- `out == in`: `payout + fee == gross`, тест на границах и на фаззе.
- `require(fee > 0)` — микро-донат ниже флора ревертится.
- **Ноль баланса:** тест проверяет, что у программы и её PDA нет token account, способного держать USDC. Не «баланс равен нулю после», а «аккаунта не существует».
- **Плательщик структурен:** попытка указать чужой ATA как источник без его подписи проваливается.
- `solana program show <id>` печатает `Authority: none`.
- Ни один сетевой адрес не захардкожен в клиентском коде; всё из `config/`.

---

### S3 — `crown-index` + Solana source

**Вход.** core-spec §4, §5, §6, §7.

**Выход.** Канистра. Таймер ингеста: `getSignaturesForAddress(splitter, until = cursor, commitment = finalized)` → пагинация через `before` → `getTransaction` → построение `Settled` → `reduce`. Курсор в стабильной памяти. Книга — `StableBTreeMap`. Certified data: меркл-корень книги в `set_certified_data`, `data_certificate()` в query.

**Проверить и записать фактом в core-spec §5:** отдаёт ли SOL RPC `meta.innerInstructions` и `meta.pre/postTokenBalances` при `encoding = base64`.

**DoD.**
- **`.did` содержит только `query`.** Скрипт CI парсит Candid и падает при виде update-метода.
- E2e на devnet: донат → таймер → `get_reputation` показывает `+gross` паре `(solana-devnet, payer, streamer)`.
- **Exactly-once:** принудительный повторный прогон ингеста с того же курсора не меняет книгу.
- **Признание:** `Settled`-двойник, эмитнутый другой программой с идентичной сигнатурой события, **не** засчитан.
- **Перекрёстная сверка:** транзакция, где событие говорит `gross = X`, а дельты token-balance говорят `Y ≠ X`, отвергается и инкрементит счётчик аномалий.
- Финальность: `commitment = finalized`, ни одного `confirmed`.
- Сертификат из `get_certificate()` проверяется офчейн против root key NNS — есть тест.

---

### S4 — Сплиттер на EVM + EVM source (Sepolia)

**Вход.** core-spec §3, §8.

**Выход.** Solidity-контракт: та же семантика, `transferFrom(msg.sender → streamer)` и `transferFrom(msg.sender → treasury)`, `emit Settled`. `FEE_BPS`, `TREASURY`, `USDC` — `immutable`. Поддержка `permit` (EIP-2612), чтобы убрать отдельный `approve`. Никаких owner/proxy/`delegatecall`/`selfdestruct`.

`EvmSource`: `eth_getLogs { address, topics, fromBlock: cursor+1, toBlock: Finalized }` через EVM RPC канистру, `RpcServices::EthSepolia`.

**DoD.**
- Тот же донат проходит на Ethereum Sepolia, книга показывает `+gross` паре `(eth-sepolia, payer, streamer)`.
- `balanceOf(splitter) == 0` инвариантно, проверено фаззом.
- Контракт верифицирован в эксплорере; байткод без `SELFDESTRUCT` и `DELEGATECALL` (проверка на опкодах в CI).
- **`git diff` показывает, что `reduce/` не тронут ни одной строкой.** Это и есть настоящий тест абстракции.
- Добавлены только: контракт, `source/evm.rs`, профиль конфига.

---

### S5 — Заморозка и mainnet

**Вход.** core-spec §9, §10.

**Выход.**
- Reproducible build индексатора, хэш wasm опубликован.
- Mainnet-профиль: Solana Mainnet + Base + Arbitrum + Optimism (все `Default`). Линт «`Custom` в mainnet» проходит.
- Деплой сплиттеров, `set-upgrade-authority --final` / верификация immutability на EVM.
- Blackhole индексатора: контроллер снят.
- Бюджет циклов задокументирован: outcalls ингеста × число сетей + стоимость permissionless-query. Механизм пополнения (USDC → cycles) описан и протестирован **до** снятия контроллера.

**DoD.**
- Один свап профиля переводит на mainnet-цели без изменения кода.
- Хэш собранного wasm воспроизводится третьим лицом.
- `dfx canister info` показывает отсутствие контроллеров.
- Пополнение циклов работает на заблэкхоленной канистре — проверено на mainnet **до** финального снятия контроллера. Обратного пути нет; порядок операций критичен.
