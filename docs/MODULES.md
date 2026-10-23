# Как написать модуль liftoff

Модуль это WASM-компонент мира `liftoff:module@0.1.0`
(`wit/liftoff-module.wit`), который демон загружает без
рестарта (`lof module install`). Модуль добавляет маршруты REST под
`/api/v1/m/<имя>/`, команды CLI, инструменты MCP, подписки на хуки ядра и
фоновые задачи; хранит данные в своём пространстве ключей и своих таблицах
SQLite. Docker, systemd, файловая система, сокеты и DNS модулю
недоступны; наружу он ходит только через `wasi:http` к хостам из своего
манифеста. Модули недоверенные: всё, что модуль объявляет или
отвечает, хост и ядро проверяют.

Как устроен хост и почему так: `docs/ARCHITECTURE.md`, раздел «Модули
» и его подразделы. Этот документ — рецепт для автора модуля;
примеры в коде: `modules/dns-cloudflare` (таблица и хуки доменов),
`modules/oidc` (публичные маршруты, хранилище ключей, `wasi:http`),
`modules/tenants` (команды, `core-call`, хуки авторизации и данных),
`modules/git-forgejo` (инструменты без актора для порта `Git`),
`modules/git-github` (device flow, sealed box, несколько хостов в
`http_allow`), `modules/git-gitlab` (`http_allow` и `git_provider.hosts`
из конфига), `modules/mcp` и `modules/web` (только конфиг и хуки данных, без маршрутов и таблиц).

## 1. Где живёт модуль

`modules/` — отдельный Cargo workspace (корневой его не включает), по
крейту на модуль плюс контрактные компоненты хоста в `modules/testing/*`.
Всё собирается под цель `wasm32-wasip2` из `rust-toolchain.toml`:
нативная цель rustc сама выдаёт компонент, биндинги генерирует
`wit-bindgen` 0.63. `cargo-component` не используется (не поддерживается с
2025-03).

Новый модуль `example`:

1. Каталог `modules/example/` и строка `"example"` в `members`
   `modules/Cargo.toml`.
2. `modules/example/Cargo.toml`:

   ```toml
   [package]
   name = "liftoff-module-example"
   description = "…"
   version.workspace = true
   edition.workspace = true
   rust-version.workspace = true
   license.workspace = true
   repository.workspace = true
   authors.workspace = true
   publish.workspace = true

   # Самый старый liftoffd, на котором работает модуль: `liftoffd_min`
   # релиза в index.json. Без этой таблицы модуль не релизится.
   [package.metadata.liftoff]
   liftoffd_min = "2.0.0-alpha.2"

   [lib]
   # `example.wasm` в `modules/target/wasm32-wasip2/release/`.
   name = "example"
   crate-type = ["cdylib"]

   [lints]
   workspace = true

   [dependencies]
   serde_json = { workspace = true }
   wit-bindgen = { workspace = true }
   ```

   Зависимости добавляются в `[workspace.dependencies]` файла
   `modules/Cargo.toml` с `default-features = false`. Под `wasm32-wasip2`
   нет C-тулчейна, поэтому годятся только чистые Rust-крейты (`ring` и
   `aws-lc-rs` не собираются; `oidc` проверяет RS256 своим кодом поверх
   `num-bigint`). `cargo deny` проверяет workspace модулей с корневым
   `deny.toml`.
3. `src/lib.rs`: биндинги из общего `wit/` и экспорт.

   ```rust
   #[expect(
       clippy::error_impl_error,
       clippy::mem_forget,
       clippy::same_length_and_capacity,
       reason = "code wit-bindgen generates"
   )]
   mod bindings {
       wit_bindgen::generate!({
           path: "../../wit",
           world: "liftoff:module/liftoff-module",
           generate_all,
       });
   }

   use bindings::exports::liftoff::module::module::{Guest, HookResult, Request, Response};
   use bindings::liftoff::module::host::{self, Error};

   struct Example;

   impl Guest for Example {
       fn manifest() -> String { /* JSON, раздел 3 */ }
       fn migrate(from: u32) -> Result<(), Error> { /* раздел 5 */ }
       fn handle_route(req: Request) -> Response { /* раздел 7 */ }
       fn handle_tool(name: String, args: String) -> Result<String, Error> { /* … */ }
       fn on_hook(hook: String, payload: String) -> Result<HookResult, Error> {
           Ok(HookResult::Continue)
       }
       fn tick(_name: String) {}
   }

   #[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
   mod export {
       use super::Example;

       crate::bindings::export!(Example with_types_in crate::bindings);
   }
   ```

   В workspace модулей `unsafe_code = "deny"`, а не `forbid`, именно
   ради этих двух `#[expect]`; остальные линты те же, что в корне.
   Модуль из нескольких слов называется с точкой (`dns.cloudflare`), а
   каталог и крейт через дефис (`modules/dns-cloudflare`,
   `liftoff-module-dns-cloudflare`, библиотека `dns_cloudflare`).

`just modules` собирает все компоненты в
`modules/target/wasm32-wasip2/release/` (вне git). `just fmt-check`,
`just lint` (clippy под `wasm32-wasip2`), `just doc`, `just deny` и
`just modules-release-check` проверяют workspace модулей вместе с
корневым; всё это входит в `just ci`.

## 2. Мир `liftoff:module`

Импорты `host` (подробности и лимиты: `docs/ARCHITECTURE.md`, «Модули»):

| Импорт | Что делает |
|---|---|
| `store-get(key)`, `store-set(key, value)` | своё пространство ключей; ключ до 256 байт, значение до 1 МиБ, больше это trap |
| `store-query(sql, params)` | один SQL-оператор над своими таблицами `m_<модуль>_*`, ответ JSON-массив строк (раздел 5) |
| `config-get(key)` | поле конфига, с которым модуль загружен; секреты расшифрованы (раздел 4) |
| `core-call(service, method, args)` | вызов сервиса ядра от имени актора текущего вызова, JSON (раздел 8) |
| `schedule(name, every-secs)` | `tick(name)` каждые `every-secs` секунд (раздел 6) |
| `log(level, msg)` | строка в журнале демона с именем модуля |

Плюс `wasi:http/outgoing-handler@0.2.12` (раздел 9) и остальной WASI 0.2,
который тянет `std` (часы, random, stdio), без каталогов, сокетов и DNS.
`wit/deps/` — копия WIT крейта `wasmtime-wasi-http` из `Cargo.lock`
(`just wit-deps`, проверка `just wit-deps-check`).

Экспорты `module`:

| Экспорт | Когда зовётся | Актор |
|---|---|---|
| `manifest()` | при каждой загрузке (установка, `config`, `enable`, старт демона) и при сборке релиза | нет |
| `migrate(from)` | один раз при установке, `from = 0` | нет |
| `handle-route(req)` | маршрут модуля, команда CLI модуля, хуки данных на `/.hook/…` | `req.actor` (ID пользователя) или `none` |
| `handle-tool(name, args)` | инструмент MCP модуля; вызовы порта `Git` для модулей `git.*` | вызывающий MCP; у вызовов `Git` нет |
| `on-hook(hook, payload)` | хуки ядра, на которые модуль подписан | у `authorize` и `gate` актор операции |
| `tick(name)` | планировщик | нет |

Все вызовы одного модуля идут по одному (экземпляр под мьютексом), так что
гонок чтения-записи внутри модуля нет. Каждый вызов ограничен
`[modules] call_timeout` (по умолчанию 10 с), память модуля
`[modules] max_memory_mb` (по умолчанию 64 МиБ). Trap, таймаут или
паника проваливают только свой вызов, следующий идёт в новом экземпляре;
три сбоя модуля подряд отключают модуль (`enabled = false` с ошибкой,
424 на его маршрутах, `lof module enable` после починки).

## 3. Манифест

`manifest()` возвращает JSON, который ядро разбирает строго
(`ModuleManifest::parse`, `crates/core/src/module/manifest.rs`):
неизвестное поле где угодно это отказ загрузки (MOD-06). Обязательны
только `name` и `version`.

| Поле | Значение |
|---|---|
| `name` | строчные латинские слова (буквы и цифры, первая буква) через точку, до 64 байт (`dns.cloudflare`) |
| `version` | semver; модули берут `env!("CARGO_PKG_VERSION")` |
| `requires` | модули, которые должны быть установлены раньше (`tenants` → `oidc`) |
| `config_schema` | JSON Schema конфига; без неё модуль конфига не принимает |
| `http_allow` | хосты `wasi:http`: имя или IP-литерал, с портом или без (раздел 9) |
| `commands` | команды CLI: `name` (до четырёх слов через пробел), `description`, `args` (`name`, `type` из `string`, `integer`, `boolean`, `project`, `required`, `hint`) |
| `routes` | маршруты: `method`, `path` (под `/api/v1/m/<name>`), `public` (без сессии) |
| `tools` | инструменты MCP: `name`, `description`, `schema` (JSON Schema аргументов) |
| `migrations` | число миграций хранилища (сейчас хост его только читает, см. раздел 5) |
| `hooks` | хуки ядра (раздел 7) |
| `replaces` | команды CLI ядра, которые модуль берёт на себя; действует только `login` (`oidc`) |
| `git_provider` | `{"hosts": [...], "oauth": bool}`: форж-хосты, которые обслуживает модуль `git.<provider>` |

Манифест читается при загрузке с уже заданным конфигом, поэтому может от
него зависеть: `oidc` выводит `http_allow` из `issuer`, `git.forgejo`
выводит `http_allow` и `git_provider.hosts` из `hosts`. Новый конфиг
действует после перезагрузки модуля (`lof module config` её делает).
Сборка релиза читает манифест без конфига.

Пример (`modules/testing/probe`):

```json
{
  "name": "probe",
  "version": "0.1.0",
  "config_schema": {
    "type": "object",
    "properties": {
      "greeting": {"type": "string", "description": "How to greet"},
      "token": {"type": "string", "format": "secret", "description": "A token to keep secret"}
    },
    "required": ["greeting"],
    "additionalProperties": false
  },
  "commands": [{"name": "hello", "description": "Says hello",
                "args": [{"name": "who", "type": "string", "required": true, "hint": "Who to greet"}]}],
  "routes": [{"method": "POST", "path": "/hello"}],
  "migrations": 1
}
```

## 4. Конфиг

`lof module install <имя> --set k=v --set k=@file` (содержимое файла без
последнего перевода строки) или `POST /api/v1/modules {name, version,
config}`; без `--set` CLI спрашивает обязательные поля на терминале
(вопрос это `description`). Конфиг проверяется по `config_schema`
(`jsonschema`, `$ref` наружу не ходит); ошибка называет поле
(`config.<поле>`), но не значение. `lof module config <имя> --set …`
сливает поля с сохранёнными (`PATCH`, `null` в API удаляет поле) и
перезагружает модуль без миграции.

- Поле верхнего уровня с `"format": "secret"` ядро запечатывает
  мастер-ключом; `"format": "secret"` глубже (вложенный объект, `$defs`,
  `anyOf`) это отказ загрузки. CLI спрашивает секретные поля без эха.
- `config-get(key)` отдаёт строку как есть, любое другое значение его
  JSON-текстом (`git.forgejo` разбирает `hosts` из `["git.example.org"]`).
- `--set` типизирует значение по `type` схемы: `integer`, `number` и
  `boolean` разбираются, поле `array` или `object` принимает JSON
  (`--set 'hosts=["git.example.org"]'`), массив строк ещё и `a,b`
  (`--set hosts=git.example.org,code.example.org`, части без пробелов по
  краям); не-JSON для объекта это отказ до отправки (64). Всё прочее
  уходит строкой.

## 5. Хранилище

**Ключи.** `store-get`/`store-set` — своё пространство ключей модуля
(таблица ядра `module_keys`), чужие ключи не видны (MOD-01). Удобно для
кэшей и маленького состояния (`oidc` держит там discovery, JWKS и
начатые входы).

**Таблицы.** `store-query(sql, params)` выполняет ровно один оператор
(`;` и хвост это `validation`) только над таблицами с префиксом модуля:
`m_` + имя с `.` → `_` + `_` + одно слово из строчных латинских букв и
цифр (`m_dns_cloudflare_records`, `m_tenants_memberships`; слово без
`_`, поэтому пространства `dns` и `dns.cloudflare` не пересекаются).
Разрешены `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE`/`DROP TABLE`,
`CREATE`/`DROP INDEX` (имя индекса по тому же правилу), `ALTER TABLE`,
`REINDEX`, функции и рекурсивные CTE. Отказ `forbidden` на всё остальное:
таблицы ядра и других модулей, `ATTACH`, `PRAGMA`, транзакции и
savepoint, временные объекты, представления, триггеры, виртуальные
таблицы, `VACUUM`, `CREATE TABLE … AS SELECT`, внешний ключ на таблицу не
модуля, `AUTOINCREMENT` (бери `INTEGER PRIMARY KEY` без него). Полный
список и причины: `docs/ARCHITECTURE.md`, «`store-query` и авторизатор
SQLite».

- `params` привязываются к `?1`, `?2`, … всегда как текст; колонка с
  affinity `INTEGER` приводит такой параметр сама, в выражении без
  колонки нужен `CAST(?1 AS INTEGER)`.
- Ответ — JSON-массив объектов по именам колонок (`NULL` → `null`, числа,
  текст, blob → base64).
- Лимиты одного оператора: 2 с, 4 МиБ ответа, 64 колонки; превышение это
  `validation`.
- Ошибки: `forbidden`, `validation` (SQL, ограничение, лимит), остальное
  `internal`.

**Миграции.** При установке ядро зовёт `migrate(0)`, и модуль создаёт
свои таблицы (`if from == 0 { CREATE TABLE IF NOT EXISTS … }`). Если `migrate`
вернул ошибку, установка отменяется. `config`, `enable` и рестарт демона
миграцию не зовут. Обновления версии установленного модуля пока нет
(`PATCH /modules/{name}` меняет только `enabled` и `config`), поэтому
`migrate` с `from > 0` не зовётся, а поле `migrations` ни на что не
влияет. Модуль без таблиц отвечает `Ok(())`.

Повторная установка после `lof module remove` без `--purge` снова зовёт
`migrate(0)` над оставшимися таблицами, поэтому `migrate(0)` обязан
быть повторяемым: `CREATE TABLE IF NOT EXISTS` и `CREATE INDEX IF NOT
EXISTS`, начальные строки только в пустые таблицы. Так делают
`dns.cloudflare` (записи остаются) и `tenants` (арендаторы и участники
остаются, импорт v1 и `default` не повторяются).

**Удаление.** `lof module remove <имя>` оставляет таблицы и ключи;
`--purge` удаляет таблицы `m_<имя>_*` и ключи модуля.

## 6. Планировщик

`schedule(name, every-secs)` заводит или заменяет период имени,
`every-secs = 0` снимает его. `tick(name)` следующего периода начинается
только после конца предыдущего, пропущенные периоды не догоняются. Не
больше 32 имён, имя до 64 байт, период не короче 10 с; остальное хост
игнорирует с предупреждением в журнале. `tick` идёт без актора, поэтому
`core-call` из него это `unauthenticated`.

Расписание живёт в памяти загруженного модуля: trap его не сбрасывает, а
перезагрузка модуля (`config`, `enable`) и рестарт демона сбрасывают.
Экспорта «модуль загружен» в мире нет, поэтому расписание заводит
какой-либо вызов модуля (маршрут, хук, инструмент; `migrate` только при
установке). Модули liftoff планировщиком не пользуются; пользуются им
контрактные компоненты `probe` и `allimports`.

## 7. Хуки, маршруты, команды и инструменты

**Хуки.** Подписка — имя в `hooks` манифеста; ядро зовёт
`on-hook(hook, payload)` с JSON. Подписчики спрашиваются в порядке
установки; первый `deny` побеждает (и для админа), `allow` не мешает
следующему отказать, иначе `continue` («нет мнения»). `allow` в
`authorize` не открывает действий только для админа (`admin.*`,
`module.install|update|remove`).

| Хук | Полезная нагрузка | Ответ |
|---|---|---|
| `authorize` | `{"actor":{"id","login","role"},"action","resource"}` (ресурсы и действия: `docs/ARCHITECTURE.md`, «`authorize` и действия») | `allow`, `deny`, `continue`; сбой модуля это отказ |
| `before_deploy` | `{"environment":{"id","project","name","limits"},"deploy":{"id","number"}}` | `deny` проваливает деплой до Docker; сбой это отказ |
| `after_deploy` | как `before_deploy` | уведомление: ответ и сбой только в журнал |
| `domain_created`, `domain_removed` | `{"domain":{"id","host","kind","proxied"}}` | `deny` на `domain_created` забирает домен назад; сбой только в журнал |
| `gate` | `{"host","actor","request":{"method","path","headers"}}` | `allow` пускает на защищённый хост; сбой это отказ |

Точный формат полезных нагрузок — rustdoc `liftoff_wasm::ModuleSet`
(`crates/wasm/src/set.rs`).

`hook-result` не несёт данных, поэтому хуки, которые отвечают значением,
хост зовёт через `handle-route` без актора на зарезервированных путях
(манифест не может объявить маршрут ниже `/.hook`, снаружи они
недостижимы):

| Хук | Запрос | Ответ |
|---|---|---|
| `authenticate` | `POST /.hook/authenticate`, тело `{"method","path","headers"}` | `200` с ID пользователя (актор), `401`/`403` с причиной (отказ), иначе «нет мнения» |
| `render_page` | `GET /.hook/render_page/<kind>?host=<host>&return=<url>` (`login`, `forbidden`, `unavailable`, `offline`; параметры, когда страница их знает) | `200` с HTML, `302`/`303`/`307` с абсолютным `https` `location` — редирект (ядро идёт по нему, только если ответил модуль `web` и `location` на origin его панели; `web` спрашивается первым), иначе встроенная страница |
| `tenant_quotas` | `POST /.hook/tenant_quotas`, `{"tenant"}` | `200` `{"max_projects","env_cpus_hundredths","env_memory_mb"}`, `404` нет квот; сбой это `503` операции |
| `project_tenant` | `POST /.hook/project_tenant`, `{"actor","tenant"}` | `200` `{"tenant"}` или problem `400`/`403`/`404`/`409`; сбой это `503` |
| `mcp_resource` | `POST /.hook/mcp_resource`, `{}` | `200` `{"resource","audiences"}`, `404` нет мнения; спрашивается только модуль `mcp`, сбой это `503` |
| `web_panel` | `POST /.hook/web_panel`, `{}` | `200` `{"origin"}` (`https://<host>[:<port>]`, `http://` только на `localhost` и `127.0.0.1`), `404` нет панели; спрашивается только модуль `web`, сбой это «панели нет» |
| `verify_token` | `POST /.hook/verify_token`, `{"token","audiences"}` | `200` `{"issuer","subject","login","email","role","expires_at"}` (Unix-секунды), `401` problem с причиной (отказ), `404` нет мнения; спрашивается только модуль, заменяющий `login` (`oidc`), сбой это `503` |

Модуль, подписанный на такой хук, разбирает эти пути в своём
`handle-route` (см. `modules/oidc`, `modules/tenants`, `modules/mcp`,
`modules/web`).

Хуки данных — `mcp_resource`, `verify_token`, `web_panel` и редирект в
ответе `render_page` (с `host` и `return` в строке запроса) — понимает
демон начиная с `2.0.0`: манифест с неизвестным хуком старый демон не
загрузит, поэтому модуль, который на них подписан, ставит
`liftoffd_min = "2.0.0"` (так у `mcp`, `web` и `oidc`). Три хука данных
ядро спрашивает не у всех подписчиков, а у одного модуля по имени или
роли: `mcp_resource` — у `mcp`, `web_panel` — у `web`, `verify_token` —
у модуля, заменяющего `login`; редирект `render_page` ядро принимает
только от `web` и только на origin его панели. Подписка другого модуля
на эти хуки ничего не меняет. Как ядро пользуется ответами:
`docs/ARCHITECTURE.md`, «MCP по HTTP» и «Панель `liftoff-web`»;
действия `artifact.*`, о которых ядро спрашивает `authorize`, — там
же, «`authorize` и действия». Хук, поднятый
`core-call` самого модуля (его `authorize`), до этого модуля не доходит:
хост сразу отвечает за него `continue`.

**Маршруты.** `/api/v1/m/<module>/<путь>` с методом и путём из `routes`
попадает в `handle-route`; маршрут сопоставляется по пути без строки
запроса, `{name}` в объявленном пути совпадает с одним непустым
сегментом, модуль видит путь со строкой запроса. Маршрут без `public`
требует сессии (401 без неё), и `req.actor` это ID пользователя;
`public` маршрут (OIDC-колбэк) идёт без актора. Заголовки приходят без
учётных данных liftoff и cookie; `Set-Cookie` с именами ядра
(`__Host-liftoff_*`) ядро из ответа модуля вырезает (кроме cookie входа
у модуля браузерного входа `oidc`). Ошибки принято
отдавать `application/problem+json` с `detail`.

**Команды CLI.** Команда из `commands` видна в `GET /modules`, и CLI
строит её на лету (`crates/liftoff/src/dynamic.rs`): `lof <слова
команды> --<арг> V`, `integer` число, `boolean` флаг, `project` берётся из
`--project` или репозитория рабочего каталога. CLI шлёт `POST
/api/v1/m/<module>/<слова через />` с аргументами JSON-объектом и
печатает ответ как пришёл. Поэтому каждой команде нужен маршрут `POST`
с этим путём в `routes` (так делают `probe` и `tenants`).

**Инструменты MCP.** Инструмент из `tools` оба транспорта MCP (`lof
mcp` и `/mcp` демона, `docs/MCP.md`) показывают как
`liftoff_<module>_<tool>` (точка в имени модуля становится `_`, имя не
длиннее 64 символов). Вызов `POST /api/v1/modules/{name}/tools/{tool}`
проверяет аргументы по `schema` инструмента и зовёт `handle-tool` от
имени вызывающего; ответ модуля должен быть JSON. Инструменты, которых
нет в `tools` (вызовы порта `Git` у `git.forgejo`), снаружи не вызвать.

## 8. `core-call`

`core-call(service, method, args)` вызывает сервис ядра от имени актора
текущего вызова, и сервис спрашивает `authorize`, как для любого
вызывающего. Без актора (`tick`, `migrate`, публичный маршрут) это
`unauthenticated`. Таблица методов (`projects.list`, `projects.get`,
`environments.list`, `environments.get`, `secrets.resolve`,
`auth.create_session`, `system.api_url`, `environments.set_protected`,
`tenancy.*`) и их ограничения: `docs/ARCHITECTURE.md`, «`core-call`», и
rustdoc `CoreBridge::METHODS`. Метод не из таблицы это `forbidden`.
`auth.create_session` доступен только модулю, заменяющему `login`;
`environments.set_protected` и `tenancy.*` только модулю `tenants`.

## 9. `wasi:http`

Запрос уходит, только если его хост и порт есть в `http_allow` (запись
без порта разрешает 80 для `http` и 443 для `https`), иначе
`HTTP-request-denied` до соединения. Хост, который является или
разрешается в непубличный адрес (loopback, частные сети, link-local,
`100.64.0.0/10`, ULA), отклоняется (`destination-IP-prohibited`). TLS
делает хост (`rustls`, корни `webpki-roots`), компонент TLS не несёт. Не
больше 16 открытых запросов на экземпляр и 64 КиБ полей в одном наборе.

Тело запроса до 4096 байт (один `blocking-write-and-flush`) пишите
целиком до `outgoing-handler.handle`: сервер, который отвечает не читая
тело, иначе закроет поток под модулем (`modules/oidc/src/http.rs`). Тело
длиннее пишите после `handle` кусками по 4096 байт: до отправки хост
буферизует только один кусок (`modules/git-forgejo/src/http.rs`).

## 10. Ошибки

`host.error { code, message }` модуля ядро переводит по коду:
`validation` (400), `unauthenticated` (401), `forbidden` (403),
`not_found` (404), `conflict` (409), `gone` (410), `module_required`
(424), `upstream` (502, сбой того, к чему модуль ходит), `unavailable`
(503). Любой другой код это 500, а текст уходит только в журнал демона.
Trap и таймаут это 500 для вызывающего и счёт к отключению модуля.

## 11. Тесты

Крейты модулей своих тестов не имеют: `just ci` не запускает `cargo test`
в workspace модулей (цель `wasm32-wasip2`). Модуль проверяется как собранный компонент против
настоящего хоста тестами хоста:

- `crates/wasm/tests/<модуль>.rs`, если модулю хватает пространства
  ключей: хост поверх `InMemoryStore` ядра (`oidc.rs`, `git_forgejo.rs`);
- `crates/liftoffd/tests/<модуль>.rs`, если модуль пользуется
  `store-query`: таблицам нужен SQLite (`SqliteStore`), а адаптеры `wasm`
  и `store` друг от друга не зависят даже в dev-зависимостях, поэтому
  такие тесты живут в корне композиции (`dns_cloudflare.rs`,
  `tenants.rs`).

Помощники в `liftoff_wasm::testing` (фича `test-support`): `component(lib)`
читает `<lib>.wasm` из каталога `WASM_MODULES_DIR` (его задаёт
`justfile`), иначе из `modules/target/wasm32-wasip2/release/`; без
собранного компонента тест падает с подсказкой `just modules`, а не
пропускается. `Host`, `limits()`, `Listener`, `RecordingCore` (фейк
`core-call`), `within`. Внешний API подменяется фейком на loopback:
`ModuleHost::routing_https_to(authority, addr)` отправляет запросы модуля
к `https://<authority>` на адрес фейка, `http_allow` проверяется по
authority как обычно. Сценарии с ядром собираются из
`liftoff_core::fakes::World` и `ModuleHooks` над `ModuleSet` хоста
(см. `crates/liftoffd/tests/dns_cloudflare.rs`). Тесты называются по
правилам репозитория (`mod <единица> { fn <ожидание>_when_<состояние>() }`)
и ссылаются на каталог кейсов (`// TC: DNS-01`).

`just test` и `just ci` сначала запускают `just modules`. Компоненты в
`modules/testing/*` (`probe`, `trap`, `loop`, `badhost`, `badmanifest`,
`allimports`, `denier`) проверяют сам хост (контракт
`liftoff_wasm::testing::contract`), а не модули; в реестр они не
релизятся (у них нет `[package.metadata.liftoff]`).

## 12. Релиз

Модули релизятся в отдельный репозиторий-реестр
`<owner>/liftoff-modules`; `liftoffd` читает `index.json` его последнего
релиза (`[modules] registry`, по умолчанию API релизов
`github.com/unwanation/liftoff-modules`; реестр на Forgejo отвечает тем
же форматом).

1. Версия модуля это `version` его `Cargo.toml` (сейчас у всех
   `version.workspace = true`, `0.1.0`; модуль, который выходит отдельно,
   задаёт свою `version`). `liftoffd_min` в
   `[package.metadata.liftoff]` — самый старый демон, на котором он
   работает.
2. `just modules-release-check` (часть `just ci`) прогоняет релиз всех
   модулей с одноразовым ключом: манифест читается из компонента примером
   `module-manifest` крейта `liftoff-wasm` (ровно как его прочёл бы
   `liftoffd`), компонент подписывается и попадает в индекс.
3. Тег `modules/<каталог>/vX.Y.Z` этого репозитория (`modules/dns-cloudflare/v0.1.0`)
   запускает `.github/workflows/modules.yml`: сборка модуля под
   `wasm32-wasip2`, проверка, что версия компонента равна версии тега,
   подпись minisign (`modules/registry/release.sh`), новый `index.json`
   (индекс последнего релиза реестра плюс эта версия; уже опубликованная
   версия это отказ) и релиз `<каталог>-vX.Y.Z` в `liftoff-modules` с
   `<имя>-<версия>.wasm`, `.minisig` и `index.json`. Релиз остаётся
   черновиком, пока грузятся ассеты.

Секреты репозитория для `modules.yml`:

| Секрет | Что это |
|---|---|
| `MODULES_SIGNING_KEY` | секретный ключ minisign реестра (`minisign -G -W`, без пароля), содержимое файла |
| `MODULES_REGISTRY_TOKEN` | токен с правом писать релизы `<owner>/liftoff-modules` (fine-grained PAT с `Contents: write` на этот репозиторий) |

Публичная половина ключа закрепляется в бинарниках демона и CLI
(`REGISTRY_PUBLIC_KEY`, `crates/core/src/module/registry_remote.rs`: один
ключ и для локального режима CLI); свой реестр значит свою сборку с
своим ключом, а с пустой строкой `lof module install` отказывает
(`503`). Подписан только `.wasm`; демон сверяет SHA-256 и
подпись, затем читает манифест самого компонента и перепроверяет имя,
версию и `requires`.
