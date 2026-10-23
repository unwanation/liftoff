# Артефакты

Артефакт — HTML-страница с файлами рядом, которую liftoff хранит с
версиями и показывает в браузере по постоянному адресу. Публикуют его
`lof artifact publish`, REST или MCP-инструментом (так Claude показывает
человеку отчёт, прототип или диаграмму). Это замена прежнего отдельного
сервера артефактов («предшественник» ниже): его инструменты MCP
сохранены как алиасы, его данные переносит `liftoffd import-artifacts`.

## Модель

- **Артефакт** — `id` из 12 символов base62 (как у предшественника),
  владелец (пользователь, который опубликовал), видимость `private` или
  `shared`, срок `expires_at` (только у `private`), номер текущей версии,
  времена создания и изменения. Заголовок артефакта — заголовок его
  текущей версии.
- **Версия** — номер `n` (с 1), HTML (`index.html` версии), файлы и
  заголовок. Версия неизменяема: `update` пишет новую целиком (HTML и
  все файлы), а не правит старую.
- **Файлы** — относительные пути, на которые ссылается HTML
  (`img/logo.png`, `app.js`). Имя нормализуется (`a/./b` → `a/b`,
  пустые сегменты отбрасываются) и отвергается (`400` на поле
  `files.<имя>`), если это абсолютный путь, есть сегмент `..` или
  сегмент с ведущей точкой (скрытые файлы), обратная косая черта,
  управляющие символы, сегмент длиннее 255 байт, путь длиннее 1024 байт,
  имя `index.html` (это HTML версии) или два файла одного имени.
  Проверка у входа (`ArtifactPath::new`, `crates/core/src/domain/artifact.rs`).
- **Размер.** Сумма байт версии вместе с HTML не больше
  `[artifacts] max_size_mb` МиБ; отказ (`400` на `files`) — до любой
  записи. Тело запроса ограничено тем же лимитом плюс 256 КиБ на разметку
  (у JSON — с поправкой 4/3 на base64); больше — `413` до чтения тела.
- **Заголовок версии** — переданный (`--title`, `title`; пробелы
  схлопнуты, не длиннее 200 символов), иначе `<title>` HTML (с
  раскрытыми сущностями), иначе заголовок предыдущей версии, иначе
  `Untitled`.

Правила живут в `ArtifactsService` (`crates/core/src/service/artifacts.rs`).

## Кто что может

Каждая операция спрашивает хук `authorize` один раз:
`artifact.publish` и `artifact.list` — о `Admin`; `artifact.get`,
`artifact.update`, `artifact.share`, `artifact.unshare`,
`artifact.rollback`, `artifact.remove`, `artifact.remove_version`,
`artifact.versions` — о самом артефакте. После хука действует правило
ядра: менять, читать и открывать ссылкой артефакт может только его
владелец или админ, даже если модуль разрешил.

- Без модулей правило ядра для «нет мнения» пускает только админа: в
  одиночном режиме единственный пользователь и есть админ.
- С `oidc` без `tenants` публикуют только админы.
- Модуль `tenants` открывает `artifact.publish` и `artifact.list` всем и
  разрешает ресурс `artifact`: каждый публикует и видит свои артефакты.
  Артефакт принадлежит пользователю, а не тенанту.
- `ls --all` (`GET /artifacts?all=true`) — только админу (`403`).

## Версии

- `update <id>` — новая версия `n+1`, она становится текущей; адрес
  артефакта не меняется.
- `rollback <id> --to N` — копия версии `N` (её файлы и заголовок) как
  новая текущая версия; блобы не копируются, новая версия ссылается на
  те же.
- Хранится не больше `[artifacts] versions` (50) версий: самые старые
  удаляются вместе с файлами, текущая — никогда.
- `rm <id> --version N` удаляет одну версию; текущую удалить нельзя
  (`409`). `rm <id>` удаляет артефакт со всеми версиями.
- `/a/{id}` показывает текущую версию, `/a/{id}/v/{n}` — версию `n`.

## Доступ, share и TTL

- Новый артефакт `private`: его видят владелец и админ после входа.
  Срок жизни — `[artifacts] ttl` (5 часов) от последней версии:
  `publish` и `update` приватного артефакта ставят `expires_at = now +
  ttl`.
- `share` делает артефакт `shared`: его видит любой по ссылке, срок
  снимается (`expires_at = null`). Share относится к артефакту целиком:
  по ссылке открывается любая его версия. `update` shared-артефакта
  оставляет его shared.
- `unshare` возвращает приватность и заводит новый срок от текущего
  момента.
- Истёкший приватный артефакт уже не существует: `404` и для просмотра,
  и для операций, и в `ls` его нет. Удаляет его вместе с файлами sweeper
  (`ArtifactsService::sweep`): демон зовёт его при старте и раз в минуту
  (`crates/liftoffd/src/sweep.rs`, `SWEEP_ARTIFACTS_EVERY`); артефакт,
  который не удалось удалить, пробуется снова через минуту.

**Вход в браузере.** Приватный артефакт требует браузерную сессию
API-хоста (cookie `__Host-liftoff_session`): вход через `oidc` или
одноразовая ссылка. Ответ `publish` приватного артефакта несёт
`login_url` — одноразовую ссылку (10 минут, одно использование) для
владельца; `lof artifact open <id>` выдаёт свежую. Ссылка открывает
сессию `browser` на 24 часа и ведёт прямо на артефакт. Без сессии
браузер уходит на страницу входа с возвратом на артефакт,
JSON-клиент получает `401`, сессия того, кому нельзя, — `403`.

## Адрес

- По умолчанию `https://<api>/a/<id>` (`<api>` — хост `[api]
  public_url`).
- С `[artifacts] host = "artifacts.example.com"` — `https://artifacts.example.com/<id>`:
  тот же просмотрщик отвечает на этом хосте теми же путями без `/a`
  (`/{id}`, `/{id}/v/{n}`, `/{id}/v/{n}/{файл}`); `/a/` API-хоста тогда
  `404`, а на `artifacts.host` всё, кроме артефактов (`/api/v1`,
  `/login`, `/gate/start`), — `404`. Caddy ведёт этот хост на `[api]
  bind`. Cookie API-хоста туда не доходит, поэтому приватный артефакт
  получает сессию как приватный адрес environment: браузер идёт на
  `https://<api>/gate/start?host=<artifacts.host>&rd=/<id>`, оттуда с
  одноразовым кодом на `https://<artifacts.host>/.liftoff/callback`,
  который ставит cookie хоста `__Host-liftoff_gate`. `host`, равный
  хосту API, конфиг отвергает (выход 78).

## Просмотр

`crates/rest/src/routes/artifacts_view.rs`, `crates/rest/src/pages/artifact.rs`.

- `GET /a/{id}` и `GET /a/{id}/v/{n}` — страница-обёртка демона: верхняя
  панель с заголовком, «version n of m», переключатель версий (ссылки с
  заголовком и датой каждой версии), ссылка на сырой HTML и, для
  владельца или админа, кнопки share/unshare (формы `POST
  /a/{id}/share|unshare`, проверка `Origin` своего хоста, затем `303` на
  обёртку). Ниже — `<iframe sandbox="allow-scripts allow-forms
  allow-popups allow-modals" src="…/v/{n}/index.html">`. HTML артефакта
  в обёртку не попадает никогда; у самой обёртки нет скриптов.
- `GET /a/{id}/v/{n}/index.html` и `GET /a/{id}/v/{n}/{файл}` — содержимое
  версии с заголовками:
  - `Content-Security-Policy: sandbox allow-scripts allow-forms
    allow-popups allow-modals; frame-ancestors 'self'`;
  - `X-Content-Type-Options: nosniff`;
  - тип по расширению (`html` — `text/html; charset=utf-8`, `css`,
    `js`/`mjs`, `json`, `svg`, `png`, `jpg`, `webp`, шрифты, `pdf`,
    `wasm`, аудио…), неизвестное расширение — `application/octet-stream`;
  - `Cache-Control: private, no-store` у `private`, `public,
    max-age=300` у `shared`.
- Каждый запрос — `Access::check` с целью `artifact:<id>`: `shared`
  пускает без сессии, `private` — сессию `browser` из cookie или сессию
  `cli` в `Authorization: Bearer`, владельца или админа. Хук `gate` для
  артефактов не спрашивается.
- Неизвестный артефакт, версия (не число, `0`, удалённая) и файл —
  `404`. Файл отдаётся, только если его называет хранимая строка версии.

## Что работает внутри артефакта

HTML исполняется в opaque origin (`sandbox` в CSP и у iframe, без
`allow-same-origin`), а не в origin API или панели, — как у
предшественника:

- работают скрипты (в том числе inline), формы, `alert`/`confirm`,
  `window.open`;
- работают относительные `<script src>`, `<img>`, `<link
  rel="stylesheet">`, `url(…)` в CSS — к файлам версии;
- скрипты и стили с CDN грузятся как обычно;
- **не работают** `fetch()`/`XMLHttpRequest` своих файлов, `localStorage`,
  `sessionStorage`, IndexedDB и cookie: данные и состояние надо
  вставлять в сам HTML;
- в отличие от предшественника, в песочнице нет `allow-downloads` и
  `allow-popups-to-escape-sandbox`: скачивание и окна без песочницы не
  работают;
- страницу может встроить в рамку только её собственный хост
  (`frame-ancestors 'self'`).

## CLI

```sh
lof artifact publish report.html --files chart.js,img/logo.png --title "Q3"
#   https://liftoff.example.com/a/Ab12Cd34Ef56     адрес
#   https://liftoff.example.com/api/v1/auth/links/lfl_…   одноразовая ссылка входа
lof artifact update Ab12Cd34Ef56 report.html --files chart.js
lof artifact share Ab12Cd34Ef56          # открыть всем, без срока
lof artifact unshare Ab12Cd34Ef56        # снова приватный, срок от сейчас
lof artifact open Ab12Cd34Ef56 --browser # свежая ссылка входа (и открыть браузер)
lof artifact ls [--all]                  # ID TITLE VISIBILITY VERSION EXPIRES
lof artifact versions Ab12Cd34Ef56       # VERSION TITLE SIZE CREATED
lof artifact rollback Ab12Cd34Ef56 --to 2
lof artifact rm Ab12Cd34Ef56 [--version 3]
```

- `--files` — через запятую или повторением; элемент — `путь` или
  `имя=путь`. Путь внутри текущего каталога называется в артефакте путём
  от него (`./img/a.png` → `img/a.png`), путь вне его — базовым именем
  (`../shared/logo.png` → `logo.png`), `имя=путь` — `имя`. Нечитаемый
  файл или имя, которое не пройдёт проверку, — ошибка до запроса.
- `publish` печатает `url`, затем `login_url` (по строке); `update` —
  `url`; `rm` — `removed <id>` или `removed version N of <id>`; `--json`
  — ответ как получен.
- `open --browser` отдаёт ссылку `xdg-open` (на macOS `open`); сбой —
  предупреждение в stderr, ссылка уже напечатана.
- Артефакты требуют сервера: в локальном режиме команды недоступны.

## REST

Под `/api/v1`, за сессией:

| Маршрут | Что делает |
|---|---|
| `POST /artifacts` | `publish`: `201` `ArtifactView` с `login_url` у приватного |
| `PUT /artifacts/{id}` | `update`: новая версия |
| `GET /artifacts[?all=true]` | свои артефакты (все — админу) |
| `GET /artifacts/{id}` | `ArtifactView` |
| `DELETE /artifacts/{id}` | `{"deleted": "<id>"}` |
| `GET /artifacts/{id}/versions` | `[{n, title, size, created_at}]`, от старых к новым |
| `POST /artifacts/{id}/rollback` | `{to}` → новая версия |
| `DELETE /artifacts/{id}/versions/{n}` | `{"deleted": "<id>/v/<n>"}` |
| `POST /artifacts/{id}/share`, `/unshare` | `ArtifactView` |

Версия приходит в двух формах: `multipart/form-data` (поле `html`,
необязательное `title`, по полю `files` на файл с именем в `filename`) или
JSON `{html, title?, files: {имя: base64}}` (форма MCP-инструментов).
`ArtifactView`: `id`, `url`, `title`, `shared`, `expires_at`, `version`,
`created_at`, `updated_at` и `login_url` (только у `publish` приватного).
Без сессии — `401`, чужой артефакт — `403`, неизвестный — `404`.

## MCP

Каждая операция — инструмент `liftoff_artifacts_<действие>`
(`publish`, `update`, `get`, `list`, `versions`, `rollback`, `delete`,
`delete_version`, `share`, `unshare`) с JSON-формой версии. Рядом —
алиасы предшественника с его именами и аргументами:

| Алиас | Аргументы | Ответ |
|---|---|---|
| `publish_artifact` | `{html, title?, files?}` | `{id, url, shared, expires_at, version, login_url}` |
| `update_artifact` | `{id, html, title?, files?}` | то же без `login_url` |
| `share_artifact`, `unshare_artifact` | `{id}` | то же |
| `list_artifacts` | `{}` | `{artifacts: [...]}` |
| `list_artifact_versions` | `{id}` | `{versions: [...]}` |
| `delete_artifact` | `{id}` | `{deleted: true, id}` |

`files` алиаса — объект `{имя: содержимое}`: строка — текст файла,
`{content, encoding: "base64"}` — двоичный файл. Ответ идёт и текстом
JSON, и как `structuredContent`. Транспорты, OAuth и подключение к
Claude: `docs/MCP.md`.

## Конфигурация

`/etc/liftoff/liftoffd.toml`:

```toml
[artifacts]
ttl = "5h"          # срок приватного артефакта от последней версии
max_size_mb = 16    # сумма байт версии, МиБ
versions = 50       # сколько версий хранить
host = ""           # пусто: https://<api>/a/<id>; иначе https://<host>/<id>
```

## Хранение

Строки — в SQLite демона: `artifacts`, `artifact_versions`,
`artifact_files` (миграция `0011_artifacts.sql`), счётчик ссылок на блобы
`artifact_blobs` и ссылки файлов `artifact_blob_links`
(`0012_artifact_blobs.sql`). Файлы — за портом ядра `Blobs`, на диске
`FileBlobs` (`crates/store/src/blobs.rs`) под `paths.data/artifacts`:

- `blobs/<sha256>` — каждое содержимое один раз (контент-адресуемое
  хранилище), запись во временный файл и `rename`;
- `<id>/<n>/<имя>` — файлы версии как hardlink'и на блобы (на файловой
  системе без hardlink — копии; выбор при старте), `index.html` —
  обычный файл версии;
- блоб удаляется вместе с последней ссылкой на него; при старте
  собирается оставшееся от упавшего процесса (временные файлы, блобы без
  ссылок старше часа, каталоги версий без строки).

Резервная копия — `paths.data` и база демона вместе.

## Импорт данных предшественника

```sh
sudo systemctl stop liftoffd
sudo -u liftoff liftoffd --config /etc/liftoff/liftoffd.toml \
  import-artifacts /path/to/artifacts-data
sudo systemctl start liftoffd
```

`<dir>` — `DATA_DIR` предшественника: `artifacts.db` (открывается только
на чтение) и `artifacts/<id>/` с `index.html` и файлами. Команда
запускается при остановленном демоне, как `setup --import`.

- Каждый артефакт сохраняет `id`, заголовок, времена (миллисекунды
  эпохи предшественника) и `shared`/`expires_at` и становится версией 1
  со своими файлами.
- Владелец — пользователь liftoff с тем же `users.email` (точное
  совпадение); нет такого, e-mail пуст или он у нескольких
  пользователей — админ.
- Пропускаются (считаются в `skipped`): уже существующий `id`, `id` не
  из 12 символов base62, артефакт без каталога или без `index.html`.
  Файл, путь которого liftoff не принимает (скрытый сегмент, не UTF-8),
  и не обычный файл (симлинк) не входят в версию. Каждая причина — в
  stderr.
- Лимит `max_size_mb` и хук `authorize` не спрашиваются: это перенос
  оператором уже принятого.
- Повторный запуск ничего не меняет; артефакт, который прерванный
  прогон оставил без `index.html`, пишется заново.
- Итог — `imported`, `skipped`, `owners defaulted` в stderr, код `0`;
  нет каталога или базы — код `1`, сбой чтения или записи — `70`.

Правила импорта — `ArtifactImport` в `liftoff-core`
(`crates/core/src/service/artifacts/import.rs`), чтение предшественника
— `crates/liftoffd/src/import_artifacts.rs`.

## Отличия от предшественника

- Версии, `rollback`, `versions`, страница-обёртка с переключателем.
- `files` алиасов — объект `{имя: содержимое}`, а не массив `{path,
  content, encoding}`.
- Адрес файла — `/<id>/v/<n>/<файл>`; ссылки предшественника
  `/<id>/raw/…` не обслуживаются.
- В песочнице нет `allow-downloads` и `allow-popups-to-escape-sandbox`.
- Имена с сегментами `.` нормализуются, а не отвергаются.
