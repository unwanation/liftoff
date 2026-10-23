# `lof dev`: проект на машине и dev-адрес

`lof dev` запускает проект на машине разработчика и, когда есть сервер,
открывает к нему dev-адрес `https://<env>.<base>`: туннель от демона до
локального порта. Код: `crates/liftoff/src/commands/dev.rs` и
`dev/tunnel.rs` (CLI), `crates/tunnel` (протокол), `crates/liftoffd/src/listeners/tunnel.rs`
и `tunnels.rs` (демон), `liftoff_core::service::TunnelsService` (правила).

```sh
lof dev                 # команда + туннель, если есть сервер
lof dev --share         # dev-адрес открыт всем, без входа
lof dev --local         # без туннеля, даже с сервером
lof dev --port 4100     # порт вместо свободного из 4000–4999
lof env share --env shop-dev     # то же, что --share, вне dev
lof env unshare --env shop-dev   # снова только со входом
lof status              # строка `tunnel <env> <state>` на каждый туннель проекта
```

## Команда

1. Что запускать: `[dev] command` из `liftoff.toml`, иначе то, что
   предлагает детекция стека (`package.json` и его менеджер); без
   команды — exit 64 и подсказка написать `[dev]`.
2. Порт: свободный из 4000–4999 (`--port N` — свой). Команде он
   приходит в `PORT`; Vite, Astro, SvelteKit и Expo игнорируют `PORT`,
   им CLI дописывает `--port N`. Если строка сама называет `--port N`,
   запуск идёт на N, а другой `--port` CLI — ошибка аргументов (64).
3. Секреты environment (`--env`, иначе `.liftoff`, иначе `prod`) — с
   сервера (`GET /secrets/resolve`), без сервера — из локального
   хранилища плюс `LIFTOFF_PROJECT`, `LIFTOFF_ENV` и пустой
   `LIFTOFF_DOMAIN`, как их ставит сервер. Всё это только в окружении
   дочернего процесса, никогда в аргументах.
4. Команда — `sh -c <строка>` в корне рабочего дерева: строка из
   манифеста самого человека, никогда с сервера (`docs/ARCHITECTURE.md`,
   «Исключения»). stdin закрыт, stdout и stderr проходят насквозь, CLI
   печатает `http://localhost:<PORT>`. Процесс — лидер своей группы:
   `Ctrl-C`, `SIGTERM`, `SIGHUP` шлют группе `SIGTERM`, через 5 секунд
   `SIGKILL`; выход — 130.

Функция (`[function]`) запускается так же: `[dev] command` шаблонов
`fn-*` собирает компонент и запускает `wasmtime serve` на `$PORT`
(`docs/FUNCTIONS.md`, «Проект функции»).

Без сервера (локальный режим) или с `--local` туннеля нет: без сервера
CLI говорит это один раз, `--share` тогда ничего не меняет; `--local
--share` — ошибка аргументов (64). Без проекта (нет `--project` и
`origin` на форже) команда запускается без секретов и без туннеля.

## Туннель и dev-адрес

С сервером и без `--local`, когда команда уже запущена, CLI открывает
`wss://<api>/api/v1/tunnel` с `cli`-сессией (`lof login`) и проектом в
`X-Liftoff-Project`. Демон:

- находит или создаёт tunnel-environment пользователя в проекте:
  `<project>-dev`, а с включённым модулем `tenants` — `<project>-<login>`,
  у каждого свой; владелец — вы, видимость `private`;
- добавляет ему managed-домен `<env>.<base>` (хук `domain_created`, то
  есть DNS-запись с модулем `dns.*`) и привязку;
- после рукопожатия (`Hello` с обеих сторон) ставит `running` и
  маршрут Caddy → tunnel-listener (`[tunnel] bind`).

CLI печатает в stdout один раз за запуск, после первого соединения,
`https://<env>.<base>` (имя — из `Hello` сервера, база — `base_domain`
из `GET /server`) и для `private` — ссылку входа (ниже). Запрос на
dev-адрес идёт: браузер → Caddy → tunnel-listener → поток `yamux`
внутри WebSocket → `127.0.0.1:<PORT>`. WebSocket самого приложения
(HMR и прочее) проходит через туннель как есть.

Что видит приложение: запрос без cookie `__Host-liftoff*`, без
`Authorization` с токеном liftoff, без `X-Forwarded-*`, `Forwarded` и
`X-Liftoff-*` браузера; listener ставит свои `X-Forwarded-For` (адрес
клиента, который записал локальный Caddy), `X-Forwarded-Proto: https`,
`X-Forwarded-Host: <env>.<base>` и для пропущенного человека
`X-Liftoff-User`, `X-Liftoff-Email`. Пути `/.liftoff/*` принадлежат
liftoff и до приложения не доходят. Порт не принимает соединение — CLI
сам отвечает `502`.

## Приватность и share

Новый туннель `private`. Его пускают:

- владельца туннеля и админа — с сессией этого хоста (cookie
  `__Host-liftoff_gate`) или с `cli`-сессией в `Authorization: Bearer`
  (`curl -H "Authorization: Bearer …"`);
- с модулем `tenants` — ещё тех, кого пускает хук `gate`.

Браузер без сессии получает `302` на вход, JSON-клиент (`Accept:
application/json`) — `401`, сессия того, кому нельзя, — `403` (HTML-страница
отказа). Проверка — до первого байта в туннель.

**Вход по ссылке** (без модуля `oidc`). Ссылка, которую печатает `lof
dev`, — `https://<api>/api/v1/auth/links/lfl_…` (заголовок
`X-Liftoff-Login-Url` ответа upgrade): одноразовая, 10 минут, только для
этого environment. Открытая в браузере, она создаёт сессию `browser` на
24 часа (cookie API-хоста `__Host-liftoff_session`) и сразу передаёт её
на dev-адрес одноразовым кодом: `302` на
`https://<env>.<base>/.liftoff/callback?code=lfc_…`, listener гасит код,
ставит cookie `__Host-liftoff_gate` и возвращает на `/`. Браузер,
пришедший на `private`-адрес без сессии, попадает на страницу входа
`https://<api>/login?host=&rd=`: туда вставляется ссылка (или токен
`lfl_…`); использованная или просроченная ссылка — та же страница с
пояснением и `410`, ссылка другого environment — `403`. С модулем `oidc`
вход — страница модуля. При переподключениях ссылка не печатается снова:
новая — при следующем `lof dev`.

**Share.** `lof dev --share` открывает свой туннель всем до upgrade
(`POST …/environments/<env>/share`; при первом `lof dev`, когда туннеля
ещё нет, — сразу после `Hello`), и ссылка входа тогда не печатается.
`lof env share --env <env>` и `lof env unshare --env <env>` делают то же
вне `dev`; `share` и `unshare` туннеля может только его владелец или
админ.

## Обрыв, `Ctrl-C`, offline

- Keepalive — пинги клиента раз в 15 секунд; сервер рвёт соединение без
  кадров клиента дольше 45 секунд, клиент — без кадров сервера дольше
  трёх интервалов.
- Обрыв: CLI пишет в stderr «the tunnel connection broke; reconnecting»
  и переподключается с backoff (1 секунда, удвоение, потолок 60
  секунд), по возвращении — «the tunnel is back». Через
  `tunnel.offline_after` (30 секунд) без клиента environment становится
  `offline`, и Caddy отдаёт `503` «dev offline»; пока environment ещё
  `running`, но клиента нет, listener отвечает той же страницей.
- `Ctrl-C`: CLI шлёт кадр `Shutdown` — environment `offline` и маршрут
  снимается сразу, — затем завершает группу команды.
- Второй `lof dev` того же пользователя и проекта вытесняет первый
  (кадр `Evicted`): первый пишет об этом один раз и дальше работает
  только локально, не переподключаясь.
- Отказ upgrade `401` (сессии нет, она истекла) или `403` (не
  `cli`-сессия, deploy-токен) — окончательный: CLI пишет «log in again
  with `lof login`», команда работает локально.
- Старт демона делает все туннели `offline`. `lof env rm <env>` туннеля
  удаляет environment, домен и маршрут.

## Лимиты демона

| Ключ `[tunnel]` | По умолчанию | Что ограничивает |
|---|---|---|
| `bind` | `127.0.0.1:7004` | адрес tunnel-listener'а (только loopback) |
| `max_streams` | `64` | одновременные запросы одного туннеля; лишний — `503` |
| `offline_after` | `30s` | сколько потерянный туннель ждёт клиента до `offline` |
| `bandwidth` | `{ bytes_per_second = 10485760, burst = 1048576 }` | полосу environment на оба направления (токен-бакет, переживает переподключение) |

Сообщение WebSocket — не больше 256 КиБ с обеих сторон; окно `yamux` —
256 КиБ на поток `max_streams` с запасом в четверть, так что память
демона на туннель ограничена.

Каждый туннель занимает порт `[domains] port_range` в своей привязке,
хотя его никто не слушает (колонки портов привязки `NOT NULL`). За
внешним прокси (`tls = "edge"`) idle-таймауты на пути WebSocket должны
быть длиннее интервала keepalive: короткий `read_timeout` рвал бы
туннель.
