# MCP

liftoff отдаёт свой API агентам как сервер MCP: каждая операция REST —
инструмент, инструменты модулей — в том же сервере, логи — ресурсы. Один
сервер (`crates/mcp`, `liftoff-mcp`), два транспорта:

| Транспорт | Где | Кто вызывает API | Что нужно |
|---|---|---|---|
| stdio | `lof mcp` на машине агента | сессия CLI из его конфига | `lof login` |
| Streamable HTTP | `https://<api>/mcp` демона | сессия `mcp` OAuth-токена | модули `oidc` и `mcp` |

## Инструменты

- **Операции API.** Таблица `liftoff_api::OPERATIONS`
  (`crates/api/src/schema.rs`) — строка на каждый эндпоинт `/api/v1`; из
  неё строятся и `/openapi.json`, и инструменты. Имя —
  `liftoff_<операция>` (`liftoff_projects_list`, `liftoff_deploys_create`,
  `liftoff_secrets_set`, `liftoff_artifacts_publish`); аргументы — один
  плоский объект из параметров пути, полей query и полей тела
  (`inputSchema` из `schemars` тех же типов, `additionalProperties:
  false`). Вызов — запрос REST с сессией транспорта; ошибка API —
  результат с `isError: true` и текстом `<статус> <title>: <detail>`.
  Операция логов отвечает один раз (`follow: true` — ошибка
  инструмента).
- **Инструменты модулей** приходят из `GET /api/v1/modules` (только
  включённые) как `liftoff_<модуль>_<инструмент>` (точка в имени модуля
  становится `_`: `liftoff_dns_cloudflare_zones_list`) и вызываются через
  `POST /api/v1/modules/{name}/tools/{tool}`. Имя длиннее 64 символов,
  совпавшее с инструментом API или уже занятое модулем раньше в порядке
  `GET /modules`, не предлагается (причина — в журнал).
- **Ресурсы:** `liftoff://deploys/{id}/log` (журнал деплоя) и
  `liftoff://projects/{p}/environments/{e}/log` (последние 500 строк
  журнала environment).

Подробности генерации: `docs/ARCHITECTURE.md`, «MCP по stdio».

## Алиасы артефактов

Чтобы инструкции, написанные под прежний отдельный сервер артефактов,
работали дальше, рядом с `liftoff_artifacts_*` есть его инструменты с теми
же именами и аргументами (`crates/mcp/src/aliases.rs`):

| Алиас | Аргументы | Тот же вызов, что |
|---|---|---|
| `publish_artifact` | `{html, title?, files?}` | `liftoff_artifacts_publish` |
| `update_artifact` | `{id, html, title?, files?}` | `liftoff_artifacts_update` |
| `share_artifact` | `{id}` | `liftoff_artifacts_share` |
| `unshare_artifact` | `{id}` | `liftoff_artifacts_unshare` |
| `list_artifacts` | `{}` | `liftoff_artifacts_list` (только свои) |
| `list_artifact_versions` | `{id}` | `liftoff_artifacts_versions` |
| `delete_artifact` | `{id}` | `liftoff_artifacts_delete` |

`files` — `{имя: содержимое}`: строка — текст (алиас кодирует его в
base64), `{content, encoding: "base64"}` — двоичный файл. Ответы — поля
предшественника (`id`, `url`, `shared`, `expires_at`, плюс `version` и
`login_url`; `{artifacts: [...]}`, `{versions: [...]}`, `{deleted: true,
id}`), текстом JSON и как `structuredContent`. Описания говорят агенту
то же, что предшественник: публиковать любой HTML для человека, отдавать
`url` как есть, приватный артефакт исчезает через TTL, `share` делает его
публичным и бессрочным, `fetch()` своих файлов и `localStorage` внутри
не работают. Модель артефактов: `docs/ARTIFACTS.md`.

## stdio: `lof mcp`

`lof mcp` берёт URL и сессию выбранного сервера из конфига CLI
(`lof login`, `.liftoff`, `lof server use`) и говорит JSON-RPC по
stdin/stdout; его журнал — только в stderr. Модули не нужны.

```sh
claude mcp add --transport stdio lof -- lof mcp
```

## HTTP: `/mcp`

Транспорт Streamable HTTP — `liftoff_mcp::http::StreamableHttp`, сервер
`rmcp` как tower-сервис. Демон монтирует его на `POST|GET|DELETE /mcp`
хоста API рядом с REST-роутером (`crates/liftoffd/src/mcp_http.rs`);
`GET /.well-known/oauth-protected-resource` — маршрут REST в корне хоста
API (`crates/rest/src/routes/mcp.rs`). Оба отвечают `424` («install the
module `mcp`»), пока модуль `mcp` не установлен и не включён.

### Установка

```sh
lof module install oidc --set issuer=https://id.example.com --set client_id=…   # если ещё нет
lof module install mcp \
  --set resource=https://liftoff.example.com/mcp \
  --set allowed_audiences=<aud access-токенов>
```

Конфиг модуля `mcp` (`modules/mcp`):

| Поле | Значение |
|---|---|
| `resource` | URL ресурса, `https://<api>/mcp`: его отдаёт `/.well-known/oauth-protected-resource` |
| `allowed_audiences` | непустой список `aud`, один из которых должен нести access-токен (`a,b` или JSON) |
| `client_id` | необязательно: клиент, заранее созданный у провайдера для MCP-клиентов без динамической регистрации; демон его не читает, это запись для оператора — клиенту его называют сами (`--client-id` ниже) |

Модуль требует `oidc` (без него установка — `424`), маршрутов и таблиц
не имеет и отвечает один хук данных `mcp_resource`.

### OAuth

liftoff — resource server OAuth (RFC 9728), сервер авторизации —
провайдер модуля `oidc`:

1. Запрос без токена или с отвергнутым токеном — `401` с
   `WWW-Authenticate: Bearer
   resource_metadata="https://<api>/.well-known/oauth-protected-resource"`.
2. Клиент читает метаданные: `{resource, authorization_servers:
   [<issuer oidc>], scopes_supported: [<scopes модуля oidc> +
   offline_access]}`, находит провайдера и проводит у него вход с этими
   scope (Zitadel отвергает запрос без `scope`)
   (authorization code с PKCE; клиент — из динамической регистрации или
   заранее созданный).
3. С access-токеном в `Authorization: Bearer` демон спрашивает ядро
   (`McpAuthService::verify_oauth`): модуль `oidc` проверяет токен хуком
   `verify_token` тем же кодом, что ID-токен входа (подпись RS256 по
   JWKS, `iss` — его `issuer`, `aud` — один из `allowed_audiences`,
   срок; `azp` не сверяется), и отвечает личностью: `sub`, логин из
   `preferred_username`, e-mail, роль из claim ролей.
4. Ядро принимает только `issuer`, заданный для `oidc`, создаёт или
   обновляет пользователя `(oidc:<issuer>, sub)`, как вход, и открывает
   сессию `mcp` до `exp` токена. Инструменты вызывают REST API демона
   по loopback (`[api] bind`) с секретом этой сессии, так что актор
   каждого инструмента — владелец токена, с его правами.

Причина отказа пишется только в журнал демона. Другой пользователь с
тем же логином — `403` с причиной (повторный OAuth не поможет).

Требования к провайдеру: access-токен — JWT (у Zitadel `access_token_type
= "JWT"`) с `aud` из `allowed_audiences`, `preferred_username` и, для
роли админа, claim ролей; PKCE (`S256`); динамическая регистрация клиента
или заранее созданный публичный клиент с redirect URI MCP-клиента.
`oidc` перечитывает JWKS из-за неизвестного `kid` не чаще раза в минуту.

### Сессии

- **Сессия API** (`mcp`): одна на токен. Её секрет — HMAC-SHA256
  access-токена под ключом установки (выведен из мастер-ключа), так что
  повторный запрос с тем же токеном находит её по хешу без памяти
  процесса, а сам токен не годится как bearer REST API. Новый токен
  (refresh) — новая сессия того же пользователя; истёкшие сессии `mcp`
  пользователя удаляются, когда открывается новая. После удаления или
  отключения модуля `mcp` REST отвергает и эти сессии (`401`).
- **Сессия транспорта** (`Mcp-Session-Id`): в памяти демона, не больше
  16 на пользователя и 1024 всего (сверх потолка закрывается самая
  старая — своя у пользователя), простаивающая 5 минут закрывается.
  Она отвечает только пользователю, который её открыл (`404` другому), и
  кончается вместе с сессией API её последнего запроса, включая поток
  событий `GET /mcp`. Каждый запрос несёт свой bearer и проверяется
  заново. Клиент, получивший `404` на сессию, начинает новую, как велит
  MCP.
- `/mcp` обслуживает только `Host` из `[api] public_url` (защита от DNS
  rebinding, `HttpConfig::allowed_hosts`); остановка демона закрывает
  открытые потоки событий.

### Подключение Claude

Claude Code:

```sh
claude mcp add --transport http liftoff https://liftoff.example.com/mcp
# без динамической регистрации у провайдера:
claude mcp add --transport http --client-id <client_id> --callback-port 33418 \
  liftoff https://liftoff.example.com/mcp
claude mcp list          # liftoff: needs authentication
```

Затем в сессии `/mcp` → `liftoff` → «Authenticate»: браузер открывает
вход провайдера, после него видны инструменты `liftoff_*` и алиасы.
Redirect URI заранее созданного клиента — `http://localhost:<callback-port>/callback`.

Claude.ai: Settings → Connectors → Add custom connector, URL
`https://liftoff.example.com/mcp` (без динамической регистрации — в
Advanced settings `client_id`, redirect URI
`https://claude.ai/api/mcp/auth_callback`).

## Как модуль добавляет инструменты

Инструмент объявляется в `tools` манифеста модуля (`name`,
`description`, `schema` — JSON Schema аргументов) и обрабатывается в
`handle-tool`. Он появляется в обоих транспортах как
`liftoff_<модуль>_<инструмент>`, пока модуль установлен и включён:
сервер читает `GET /modules` на каждом `tools/list`, так что перезапуск
`lof mcp` не нужен. Ядро проверяет аргументы по `schema` до
модуля (`400 arguments.<поле>`), зовёт `handle-tool` от имени вызывающего
(`core-call` модуля несёт его права) и ждёт JSON в ответ. Рецепт:
`docs/MODULES.md`, раздел 7.

## Ответы `/mcp`

| Статус | Когда |
|---|---|
| `424` | модуль `mcp` не установлен или выключен (проверяется первым, и без токена) |
| `401` + `WWW-Authenticate` | нет токена, токен не прошёл проверку или он другого `issuer` |
| `403` | токен верный, но логин занят другим пользователем |
| `404` | сессия транспорта неизвестна, закрыта или чужая |
| `503` | модуль не смог ответить |
