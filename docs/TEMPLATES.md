# Как сделать шаблон

Шаблон liftoff это обычный репозиторий на форже (Forgejo, GitHub или
GitLab) с файлом `liftoff.toml` в корне ветки по умолчанию. `lof create`
показывает его в каталоге, генерирует из него репозиторий нового проекта,
подставляет имя и домен, спрашивает секреты и подключает проект к
серверу. Исходники шаблонов, которые идут с liftoff, лежат в
`templates/<имя>/` этого репозитория и публикуются отдельными
репозиториями:

| Каталог | Репозиторий | Что |
|---|---|---|
| `templates/fn-ts/` | `github.com/unwanation/liftoff-fn-ts` | функция на TypeScript: `componentize-js` 0.23.0 через `jco` 1.39.0, Bun |
| `templates/fn-rust/` | `github.com/unwanation/liftoff-fn-rust` | функция на Rust: цель `wasm32-wasip2`, `wit-bindgen` 0.63 |
| `templates/fn-python/` | `github.com/unwanation/liftoff-fn-python` | функция на Python: `componentize-py` 0.25.1, uv |

Тест `crates/core/tests/templates.rs` разбирает каждый
`templates/*/liftoff.toml` тем же `ProjectManifest::parse`, что и
каталог, и проверяет, что файлы из `[substitute]` у функций на месте.
`just functions` (часть `just ci`) собирает каждый шаблон функции его же
`[function] build` в копии под `target/templates/<имя>/`
(`scripts/build-templates.sh`, `{{project}}` подставлен), а
`crates/wasm/tests/templates.rs` пропускает компонент через проверку
деплоя (`liftoff_wasm::functions::check`), активирует его с прогревом и
проверяет `GET /`, `POST /echo` и 404.
Шаблоны функций — компоненты мира `liftoff:function/function` (копия
`wit/function/` в их `wit/`, `scripts/wit-deps.sh --check` держит её
равной), с теми же инструментами и версиями, что контрактные функции;
что они отвечают и как собираются — `docs/FUNCTIONS.md`, «Проект
функции».

## Каталог

Каталог (`lof templates [--group G]`, первый шаг `lof create`) собирается
из двух списков:

- `[templates] repos` в конфиге сервера (`liftoffd.toml`; по умолчанию
  `["github.com/unwanation/liftoff-fn-ts"]`, `config/default.toml`);
- `[local] templates` в конфиге CLI (`~/.config/liftoff/config.toml`):
  добавляется к серверному списку, а без сервера это весь каталог.

Запись это `[хост/]владелец/имя`. Без хоста (`acme/app`) репозиторий
ищется через каждое подключение пользователя (`lof git connect`) по
порядку хостов, и берётся первое, где нашёлся `liftoff.toml`. Модуль
`git.*` читает файл с ветки по умолчанию учёткой самого пользователя:
приватный шаблон виден только тем, кому его видит форж. Прочитанное
кэшируется на 10 минут, поэтому исправленный манифест появляется в
каталоге не сразу. Репозиторий без `liftoff.toml`, с манифестом без
`[template]` или невалидным, или недоступный попадает в предупреждения
каталога и не ломает остальной список. Шаблоны группируются по `group` и
сортируются по имени.

Репозиторий должен уметь становиться шаблоном на своём форже:

- Forgejo и GitHub генерируют проект через `POST /repos/{шаблон}/generate`,
  поэтому в настройках репозитория включается «Template repository».
  Forgejo копирует только содержимое git (`git_content`), GitHub все
  ветки (`include_all`).
- У GitLab шаблонных репозиториев нет: модуль `git.gitlab` делает fork
  с новым именем и сразу снимает связь fork; нужен доступ на чтение
  шаблона.

## `liftoff.toml`

Неизвестный ключ на любой глубине это ошибка с путём
ключа (`service.expose`).

```toml
[template]
group = "web"             # web | api | function | service
name = "app"              # имя в каталоге и имя проекта по умолчанию
description = "SolidJS, Vite, Bun"

[service]                 # compose-проект
compose = "deploy/compose.yaml"
expose = "web:3000"       # сервис:порт, на который идёт домен

# [function]              # вместо [service]: WASM-компонент
# build = "bun install && bun run build"
# component = "dist/fn.wasm"

[dev]
command = "bun dev"

[secrets]
SESSION_SECRET = { hint = "32+ random bytes", generate = "random:32" }
DATABASE_URL = { hint = "postgres://…", scope = "environment" }

[substitute]
files = ["README.md", "package.json", "deploy/compose.yaml"]
```

- `[template]` есть только у шаблона; `group`, `name` и `description`
  обязательны, управляющие символы в `name` и `description` запрещены.
- Ровно один из `[service]` и `[function]`. `expose` необязателен, но
  без него проект не получит привязку домена (`lof bind` потом). Домен
  функции `lof create` привязывает к `prod` сам: у функции нет
  `service:port` (другие домены — `lof bind <host>`). Все
  пути (`compose`, `component`, файлы `[substitute]`) относительные,
  внутри репозитория, без `..`.
- `[function] build` это команда оболочки, которая собирает компонент
  `component`; компонент экспортирует `wasi:http/incoming-handler@0.2`.
  Её запускает CI-workflow и `lof deploy` (`docs/FUNCTIONS.md`, «Сборка
  и деплой»), поэтому она сама ставит зависимости (`bun install && …`);
  раннеру CI нужен тулчейн этой команды.
- `[dev] command` говорит, как проект запускается на машине
  (`bun dev`, `wasmtime serve --addr 127.0.0.1:$PORT …`); его предлагает
  детекция стека, но CLI его не запускает: человек запускает команду
  сам, секреты берёт `lof secret pull --write` (`.env.<env>`), а адрес в
  сети даёт `lof expose <PORT>` (`docs/EXPOSE.md`).

## Подстановки

В файлах из `[substitute] files` и только в них `{{project}}`
заменяется именем проекта, а `{{domain}}` его доменом
(`<имя>.<базовый домен>` или `--domain`). Остальные фигурные скобки не
трогаются, шаблонизатора нет. Файл из списка, которого нет, не UTF-8 или
симлинк, пропускается.

- С сервером `lof create` клонирует сгенерированный репозиторий и
  подставляет оба значения до коммита.
- Без сервера (локальный режим) домена ещё нет: подставляется только
  `{{project}}`, а `{{domain}}` подставит `lof server connect`, когда
  сервер назовёт домен, и закоммитит изменённые файлы вместе с остальным
  планом.

Сам `liftoff.toml` в `[substitute]` не включают: каталог читает его до
подстановки.

## Секреты

Каждый ключ `[secrets]` это секрет, который `lof create` кладёт проекту.

- `hint` показывается в вопросе о значении (`--secret K=V` в batch).
- `generate = "random:<n>"`: CLI не спрашивает, а генерирует `n`
  случайных байт (1–65535) и сохраняет их в base64url без `=`.
- `scope`: `project` (по умолчанию: все environment проекта),
  `environment` (`prod`, который создаёт `lof create`) или `tenant`
  (общий для проектов тенанта, с модулем `tenants`; существующий секрет
  тенанта не перезаписывается).

Без сервера значения ложатся в локальное хранилище
(`~/.config/liftoff/secrets.db`), и `lof server connect` берёт их оттуда.
Все вопросы задаются до генерации репозитория: отказ или недостающий
`--secret` ничего не создают. Ключи `LIFTOFF_TOKEN`, `REGISTRY_USER` и
`REGISTRY_TOKEN` (секреты CI) объявлять не нужно: их заводит план.

## Сниппет CI

Шаблону не нужно своего workflow деплоя. Когда проект подключается к
серверу (`lof create` с сервером или `lof server connect`), план пишет
файл для форжа нового репозитория, если такого файла в нём ещё нет, и
коммитит его:

| Форж | Файл |
|---|---|
| Forgejo | `.forgejo/workflows/deploy.yml` |
| GitHub | `.github/workflows/deploy.yml` |
| GitLab | `.gitlab-ci.yml` |

Compose-проект собирает образ `docker buildx`, пушит его в registry
форжа как `sha-<коммит>` и вызывает `POST /api/v1/deploys` с
`compose`-файлом манифеста; функция выполняет `[function] build` и
загружает `component`. Секреты CI (`LIFTOFF_TOKEN`, а у Forgejo для
compose ещё `REGISTRY_USER` и `REGISTRY_TOKEN`) план отправляет в
репозиторий сам. `lof token snippet` печатает тот же workflow для
существующего проекта.

Шаблон, которому нужен свой workflow (раннер без Docker, другая сборка),
коммитит его по пути своего форжа: существующий файл план не
перезаписывает. Workflow должен вызывать `POST /api/v1/deploys` с
`Authorization: Bearer $LIFTOFF_TOKEN` и полями `repo`, `ref`, `tag` и
`compose` (или `component` у функции).

## Проверка шаблона

1. `lof templates` показывает шаблон в своей группе; если нет, его
   причина в предупреждениях (через 10 минут после исправления).
2. `[dev] command` в клоне шаблона запускает проект, `lof expose <PORT>`
   открывает его порт.
3. `lof create demo --template <хост>/<владелец>/<имя>` создаёт проект;
   без сервера он останавливается после пуша, и `lof server connect`
   подключает его потом.
