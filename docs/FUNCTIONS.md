# Функции

Функция liftoff — HTTP-обработчик в виде компонента WebAssembly, который
исполняет сам демон: без Docker, без порта и без процесса на проект.
Environment вида `function` деплоится из CI или прямо `lof deploy` после
локальной сборки. Здесь: мир компонента, проект
функции, сборка и деплой, что функция может в демоне, языки и замеры.

## Мир

Компонент мира `liftoff:function/function` (`wit/function/function.wit`):
стандартный `wasi:http/proxy@0.2.12` плюс

- `wasi:config/store@0.2.0-rc.1` — собранный env environment
  (секреты областей tenant → project → environment и системные `LIFTOFF_PROJECT`, `LIFTOFF_ENV`, `LIFTOFF_DOMAIN`, как у compose; без `IMAGE_TAG`);
- `wasi:keyvalue/{store,atomics,batch}@0.2.0-draft` — таблица `kv`
  environment.

Своего в мире ничего нет, поэтому компонент, собранный под другой хост
этих миров (Spin, Fastly Compute, `wasmtime serve`), работает без
изменений. Экспорт один: `wasi:http/incoming-handler@0.2`.
`deps/config/` и `deps/keyvalue/` — копии WIT крейтов
`wasmtime-wasi-config` и `wasmtime-wasi-keyvalue` того wasmtime, что
линкует демон (49.0.4); остальное в `deps/` — WIT WASI 0.2 из
`wit/deps/`. Шаблоны `templates/fn-*` несут копию этого каталога в своём
`wit/` (обычными файлами: шаблон — отдельный репозиторий);
`scripts/wit-deps.sh --check` (в `just ci`) держит копии равными
`wit/function/deps/`.

## Проект функции

`liftoff.toml` с `[function]` вместо `[service]`:

```toml
[function]
build = "bun install && bun run build"   # что собирает компонент
component = "dist/fn.wasm"               # что получилось, от корня рабочего дерева
```

Быстрее всего начать с шаблона: `lof create hello --template
github.com/unwanation/liftoff-fn-ts` (или `liftoff-fn-rust`,
`liftoff-fn-python`; исходники в `templates/`). `lof
create` функции создаёт проект, environment `prod` вида `function` на
ветке по умолчанию, домен `<name>.<base>` (или `--domain`), привязанный к
`prod` без `service:port` (у функции нет `[service] expose`), deploy-токен и CI-workflow, который на
каждый push выполняет `build` и загружает `component` (раннеру нужен
тулчейн сборки). Адрес отвечает после первого деплоя.

Новый function-environment — `shared` (открыт всем); `lof env unshare`
ставит перед ним вход (ниже «Доступ»). Второй домен функции или домен
environment, добавленного `lof env add <имя> --kind function`,
привязывает `lof bind <host> --env <имя>` — без `service:port`, которых
у функции нет.

Шаблоны отвечают одинаково на всех трёх языках:

| Запрос | Ответ |
|---|---|
| `GET /` | `200`, JSON `{"greeting", "visits"}`: секрет `GREETING` через `wasi:config` (иначе «Hello from <project>») и счётчик `visits` в `wasi:keyvalue` (bucket `""`); прогрев демона не считается |
| `POST /echo` | `200`, тело запроса и его `content-type` |
| всё остальное | `404` |

| Шаблон | Сборка (`[function] build`) | Компонент | Нужно |
|---|---|---|---|
| `fn-rust` | `cargo build --target wasm32-wasip2 --release --locked` (`wit-bindgen` 0.63 над `wit/`) | `target/wasm32-wasip2/release/function.wasm` | Rust 1.98.1 с `wasm32-wasip2` (`rust-toolchain.toml`) |
| `fn-ts` | `bun install && bun run build`: `bun build` в один ESM с внешними `wasi:*`, затем `jco componentize` 1.39.0 (`componentize-js` 0.23.0) | `dist/fn.wasm` | bun |
| `fn-python` | `uv run componentize-py -d wit -w function componentize app -o dist/fn.wasm` (`componentize-py` 0.25.1 в `uv.lock`) | `dist/fn.wasm` | uv |

Версии инструментов те же, что у контрактных функций (ниже); выход
сборки (`target/`, `dist/`, `build/`, `node_modules/`, `.venv/`) в
`.gitignore` шаблона. `[dev] command` шаблонов собирает компонент и
запускает `wasmtime serve -Scli -Sconfig -Skeyvalue` на `127.0.0.1:$PORT`
(нужен CLI wasmtime) без значений `wasi:config`: `wasmtime serve` берёт их
только из аргументов (`-Sconfig-var`), а секрет в argv виден в `ps`, поэтому
локально `GET /` отвечает приветствием по умолчанию; хранилище
`wasi:keyvalue` у `wasmtime serve` в памяти и пустое на каждый запрос, поэтому `visits` локально всегда
`1`. Как `lof dev` запускает команду и открывает туннель — `docs/DEV.md`.

## Сборка и деплой

`lof deploy` в каталоге проекта с `[function]` (`crates/liftoff/src/commands/functions.rs`):

1. `[function] build` выполняется как `sh -c <строка>` в корне рабочего
   дерева. Строка — из манифеста самого человека, никогда с сервера
   (второй и последний shell CLI после `[dev] command`, `docs/ARCHITECTURE.md`
   «Исключения»). stdin закрыт, stdout и stderr сборки идут в stderr CLI
   (stdout принадлежит CLI), секретов сборка не получает. Сборка — лидер
   своей группы процессов: `Ctrl-C`, `SIGTERM`, `SIGHUP` завершают всю
   группу. Упавшая сборка — exit 1, ничего не загружено.
2. `[function] component` проверяется той же функцией, что первая
   проверка демона (`liftoff_wasm::functions::check`): не больше 50 МиБ,
   компонент (не core-модуль), экспорт `wasi:http/incoming-handler@0.2`,
   импорты только из `wasi:`. Отказ — «… is not a wasi:http component:
   <причина>; nothing was uploaded», exit 1.
3. Компонент уходит полем `component` формы `multipart/form-data` в
   `POST /api/v1/deploys` (прогресс в stderr), CLI ждёт конца деплоя
   (`--wait` у функции всегда). Из CI то же с deploy-токеном
   (`LIFTOFF_TOKEN`): деплой идёт во все function-environment ветки
   `ref`, compose-environment той же ветки его не получают.

Демон проверяет компонент до записи и до создания деплоя, хранит файл
по хешу (`paths.data/functions/<hash>.wasm`), компилирует его заранее
(Cranelift, кэш по хешу и версии wasmtime в `paths.data/cache`), затем
активирует: один инстанс с `wasi:config` environment отвечает на `GET /`
с заголовком `X-Liftoff-Warmup`, и только после этого маршрут
переключается на новый компонент. Упавший прогрев — деплой `failed`,
маршрут и текущий деплой не меняются. `tag` деплоя функции —
`sha256-<первые 12 hex хеша>`; история — последние 20 деплоев, файлы,
на которые она не ссылается, удаляются; `lof redeploy --to N`
возвращает прежний компонент. Значения `wasi:config` берутся при
активации: изменённый секрет функция видит после следующего деплоя или
`redeploy`. Активные компоненты живут в памяти демона: после рестарта
он активирует текущий деплой каждого function-environment заново, до
этого адрес отвечает `503`.

## Исполнение

Функцию вызывает function-listener демона (`[functions] bind`, только
loopback): Caddy отдаёт ему домены `running` function-environment с
`Host` без изменений. `stopped`, `failed`, `empty` — страница
`503`. `lof env stop|start` меняют только состояние и маршрут.

| Ключ `[functions]` | По умолчанию | Что ограничивает |
|---|---|---|
| `bind` | `127.0.0.1:7003` | адрес listener'а (только loopback) |
| `max_memory_mb` | `128` | сумму линейных памятей одного инстанса; рост сверх — trap, `500` |
| `timeout` | `30s` | один запрос от прихода: ожидание инстанса, создание нового и обработку (epoch interruption, `504`); тот же срок у исходящего `wasi:http` и у чтения тела запроса (`408`) |
| `instances_per_env` | `4` | инстансы environment, которые обслуживают запросы одновременно; следующий запрос ждёт в пределах `timeout` |
| `kv_max_mb` | `64` | ключи и значения `wasi:keyvalue` environment вместе |

Сверх них, без ключа конфига:

- тело запроса и тело ответа — не больше 64 МиБ (`413` и trap `500`);
  listener одновременно держит не больше 64 запросов, следующий ждёт не
  дольше `timeout`, затем `503`;
- инстанс создаёт не больше 32 core-инстансов, 32 таблиц (до 100 000
  элементов) и 4 памятей, как модуль;
- нет файловой системы, сокетов, переменных окружения и `store-query`;
  исходящий HTTP только через `wasi:http/outgoing-handler`, без
  allow-list хостов, но loopback, частные, link-local и unspecified
  адреса отказываются (на loopback слушают REST демона и admin API
  Caddy);
- функция, вернувшаяся без ответа, — trap (`500`); причина trap и
  сбоя хоста уходит в лог демона, вызывающему — короткий `text/plain`.

Функция не видит того, что принадлежит liftoff: заголовков `X-Liftoff-*`
клиента, cookie `__Host-liftoff*`, `Authorization` с любым токеном
liftoff, hop-by-hop заголовков и `X-Liftoff-Warmup` клиента. Пропущенный
человек (была сессия) приходит в `X-Liftoff-User` и `X-Liftoff-Email`.
Пути `/.liftoff/*` на адресе функции принадлежат liftoff (передача
сессии входа, `404` на прочие) и до функции не доходят.

stdout и stderr функции — её лог: последние 1000 строк environment в
памяти демона (строка до 16 КиБ), `lof logs [--tail N] [-f]` без имени
сервиса; рестарт демона их забывает.

### `wasi:keyvalue`

- У environment один bucket: `open("")` или `open("default")`; другое
  имя — `no-such-store`. Шаблоны берут `""`, который понимает и
  `wasmtime serve`.
- `atomics.increment` хранит счётчик десятичным текстом, как сам
  wasmtime; `list-keys` не поддержан (`other`); ключ длиннее 256 байт —
  `other`.
- Запись сверх `kv_max_mb` — ошибка `other`, которую получает функция,
  а не ошибка запроса; `set-many` сверх лимита не пишет ничего.
- Записи прогрева остаются (контрактные функции и шаблоны прогрев не
  считают).
- Environments не видят ключей друг друга.

### Доступ

Модель та же, что у туннеля (`docs/DEV.md`, «Приватность»): `shared`
пускает всех; `private` (после `lof env unshare`) пускает админа (у
функции нет владельца-пользователя, он есть только у туннеля), с модулем
`tenants` — ещё тех, кого пускает хук `gate`. Сессия — cookie
`__Host-liftoff_gate` этого хоста (после входа браузера) или
`cli`-сессия в `Authorization: Bearer`. Браузер без сессии получает `302`
на вход, JSON-клиент (`Accept: application/json`) — `401`, сессия того,
кому нельзя, — `403`. Доступ проверяется до чтения тела. Без модуля
`oidc` браузер входит одноразовой ссылкой: для функции её выдаёт `POST
/api/v1/auth/links {"target": "environment:<id>"}` (своей команды CLI у
этого нет). Защищённый (`tenants`, `protected`) домен функции сначала
проходит гейт (`forward_auth`), как compose.

## Языки

| | Как | Работает | Не работает |
|---|---|---|---|
| Rust, Go | нативная цель `wasm32-wasip2` | почти всё, кроме сокетов и процессов | нативные библиотеки |
| JS, TS | `componentize-js` (StarlingMonkey) через `jco componentize` | явный экспорт `incomingHandler` над `wasi:http/types`, исходящие запросы через `wasi:http/outgoing-handler`, Web-API StarlingMonkey (`Request`, streams, `crypto.subtle`), чистые npm-пакеты | стиль `addEventListener("fetch", …)` и fetch-фреймворки на нём (Hono) с 0.23.0 не собираются (ниже); глобальный `fetch()` не проверен; Node API (`fs`, `net`), нативные аддоны; нет JIT |
| Python | `componentize-py` (CPython) | чистый Python | пакеты с C-расширениями (`numpy`, `pydantic-core`, `psycopg`), ASGI-фреймворки без адаптера |

Проверено с версиями репозитория (wasmtime 49.0.4 на хосте); шаблоны
собираются теми же версиями:

- **TS**: `@bytecodealliance/componentize-js` 0.23.0 через
  `@bytecodealliance/jco` 1.39.0 (`jco componentize`) под bun 1.4.2;
  исходник сначала собирается `bun build` в один ESM-файл с внешними
  `wasi:*`. Собственный CLI `componentize-js` 0.23.0 не находит каталог
  WIT (`ComponentError: reading file …/wit/function … No such file or
  directory (os error 44)`), поэтому сборка идёт через `jco`. Обработчик
  экспортируется явно как `incomingHandler` (`handle(request,
  responseOut)` над `wasi:http/types@0.2.12`): стиль
  `addEventListener("fetch", …)` с миром `wasi:http/proxy@0.2.12` не
  собирается (`failed to find export of interface
  wasi:http/incoming-handler@0.2.12 function handle`).
- **Python**: `componentize-py` 0.25.1 (`uvx`, uv 0.12.17) собирает мир
  прямо из `wit/function` (симлинки `deps/` он читает); обработчик —
  класс `IncomingHandler(exports.IncomingHandler)` модуля приложения.
- Версия `wasi:http@0.2.12` выхода обоих инструментов совпадает с тем,
  что принимает wasmtime 49: компоненты компилируются и инстанцируются
  без адаптеров.
- `OutputStream.blocking-write-and-flush` принимает не больше 4096 байт
  за вызов: тело ответа пишется кусками (так делают и шаблоны).
- Детекция стека предлагает для TS-функции сборку как в
  `fn-ts`: скрипт `build` из `package.json` после установки (`bun install
  && bun run build`, у pnpm, yarn и npm — их `install` и `run build`); без
  скрипта под bun — `bun build` точки входа (`src/index.ts` или
  `src/index.js`) и `bunx @bytecodealliance/jco componentize` над `wit/` и
  миром из `wit/world.wit`; без скрипта и без bun сборщика нет, и `lof
  create` спрашивает команду сборки.

## Контрактные функции

`modules/testing/functions/{rust,ts,python}/` — одна и та же функция на
трёх языках (`fn_rust`, `fn_ts`, `fn_python`):

| Запрос | Ответ |
|---|---|
| `GET /` | `200`, `application/json`: `{"method", "path", "greeting", "counter", "fetch"}` |
| `POST /echo` | `200`, тело запроса и его `content-type` |
| всё остальное | `404` |

`greeting` — значение `wasi:config` `GREETING` (`null` без него),
`counter` — `wasi:keyvalue/atomics.increment` ключа `counter` в bucket
`default`, `fetch` — тело `GET` по значению `wasi:config` `FETCH_URL`
через `wasi:http/outgoing-handler` (`null` без значения; при ошибке
`"error: <Case>"` — случай `wasi:http/types.error-code` в UpperCamelCase,
одинаково на всех трёх языках, например `"error: ConnectionRefused"`). Прогрев демона (`X-Liftoff-Warmup`) не считается и ничего
не запрашивает. `just functions` собирает все три в `WASM_MODULES_DIR`
(выход сборки вне git); один набор тестов
`crates/wasm/tests/contract_functions.rs` (`FNC-01…06`) гоняет все три
через `FunctionRuntime`.

## Размер и холодный старт

Замер на машине разработки (AMD Ryzen 9 5950X, 6 vCPU,
release-сборка `liftoff-wasm`, лимиты `[functions]` по умолчанию; `GET /`
без `GREETING` и `FETCH_URL`). Компиляция — `FunctionRuntime::precompile`
(Cranelift) без кэша и с кэшем компиляции wasmtime (так стартует демон
после перезапуска); активация — `activate` с прогревом; запрос — `invoke`
первого запроса после активации и медиана из 20.

| | Размер | Компиляция | Из кэша | Активация | Первый запрос | Медиана |
|---|---|---|---|---|---|---|
| `fn_rust` | 124 018 Б (121 КиБ) | 0,15–0,17 с | 2 мс | 0,3–0,4 мс | 30–40 мкс | 8–15 мкс |
| `fn_ts` | 13 219 333 Б (12,6 МиБ) | 12,8–14,0 с | 91 мс | 8–11 мс | 0,3–0,5 мс | 0,24–0,42 мс |
| `fn_python` | 18 532 705 Б (17,7 МиБ) | 11,2–12,2 с | 116 мс | 12–14 мс | 0,2–0,4 мс | 0,13–0,19 мс |

Компиляция — разовая цена деплоя (и первого запуска без кэша); запросы
идут к уже инстанцированным экземплярам пула. Оба компонента TS и Python
укладываются в лимит 50 МиБ и в 128 МиБ памяти по умолчанию.
