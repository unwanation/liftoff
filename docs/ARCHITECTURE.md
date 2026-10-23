# Architecture

## Design brief

**Goal.** liftoff, a deploy service for one server and its domain, built
so the business rules can be tested
without Docker, Caddy or Forgejo.

**Shape.** Hexagonal ("ports and adapters"). `liftoff-core` owns the types,
rules and the *ports* (traits). Driven adapters (`store`, `host`, `proxy`,
`wasm`) implement the ports; the driving adapter (`rest`) calls the services,
and `mcp` (also over HTTP, mounted by the daemon) calls `rest`. The
daemon `liftoffd` composes them; the CLI `liftoff` speaks HTTP to it, and
without a server (local mode) calls the same core services over
its own adapters and its own module host (see «Локальный режим CLI»). Its
host processes are listed in «Исключения» below.

**Assumptions made while scaffolding** (change freely):

| Decision                                   | Why                                                                  |
|--------------------------------------------|----------------------------------------------------------------------|
| One crate per interface | Adapters compile independently and never depend on each other |
| Ports return `impl Future`, services are generic over the ports | Static dispatch; zero boxing on the hot path; fakes in tests without mocking frameworks |
| `thiserror` enums in libraries, no `anyhow` anywhere | Exit codes and HTTP statuses need typed errors                       |
| Config = defaults ← TOML ← env (`figment`) | 12-factor friendly; unknown keys rejected                             |
| JSON logs via `tracing`, stderr when stdout is a protocol channel | The CLI owns stdout                                       |
| Loopback binds by default                  | Exposure is an explicit deployment decision                          |
| `panic = "unwind"` kept                    | Lets the REST panic guard return 500 instead of dropping the connection |
| `overflow-checks = true` in release        | Security over a few percent throughput                               |
| Hooks via lefthook, tools via mise         | Same pinned versions locally and in CI; `bun` and `uv` (in `mise.toml`) only build the TS and Python contract functions (`just functions`) |

## Crate dependency rules

```text
liftoffd ──► rest, store, host, proxy, wasm ──► core
liftoffd ──► tunnel
liftoffd ──► mcp (: /mcp, liftoffd::mcp_http)
store ──► module-sql ──► core
rest ──► api
liftoff (CLI) ──► core, wasm, module-sql, mcp, api (: локальный режим)
liftoff (CLI) ──► tunnel (: lof dev)
mcp ──► api
tunnel ──► (ни одного крейта workspace)
```

| Крейт | Роль |
|---|---|
| `crates/core` (`liftoff-core`) | домен, сервисы, порты, хуки, реестр модулей (`core::module`: манифест, `ModuleRegistry`, `ModuleHooks`, `CoreBridge`, `ModuleGit`); без I/O и без wasmtime |
| `crates/api` (`liftoff-api`) | типы провода REST и MCP, таблица операций `OPERATIONS` |
| `crates/rest` (`liftoff-rest`) | driving-адаптер HTTP: axum, `/api/v1`, маршруты модулей `/api/v1/m/…`, гейт, `/login`; — просмотр артефактов (`/a/{id}…` или `[artifacts] host`), `/.well-known/oauth-protected-resource`, CORS панели на `/api/v1` (`cors.rs`) |
| `crates/store` (`liftoff-store`) | SQLite: `Store` и `ModuleStore` демона (ключи модулей и `store-query` через `liftoff-module-sql`, таблицы импорта v1 для `tenants`), импорт v1; `FileBlobs` — порт `Blobs` на диске (`blobs.rs`) |
| `crates/module-sql` (`liftoff-module-sql`) | не адаптер, а SQL хранилища модулей поверх `rusqlite::Connection`: таблица ключей, авторизатор `store-query`, лимиты, `purge`; общий для хранилища демона и `modules.db` CLI, чтобы правило было одно |
| `crates/host` (`liftoff-host`) | Docker Compose, systemd, файлы проекта, `DnsResolver` |
| `crates/proxy` (`liftoff-proxy`) | Caddy admin API |
| `crates/wasm` (`liftoff-wasm`) | хост модулей на wasmtime: загрузка компонентов, импорты `host`, `wasi:http` с allow-list, лимиты, планировщик, кэш компиляции, `ModuleSet` для диспетчера хуков, `HttpModuleSource` (реестр), `HostRuntime` (порт `ModuleRuntime` над хостом и каталогом компонентов: демона и CLI), хост без планировщика (`without_scheduler`, CLI), контракт хоста (`testing`, фича `test-support`); рантайм функций (`functions`: мир `wasi:http/proxy` с `wasi:config` и `wasi:keyvalue`, пул инстансов на environment, лимиты, прекомпиляция, прогрев) |
| `crates/tunnel` (`liftoff-tunnel`) | туннель `lof dev`: кадры управления (`Hello`, `Shutdown`, `Evicted`) текстом WebSocket, `yamux` в бинарных сообщениях, сервер открывает потоки; `TunnelServer` (сторона демона: `serve` над сырым IO после `101`, `forward` запроса с `503` без туннеля и сверх `max_streams`, склейка апгрейдов) и `TunnelClient` (сторона CLI: подключение с сессией и `X-Liftoff-Project`, `502` при закрытом локальном порте, keepalive, backoff до 60 секунд, `Shutdown`, окончательный `Refused` при `401`/`403`); лимит полосы на environment (`Bandwidth`), лимиты сообщения WebSocket и окна `yamux`; `serve_as` держит туннель под ключом (демон: id environment, имена уникальны только в проекте), а в `Hello` называет имя; не зависит ни от одного крейта workspace |
| `crates/mcp` (`liftoff-mcp`) | MCP-сервер на стороне клиента API: инструменты из `OPERATIONS` и из `GET /modules`, алиасы прежнего сервера артефактов (`aliases.rs`), вызовы в REST; транспорты stdio (`lof mcp`) и Streamable HTTP (`http.rs`, tower-сервис, который монтирует демон); зависит только от `api` |
| `crates/liftoffd` | корень композиции: конфиг, `setup`, очередь, реконсиляция, сборка хоста модулей (`modules.rs`, `daemon.rs`); listener'ы на hyper (`listeners/functions.rs`: `[functions] bind`; `listeners/tunnel.rs`: `[tunnel] bind`, `Host` → tunnel-environment → `Access::check` до первого байта → `TunnelServer::forward` со своими `X-Forwarded-*`; общее у обоих в `listeners/common.rs`) и `tunnels.rs` (`DaemonTunnels`: соединение после `101` `GET /api/v1/tunnel` идёт в `TunnelServer` под ключом id environment, его начало и конец — в `TunnelsService`, потерянное через `tunnel.offline_after` проверяется `expire`); — `mcp_http.rs` (`/mcp` рядом с роутером `rest`), `sweep.rs` (sweeper артефактов), `import_artifacts.rs` (`liftoffd import-artifacts`) |
| `crates/liftoff` | CLI `lof`: clap, динамические команды модулей (`dynamic.rs`), `lof module *`, `lof mcp`; локальный режим (`local/`): сервисы ядра над локальными адаптерами и свой хост модулей (раздел «Локальный режим CLI») |
| `modules/*` | отдельный workspace компонентов `wasm32-wasip2`: модули и контрактные компоненты хоста (раздел «Модули», рецепт в `docs/MODULES.md`) |

wasmtime и `wasmtime-wasi*` есть только в `crates/wasm`: `cargo tree -p
liftoff-core` их не содержит. Для локального режима CLI зависит от
`liftoff-core`, `liftoff-wasm` и `liftoff-module-sql`; это осознанное
изменение правила v1 «CLI без логики»: без сервера CLI
вызывает те же сервисы ядра. От серверных адаптеров (`store`, `host`,
`proxy`, `rest`) CLI не зависит (`cargo tree -p liftoff`).

- `core` depends on no framework, runtime or I/O crate. `cargo tree -p liftoff-core`
  should stay a short list.
  One exception: `core::crypto::MasterKey` draws AEAD nonces from the OS
  through `chacha20poly1305`'s `getrandom` feature (`getrandom`, `libc`).
  The cipher lives in core so the daemon and the CLI's local store seal the
  same way, and a nonce must never come from a fake `Rng`.
  `core::system` (`SystemClock`, `OsRng`, `fill`) reads the same random
  source and the wall clock, moved from `liftoffd` so the CLI's
  local mode stamps and identifies its records as the daemon does.
- Adapters depend on `core` and their protocol stack only; never on each other.
- The CLI depends on `core` for local mode only (`core`, `wasm`,
  `module-sql`), never on the server adapters (`store`, `host`, `proxy`,
  `rest`); see «Правило общих сервисов» and «Локальный режим CLI».
- Only `liftoffd` knows about configuration files, signals and which adapters exist.

### Правило общих сервисов

Правила живут в сервисах `liftoff-core` (`core::service`), и только там.
Адаптеры переводят протокол в вызов сервиса и обратно (REST) или дают
сервису I/O (порты); своих правил у них нет. Отсюда:

- демон и CLI не дублируют проверок: с сервером CLI говорит с демоном по
  HTTP и получает отказ сервиса как problem+json;
- без сервера (локальный режим;)
  CLI зависит от `liftoff-core` и `liftoff-wasm` и вызывает **те же**
  сервисы (`GitService`, `ModuleService`, `TemplateCatalog`,
  `CreateService`, `StackDetector`, `core::dev`) над своими адаптерами
  (`crates/liftoff/src/local/`: хранилище секретов, хранилище модулей,
  хост модулей, `git`), а не пишет свою копию правил. Серверные адаптеры
  (`store`, `host`, `proxy`, `rest`) CLI не получит никогда (`cargo tree
  -p liftoff`);
- `liftoff-api` остаётся единственным словарём провода: сервисы
  возвращают доменные типы, `rest` переводит их в типы `api`.

### Порты

Все порты собраны в один трейт `core::ports::Ports` с ассоциированным
типом на порт; сервисы обобщены по `P: Ports`, демон собирает
продакшен-набор (`crates/liftoffd/src/daemon.rs`), тесты собирают
`fakes::FakePorts` с общим `CallLog`.

| Порт | Что даёт сервису | Реализация в демоне |
|---|---|---|
| `Store` | все записи ядра | `liftoff-store` (SQLite); контракт `fakes::store_contract`; в локальном режиме CLI `LocalSecretStore` (только записи локального режима) |
| `Docker`, `ProjectFiles`, `Systemd` | `docker compose`, файлы compose-проекта, slice'ы | `liftoff-host` |
| `Proxy` | `apply(ProxyConfig)` целиком и `ready()` | `liftoff-proxy::CaddyAdmin` (`POST /load` admin API); `ProxyConfig` считает ядро (`core::proxy_config`) из хранилища |
| `Git` | аккаунт, роль в репо, наличие образа, CI-секрет, шаблоны, от имени подключения пользователя | `liftoff_core::module::ModuleGit` над модулями `git.*` (раздел «Модуль `git.forgejo`») |
| `Resolver` | CNAME и A для проверки external-доменов | `liftoff-host::DnsResolver` (публичные резолверы) |
| `Clock`, `Rng` | время, ULID, секреты токенов | `liftoffd::system` |
| `Secrets` | шифрование мастер-ключом; `mac` — HMAC-SHA256 под ключом, выведенным из него (секрет сессии `mcp`) | `core::crypto::MasterKey` |
| `DeployQueue` | постановка деплоя в очередь | `liftoffd::queue::JobQueue` |
| `Hooks` | точки расширения | `liftoff_core::module::ModuleHooks` над модулями хоста (`DaemonHooks`, см. «Диспетчер хуков»); `NoHooks` это ядро без модулей |
| `ModuleStore` | хранилище модуля: своё пространство ключей (`store-get`/`store-set`) и SQL по своим таблицам `m_<module>_*` (`store-query`) | `liftoff-store` (SQLite, авторизатор `Connection::authorizer`, фича rusqlite `hooks`, правила в `liftoff-module-sql`); `LocalModuleStore` CLI (`modules.db`, те же правила); ключи в контракте `fakes::store_contract`, SQL только в SQLite |
| `FunctionHost` | рантайм функций для деплоя: `check` (без компиляции), `precompile`, `activate` (прогрев и переключение одним шагом), `is_active`, `logs`; запросы listener зовёт мимо ядра (`Functions::invoke` в `liftoffd`) | `liftoff_wasm::functions::FunctionDeployer` над `FunctionRuntime` и SQLite-хранилищем (`wasi:keyvalue` → `kv`), `crates/liftoffd/src/functions.rs` |
| `ComponentFiles` | файлы компонентов `paths.data/functions/<hash>.wasm` и их блокировка (`ComponentsLock`) на запись деплоя и чистку | `liftoff_host::FileComponentFiles` |
| `Blobs` | файлы версий артефактов: контент-адресуемые блобы и ссылки файлов версий на них (`put`, `release`, `link`, `read`, `remove_version`, `remove_artifact`) | `liftoff_store::FileBlobs` (`paths.data/artifacts`, счётчик ссылок в SQLite); контракт `fakes::blobs_contract`, фейк `FakeBlobs`; в локальном режиме CLI `Offline` (артефакты требуют сервера) |

### Хуки

`core::ports::Hooks` это точки расширения, на которые подписываются
модули: `authenticate`, `authorize`, `before_deploy` (отказ
`Deny` проваливает деплой до Docker), `after_deploy`, `domain_created`,
`domain_removed`, `gate`, `render_page` и хуки данных `tenant_quotas`,
`project_tenant`, а `mcp_resource` и
`verify_token` (MCP по HTTP) и `web_panel` (панель); `render_page` может ответить и редиректом. У каждого метода реализация по умолчанию
«нет мнения», поэтому `NoHooks` это ядро без модулей. Демон собирает
`ModuleHooks` над модулями, которые
загрузил хост (`module_ports` в `crates/liftoffd/src/daemon.rs`); без
установленных модулей он отвечает так же, как `NoHooks`. Сервисы зовут
хуки в фиксированных местах; хук не обходит правил ядра, а только
добавляет отказ или данные.

### `authorize` и действия

Каждая операция сервиса ядра, которая действует от имени пользователя,
ровно один раз спрашивает единственный `core::service::Authorizer` (кроме
списков для не-админа, см. «Списки и тенанты» ниже)
(`check(actor, Action, &Resource)`), а он спрашивает хук `authorize`:
`Deny` отказывает с причиной (и админу), `Allow` пускает без проверки
роли, кроме действий `admin.*` (`Action::is_admin_only`: `admin.token`,
`admin.users`, `admin.revoke_sessions`, `admin.reconcile`) и установки,
изменения и удаления модулей (`module.install`, `module.update`, `module.remove`), которые
модуль может только сузить, но не открыть никому, кроме админа;
`Continue` значит «решает ядро», и без модулей разрешено только
единственному админу (MOD-04, MOD-05). Действие это
`core::ports::Action` (строка `Action::as_str` видна модулям и журналу
аудита), ресурс это `core::ports::Resource::{Project(id),
Environment(id), Domain(id), Secret(scope), Artifact(id), Admin}`; `Admin` означает
«инсталляция целиком»: всё, у чего нет более узкого ресурса. Проверка
идёт после поиска ресурса по имени (ресурс несёт id), но до любого
изменения и любого внешнего вызова: сервисы зовут
`support::authorize` (ресурс `Admin`) или `support::authorized_project`,
`authorized_environment`, `authorized_domain`, `authorized` (поиск, затем
проверка). Если поиск ничего не нашёл, хук спрашивают про `Admin`, и
`NotFound` получает только тот, кого пустили: отказанный получает
`Forbidden` и не отличает чужое имя от несуществующего. Хук спрашивают
ровно один раз и в этом случае. Тест
`service::authorizer::tests::operations` перебирает `Action::ALL` и
проверяет эту таблицу (вызов, действие, ресурс; операция админа доходит
до конца, так что второй вопрос дальше по её пути был бы виден; без
модулей не-админ получает `Forbidden` на каждое действие).

| Действие | `Action` | Место вызова | `Resource` |
|---|---|---|---|
| `project.create` | `ProjectCreate` | `ProjectService::create` | `Tenant(id)` в тенанте, иначе `Admin` |
| `project.list` | `ProjectList` | `ProjectService::list` | `Admin` |
| `project.get` | `ProjectGet` | `ProjectService::get` | `Project` |
| `project.set_manifest` | `ProjectSetManifest` | `ProjectService::set_manifest` | `Project` |
| `project.remove` | `ProjectRemove` | `ProjectService::remove` | `Project` |
| `project.resolve` | `ProjectResolve` | `ProjectService::resolve` | `Project` |
| `environment.list` | `EnvironmentList` | `EnvironmentService::list` | `Project` |
| `environment.get` | `EnvironmentGet` | `EnvironmentService::get` | `Environment` |
| `environment.update` | `EnvironmentUpdate` | `EnvironmentService::update` | `Environment` |
| `environment.deploys` | `EnvironmentDeploys` | `EnvironmentService::deploys` | `Environment` |
| `environment.logs` | `EnvironmentLogs` | `EnvironmentService::logs` | `Environment` |
| `environment.add` | `EnvironmentAdd` | `EnvironmentService::add` | `Project` |
| `environment.stop` | `EnvironmentStop` | `EnvironmentService::stop` | `Environment` |
| `environment.start` | `EnvironmentStart` | `EnvironmentService::start` | `Environment` |
| `environment.remove` | `EnvironmentRemove` | `EnvironmentService::remove` | `Environment` |
| `environment.share` | `EnvironmentShare` | `VisibilityService::share` | `Environment` |
| `environment.unshare` | `EnvironmentUnshare` | `VisibilityService::unshare` | `Environment` |
| `domain.list` | `DomainList` | `DomainService::list` | `Admin` |
| `domain.add_managed` | `DomainAddManaged` | `DomainService::add_managed` | `Project` |
| `domain.add_external` | `DomainAddExternal` | `DomainService::add_external` | `Project` |
| `domain.remove` | `DomainRemove` | `DomainService::remove` | `Domain` |
| `domain.set_proxied` | `DomainSetProxied` | `DomainService::set_proxied` | `Domain` |
| `binding.bind` | `BindingBind` | `BindingService::bind` | `Environment` |
| `binding.unbind` | `BindingUnbind` | `BindingService::unbind` | Domain (привязанного домена) |
| `deploy.redeploy` | `DeployRedeploy` | `DeployService::redeploy` | `Environment` |
| `deploy.request` | `DeployRequest` | `DeployService::request_cli` | `Environment` |
| `deploy.get` | `DeployGet` | `DeployService::get` | Environment (деплоя) |
| `deploy.rotate_token` | `DeployRotateToken` | `DeployService::rotate_token` | `Project` |
| `deploy.snippet` | `DeploySnippet` | `DeployService::snippet` | `Project` |
| `secret.set` | `SecretSet` | `SecretsService::set` | `Secret(scope)` |
| `secret.get` | `SecretGet` | `SecretsService::get, get_versioned` | `Secret(scope)` |
| `secret.list` | `SecretList` | `SecretsService::list` | `Secret(scope)` |
| `secret.remove` | `SecretRemove` | `SecretsService::remove` | `Secret(scope)` |
| `secret.history` | `SecretHistory` | `SecretsService::history` | `Secret(scope)` |
| `secret.rollback` | `SecretRollback` | `SecretsService::rollback` | `Secret(scope)` |
| `secret.sync` | `SecretSync` | `SecretsService::sync` | `Secret(Project)` |
| `secret.resolve` | `SecretResolve` | `SecretsService::resolve` | `Secret(Environment)` |
| `git.list` | `GitList` | `GitService::list` | `Admin` |
| `git.connect` | `GitConnect` | `GitService::connect` | `Admin` |
| `git.disconnect` | `GitDisconnect` | `GitService::disconnect` | `Admin` |
| `git.templates` | `GitTemplates` | `TemplateCatalog::list` | `Admin` |
| `admin.token` | `AdminToken` | `AuthService::create_admin_token` | `Admin` |
| `admin.users` | `AdminUsers` | `AuthService::users` | `Admin` |
| `admin.revoke_sessions` | `AdminRevokeSessions` | `AuthService::revoke_sessions` | `Admin` |
| `admin.reconcile` | `AdminReconcile` | `Reconciler::reconcile` | `Admin` |
| `module.list` | `ModuleList` | `ModuleService::list` | `Admin` |
| `module.available` | `ModuleAvailable` | `ModuleService::available` | `Admin` |
| `module.install` | `ModuleInstall` | `ModuleService::install` | `Admin` |
| `module.update` | `ModuleUpdate` | `ModuleService::update` | `Admin` |
| `module.remove` | `ModuleRemove` | `ModuleService::remove` | `Admin` |
| `link.issue` | `LinkIssue` | `LinksService::issue` | `Environment` или `Artifact` (цели) |
| `tunnel.open` | `TunnelOpen` | `TunnelsService::open` | `Project` |
| `artifact.publish` | `ArtifactPublish` | `ArtifactsService::publish` | `Admin` |
| `artifact.update` | `ArtifactUpdate` | `ArtifactsService::update` | `Artifact` |
| `artifact.share` | `ArtifactShare` | `ArtifactsService::share` | `Artifact` |
| `artifact.unshare` | `ArtifactUnshare` | `ArtifactsService::unshare` | `Artifact` |
| `artifact.rollback` | `ArtifactRollback` | `ArtifactsService::rollback` | `Artifact` |
| `artifact.remove` | `ArtifactRemove` | `ArtifactsService::delete` | `Artifact` |
| `artifact.remove_version` | `ArtifactRemoveVersion` | `ArtifactsService::delete_version` | `Artifact` |
| `artifact.list` | `ArtifactList` | `ArtifactsService::list` | `Admin` |
| `artifact.versions` | `ArtifactVersions` | `ArtifactsService::versions` | `Artifact` |
| `artifact.get` | `ArtifactGet` | `ArtifactsService::get` | `Artifact` |

Секреты при отказе дописывают в аудит `secret.<операция>.denied`
(SEC-08). `Secret(scope)` это `Tenant(id)`, `Project(id)` или
`Environment(id)`; область тенанта называется через проект
(`ScopeRef::Tenant(project)`,), а проект вне тенантов
спрашивает о `Admin` и получает `424` `tenants`. Не действия (нет пользователя-актора): `DeployService::request_ci`
(deploy-токен CI), `DeployService::run` (очередь), `Reconciler::startup`
и `run`, `DomainService::allow_certificate` и `reverify_external`,
`AuthService::exchange`, `authenticate`, `logout`, `issue_setup_token`,
`bootstrap_admin`, `ModuleService::restore` и `sync_status` (демон сам
восстанавливает и записывает установленное админом), `LinksService::redeem`
(одноразовая ссылка сама удостоверяет пользователя) и `Access::check`
(проверка приватного адреса: сессия владельца или админа, иначе хук
`gate`, как у гейта). У `share`, `unshare` и `link.issue` после
`authorize` действует правило владельца: чужой `tunnel` не-админу не
открыть для всех и не выдать на него ссылку, даже если хук разрешил
(VIS-06); то же у операций над артефактом (кроме `publish` и `list`) и у
ссылки на него: чужой артефакт меняет, смотрит и открывает по ссылке
только админ (ART-11). Внутренние шаги одной операции (например запись
`LIFTOFF_TOKEN` внутри `deploy.rotate_token` или одноразовая ссылка,
которую `tunnel.open` выдаёт владельцу своего `tunnel`, или ссылка
входа, которую `artifact.publish` отдаёт автору) второй раз не
спрашивают.

Правила форжа (`ensure_repo_admin`: роль в репозитории через своё
подключение) и правило «лимиты меняет только админ» в
`environment.update` остаются правилами ядра поверх `authorize`.

**Списки и тенанты (закрывает открытый вопрос 2.1).**
`project.list` и `domain.list` спрашивают `authorize` о `Admin`; если
хук разрешил список не-админу (`tenants` разрешает его всем), ядро
(`Authorizer::visible`) спрашивает хук о каждом элементе с тем же
действием и ресурсом элемента (`Project(id)`, `Domain(id)`) и оставляет
только разрешённые (TEN-02). Админ получает весь список после одного
вопроса, так что MOD-04 для админа и для ядра без модулей не меняется.
`project.create` в тенанте спрашивает о `Resource::Tenant(id)`, без
тенанта о `Admin`; `max_projects` тенанта проверяет ядро по хуку данных
`tenant_quotas` (`Conflict`, TEN-04). Имя проекта уникально во всех
тенантах (compose-проект и slice environment названы по нему), поэтому
`support::project_by_name` ищет без тенанта.

### Модули

Как написать модуль (мир, манифест, хранилище, хуки, тесты, релиз):
`docs/MODULES.md`. Здесь — устройство хоста и решения.

```text
 REST /api/v1/m/<module>/…  /modules*  CLI lof <команда модуля>, lof mcp
        │                      │
        ▼                      ▼
 core: ModuleRouteService   ModuleService ──► ModuleSource (HttpModuleSource: index.json, minisign)
       Authorizer ─► Hooks = ModuleHooks ─┐   └─► ModuleRuntime (liftoffd::DaemonModules:
       DomainService …       Git = ModuleGit┤        файлы paths.data/modules, load, migrate)
                                            │
       ModuleRegistry ◄── исходы вызовов ───┼── манифесты, статус, MOD-09
                                            ▼
 wasm: ModuleSet ─► Module (экземпляр под мьютексом, epoch, StoreLimits) ─► wasmtime component
                        │ импорты host
        ┌───────────────┼──────────────────┬────────────────┬──────────────┐
        ▼               ▼                  ▼                ▼              ▼
  store-get/set/   core-call ─► LateCore   schedule ─►     log ─►      wasi:http ─► http.rs
  query ─►         ─► CoreBridge ─►        scheduler       TracingLog  (http_allow, публичные
  ModuleStore      сервисы ядра            (tokio, tick)               адреса) ─► send.rs
  (SqliteStore)    (authorize актора)                                  (TCP на проверенные, rustls)
```

Демон собирает это в `module_ports` (`crates/liftoffd/src/daemon.rs`):
один `ModuleHost` над `SqliteStore`, `TokioTimer`, `TracingLog` и
`LateCore` (кэш компиляции в `paths.data/cache`), над ним
`DaemonModules` (порт `ModuleRuntime`), `ModuleHooks` (порт `Hooks`) и
`ModuleGit` (порт `Git`) над общими `ModuleRegistry` и `ModuleSet`.

Хост модулей это `crates/wasm` (wasmtime с component model): мир
`wit/liftoff-module.wit` (`liftoff:module@0.1.0`), импорты
`host` через порт `ModuleStore`, `Timer` планировщика и `ModuleLog`.
`wit/deps/` это дословная копия `wit/deps/` крейта `wasmtime-wasi-http`
из `Cargo.lock` (WASI 0.2.12): `just wit-deps` обновляет её,
`just wit-deps-check` (часть `just ci`) падает при расхождении.

Модули и контрактные компоненты хоста живут в отдельном workspace
`modules/` (корневой workspace его исключает), по крейту на модуль, цель
`wasm32-wasip2` из `rust-toolchain.toml`. Компонент собирает нативная
цель (`cargo build --target wasm32-wasip2 --release`, `wit-bindgen`
0.63), без `cargo-component`: он не поддерживается с 2025-03. `just
modules` собирает всё в `modules/target/wasm32-wasip2/release/`
(`probe.wasm` и т.д., вне git); `just test` и `just ci` сначала вызывают
его, а тесты хоста читают компоненты из каталога в
`WASM_MODULES_DIR` (его задаёт justfile), иначе из этого пути по
умолчанию. Без собранного компонента тест падает с подсказкой `just
modules`, а не пропускается. Контрактный компонент `modules/testing/probe`
проверяет MOD-01 и MOD-03, лимиты и trap против настоящего хоста поверх
фейков ядра (`crates/wasm/tests/probe.rs`, `InMemoryStore`), а MOD-02
через хост поверх SQLite в корне композиции
(`crates/liftoffd/tests/modules.rs`): адаптеры `wasm` и `store` друг от
друга не зависят даже в dev-зависимостях. `just lint`, `just doc` и
`just deny` проверяют и workspace `modules/` (clippy и rustdoc под
`wasm32-wasip2`, cargo-deny с корневым `deny.toml`).

Импорты `host` в: `store-get`/`store-set` (своё пространство
ключей, таблица `module_keys`; сбой хранилища прерывает вызов как
сбой хоста, см. ниже),
`store-query` (ниже), `config-get` (конфиг, с которым модуль загружен),
`schedule`, `log` (в `ModuleLog` с именем модуля; в демоне `tracing`;
управляющие символы, в том числе перевод строки, экранируются, чтобы
модуль не подделал строки журнала демона; строка не длиннее
`MAX_LOG_BYTES` (4 КиБ, дальше обрезка с `…`), не больше
`MAX_LOG_LINES_PER_CALL` (100) строк за один вызов модуля, остальные
отбрасываются с одним предупреждением).
Остальное: `core-call` идёт через `CoreCalls`
хоста (у демона `CoreBridge` ядра, таблица ниже; без него ответ
`unavailable`), `wasi:http` проверяет `http_allow` (ниже), каждый вызов
ограничен `call_timeout`. Остальной WASI 0.2 (часы, random, stdio, их
тянет `std`) подключён без каталогов, сокетов и DNS. Лимит памяти
(`Limits::max_memory_bytes`) относится ко всему модулю: `StoreLimits`
разрешает одну линейную память, до 32 core-инстансов и таблиц и 100 000
элементов таблиц. Ключ `store-get`/`store-set` не длиннее
`MAX_KEY_BYTES` (256 байт), значение не больше `MAX_VALUE_BYTES` (1 МиБ);
больше это trap. Квоты модуля на число и объём ключей и строк его
таблиц целиком нет (реестр её не ввёл); ограничены только
отдельное значение, оператор `store-query` и его ответ.

Что модуль заставляет держать хост вне своей памяти, тоже ограничено
(MOD-10): не больше `MAX_HOST_RESOURCES` (1024) дескрипторов в
`ResourceTable` инстанса (потоки, pollable, поля, запросы и ответы
`wasi:http`; следующий это trap вызова), не больше
`MAX_HTTP_FIELDS_BYTES` (64 КиБ) в одном наборе полей `wasi:http` и не
больше `MAX_HTTP_REQUESTS_IN_FLIGHT` (16) открытых запросов на инстанс
(следующий получает `connection-limit-reached`; место освобождается,
когда соединение закрыто или модуль бросил запрос).

`call_timeout` (`[modules]`, больше 0 и не больше 1 ч;
`max_memory_mb` от 1 до 4096, иначе демон не стартует с ошибкой ключа)
работает через epoch: поток движка увеличивает epoch каждые 10 мс,
идущий вызов на каждом тике уступает исполнитель, а после дедлайна
прерывается (`ModuleError::Timeout`, для ядра `Internal`). Epoch
прерывает только код wasm, а не импорт, который ждёт хост (SQLite,
HTTP-сервер, который не отвечает), поэтому каждый вызов ещё обёрнут в
`tokio::time::timeout` на `call_timeout` + 1 с (`HOST_WAIT_GRACE`), после
чего он тоже `Timeout`; у `store-query` свой лимит времени (ниже).
Контракт проверяет оба пути по времени: бесконечный цикл кончается
раньше, чем сработал бы запас хоста, а запрос к молчащему серверу
кончается вскоре после `call_timeout`.

Инстанс на время вызова вынимается из слота модуля и возвращается
только после ответа. Вызов, чей future брошен на полпути (клиент REST
ушёл), уносит инстанс с собой: следующий вызов идёт в новом инстансе, а
не в госте, остановленном посередине (wasmtime такой инстанс не
пускает, `cannot enter component instance`), и брошенный вызов ничего
не засчитывает в MOD-09.

#### `core-call` (MOD-11)

`CoreBridge` (`crates/core/src/module/core_call.rs`) держит таблицу
методов (`CoreBridge::METHODS`); метод не из таблицы это `Forbidden` до
того, как запущен какой-либо сервис. Разрешённый метод запускает сервис
ядра от актора текущего вызова модуля, и сервис спрашивает `authorize`,
как для любого другого вызывающего. Вызов без актора (`tick`, `migrate`,
инструмент без актора) это `Unauthenticated`, `authorize` не
спрашивается. Аргументы и ответы JSON; аргументы проверяются на границе
(`deny_unknown_fields`, `ProjectName::new`, `EnvName::new`), иначе
`Validation`. Отказ ядра доходит до модуля как его `host.error` с кодом
варианта (`forbidden`, `validation`, ...), внутренняя ошибка как
`internal` без источника.

| Метод | `args` | Ответ | Действие `authorize` |
|---|---|---|---|
| `projects.list` | `{}` | `[project]` | `ProjectList` |
| `projects.get` | `{"project"}` | `project` | `ProjectGet` |
| `environments.list` | `{"project"}` | `[environment]` | `EnvironmentList` |
| `environments.get` | `{"project","env"}` | `environment` | `EnvironmentGet` |
| `secrets.resolve` | `{"project","env"}` | `{"KEY": "value"}` | `SecretResolve` |
| `auth.create_session` | `{"issuer","subject","login","email","role","name","kind"}` | `{"token","user"}` | нет (без актора, см. ниже) |
| `system.api_url` | `{}` | `{"url"}` (`https://<api host>`) | нет (без актора; колбэк входа `oidc`) |
| `environments.set_protected` | `{"project","env","protected"}` | `environment` | нет (только `tenants`, см. «Модуль `tenants`») |
| `tenancy.users`, `tenancy.user`, `tenancy.tenant_of`, `tenancy.projects`, `tenancy.environment`, `tenancy.assign`, `tenancy.adopt` | см. rustdoc `CoreBridge::METHODS` | | нет (только `tenants`) |

`CoreCalls::call` получает `Caller`: модуль, `replaces` его манифеста и
актора. Методы без актора это `system.api_url` (публичный адрес API) и
`auth.create_session`: конец входа, который модуль провёл на своих публичных маршрутах,
где актора ещё нет. Его может вызвать только модуль, чей манифест
объявляет `replaces: ["login"]` (`Caller::logs_in`), иначе `Forbidden`;
хост берёт `replaces` из манифеста при загрузке. Метод проверяет
аргументы на границе (`OidcIssuer`, `Login`, `UserRole`, `SessionKind`,
`subject` от 1 до 255 байт) и вызывает `AuthService::open_session`:
пользователь `(oidc:<issuer>, subject)` создаётся или получает логин,
почту и роль из нового входа (`Store::user_by_subject`, уникальность
`(source, subject)` в контракте хранилища и индексом
`users_source_subject`), и открывается новая сессия `cli` (30 дней с
продлением) или `browser` (24 часа); `mcp` отказ. Роль решает модуль
(по claim провайдера), ядро её только хранит. `OidcIssuer` всегда
`https://`; фейковый провайдер тестов (HTTP на loopback) проходит только
через `OidcIssuer::loopback` и `CoreBridge::allowing_loopback_issuers`,
которые есть лишь с фичей `test-support`.

Модуль, заменяющий `login`, один. `ModuleService` отказывает (`Conflict`)
установке, включению и смене конфига модуля, который заменяет команду,
уже заменённую другим установленным модулем; что заменяет другой модуль,
берётся из его загруженного манифеста, а если он не загружен (упал при
старте), из сохранённого компонента; нечитаемый компонент тоже отказ.
При установке проверка повторяется по манифесту, с которым модуль
загрузился (его `replaces` хост берёт именно оттуда, с конфигом), и
ловит заодно установку, занявшую строку параллельно. Сам
`auth.create_session` при каждом вызове отказывает (`Forbidden`), пока в
реестре есть другой модуль с `replaces: ["login"]`: так ни старт демона,
ни манифест, зависящий от конфига, не оставят двух модулей, выпускающих
сессии.

В демоне `core-call` идёт через `LateCore` (`crates/liftoffd/src/modules.rs`):
хост модулей входит в порты, поэтому он строится над непривязанным
`LateCore`, а корень композиции привязывает `CoreBridge` над портами
(через `Weak`), когда они созданы, как `LateRunner` для деплоев.

`project` это `id`, `name`, `tenant_id`, `git_host`, `repo`,
`created_by`, `created_at`; `environment` это `id`, `project`, `name`,
`kind`, `state`, `visibility`, `branch`, `owner_user_id`, `created_at`.
Времена в RFC 3339, как в REST (`liftoff_api::ProjectView`,
`EnvironmentView`). JSON строит само ядро (`project_json`,
`environment_json` в `core_call.rs`), отдельно от видов `liftoff-api`, но
в том же формате: `core` от `api` не зависит, а общий источник формата
для `core-call` и REST (зависимость `wasm → api` или общие
преобразования вне `rest`) не выбран. Вопрос открыт.

Повторный вход. `core-call` модуля выполняется внутри его вызова, пока
тот держит мьютекс модуля, а сервисы ядра спрашивают `authorize`, на
который подписываются модули (`oidc`, `tenants`). Правило:
модуль не получает хуки, поднятые его собственным вызовом. Хост держит
task-local цепочку модулей, чей вызов идёт в этой задаче
(`module::calling`); `Module::on_hook` модуля из цепочки сразу отвечает
`Continue` (нет мнения), не входя в модуль и не ожидая его мьютекса, а
другие экспорты такого модуля отвечают `ModuleError::Reentered` (не
засчитывается в MOD-09). Цепочка проходит и через хуки других модулей:
A → `core-call` → хук B → `core-call` B → хук A пропускает A. Два вызова в
разных задачах (A ждёт B, B ждёт A) цепочка не видит; их развязывает
таймаут вызова. Тест контракта `skips_module_when_its_own_core_call_raises_hook`
поднимает хук из `core-call` в тот же модуль.

#### Кэш компиляции (MOD-12)

`ModuleHost::with_compile_cache(paths.data/cache)` включает собственный
кэш wasmtime (`Config::cache`): ключ это хеш компонента, версия wasmtime
и настройки движка, файлы лежат в `modules/wasmtime-<версия>/`. Свой кэш
(`Component::serialize` в файл и `Component::deserialize` при старте)
невозможен без `unsafe`, а workspace его запрещает (`unsafe_code =
forbid`): wasmtime не проверяет загружаемый машинный код. Поэтому вместо
счётчика фейка из каталога (MOD-12) тесты читают
`ModuleHost::compile_stats` (загрузки минус `Cache::cache_hits`).

Каталог кэша входит в доверенную базу демона, как его бинарник: wasmtime
исполняет найденный там код по ключу, мимо подписи и хешей модулей.
Хост создаёт каталог с правами `0700`, сужает до `0700` свой каталог,
открытый другим, и отказывается стартовать на символической ссылке или
каталоге другого пользователя (`ModuleError::Load`).

Trap (паника модуля, превышение лимита), таймаут и сбой хранилища под
`store-get`/`store-set` проваливают только свой вызов: инстанс после trap
войти нельзя, поэтому `Module` его выбрасывает, и следующий вызов (или
`tick`) идёт в новом инстансе из того же компонента. Ключи, таблицы и
расписание модуля при этом сохраняются. Полная ошибка wasmtime (цепочка
и wasm-backtrace) уходит в журнал демона (`tracing::error!` с именем
модуля); вызывающий получает `ModuleError::Trap { module, reason }`, где
`reason` это только код trap (`wasm trap: …`) или собственное сообщение
хоста (лимит). Сбой хранилища это `ModuleError::HostFailed`: вина демона,
а не модуля.

#### Реестр модулей и отключение (MOD-09)

`liftoff_core::module::ModuleRegistry` держит загруженные модули в
порядке установки: имя, манифест и статус (`enabled`, `error`), и
отвечает, какие маршруты, инструменты, команды и подписчики хуков у
включённых модулей. Правило MOD-09 живёт в нём, а не в адаптере: хост
(`ModuleHost`) регистрирует каждый загруженный модуль и сообщает исход
каждого вызова (`CallOutcome`), а реестр после `FAILURES_TO_DISABLE` (3)
сбоев модуля подряд ставит `enabled = false` с текстом последнего сбоя.
Сбой модуля это trap, таймаут (бесконечный цикл тоже вина модуля) и
новый инстанс, который не инстанцировался после trap. Ответ модуля
(результат или его собственная ошибка) обнуляет счёт; `HostFailed`
(сбой хранилища) не считается и не обнуляет. Одинаково для всех
экспортов: `tick`, `migrate`, `handle-tool`, `handle-route`
(`Module::handle_route`) и `on-hook` (`Module::on_hook`; правило:
трижды подряд trap в хуке). Отключённый модуль не входит в вызов
(`ModuleError::Disabled` → `ModuleRequired`, 424), его маршрут в реестре
отвечает `ModuleRequired`, а хуки, инструменты и команды выпадают из
списков до `ModuleRegistry::enable`. Реестр живёт в памяти, таблица
`modules` переживает рестарт: `ModuleService::sync_status`
(в демоне каждые 30 с) записывает в строку модуль,
который реестр отключил, а `ModuleService::restore` на старте загружает
установленные модули в порядке установки, оставляет отключённые
отключёнными с их ошибкой и помечает упавший при загрузке `enabled =
false` с `error`; демон работает без него до `lof module enable`.

#### Установка модулей и реестр (REG-01…08)

Правила установки живут в `liftoff_core::service::ModuleService` над
двумя портами: `ModuleSource` (реестр: `index.json` и файлы релизов; в
демоне `liftoff_wasm::HttpModuleSource`, адаптер в
`crates/wasm` рядом с загрузкой модулей (до жил в
`crates/remote`), в тестах
`fakes::FakeModuleSource`, который подписывает релизы своим ключом
minisign) и `ModuleRuntime` (хост модулей: `inspect`, `save`/`read`/
`discard` файла `paths.data/modules/<name>-<version>.wasm`, `load` с
миграцией, `unload`, `purge`; в демоне `liftoffd::modules::DaemonModules`
над `ModuleHost` и `ModuleSet`, в тестах `fakes::FakeModuleRuntime`).
wasmtime в ядро не попадает. Формат `index.json` описан в
`crates/core/src/module/registry_remote.rs`; к полям плана (`url`,
`sha256`, `signature`, `wit_version`, `liftoffd_min`) добавлены
`requires` и `config_schema`: зависимости проверяются до скачивания, а CLI спрашивает конфиг по схеме до отправки. Подписан
только `.wasm`; всё остальное в индексе подсказка, которую демон
перепроверяет по манифесту подписанного компонента.

`install` по порядку: `authorize` (`module.install`, только админ); ключ
реестра не закреплён → `Unavailable` (ключ это константа
`liftoff_core::module::REGISTRY_PUBLIC_KEY`, один для демона и CLI, пустая строка значит «не закреплён»,
как `RELEASE_KEY` в `install.sh`; переопределения через конфиг нет);
модуль уже установлен → `Conflict`; выбор релиза (указанный или новейший
с `wit_version` = `WIT_VERSION` и `liftoffd_min` не новее демона; иначе
`Conflict` с обеими версиями, REG-02); зависимости → `ModuleRequired`
(424, «install the module `oidc`», REG-03); скачивание; SHA-256 и
подпись minisign (без legacy-подписей) → `Upstream{registry}` (502,
REG-01); манифест компонента (`inspect`, ничего не регистрирует) должен
назвать тот же модуль и версию, его `requires` проверяются ещё раз;
конфиг по `config_schema` (`jsonschema` без `resolve-http`/
`resolve-file`, так что `$ref` наружу не ходит) → `Validation` с путём
`config.<поле>` (REG-04; текст ошибки без значения, оно может быть
секретом; без схемы модуль конфиг не принимает); поля с `"format":
"secret"` верхнего уровня запечатываются мастер-ключом (`ConfigValue::
Sealed`, в колонке `modules.config` как `{"sealed": …}`, REG-05);
`"format": "secret"` где-либо ещё (вложенный объект, `$defs`,
`anyOf`, `additionalProperties`) парсер манифеста отклоняет с
JSON-указателем на место, иначе такое поле легло бы открытым текстом;
строка `modules` (с SHA-256 проверенного компонента), сохранение файла,
`load` с `migrate(0)` и регистрация маршрутов, инструментов, команд и
хуков без рестарта (REG-06). Строка вставляется до файла: она и есть
заявка на имя, так что из двух одновременных установок проигравшая
ещё ничего не сохранила и не удалит файл победившей (путь файла у них
общий). Сбой после вставки строки убирает строку и файл. Перед каждой
следующей загрузкой (`restore`, `update`) файл сверяется с SHA-256 из
строки: изменённый файл не загружается, `restore` отключает
модуль с этой ошибкой. Каталог `paths.data/modules/` создаётся по
правилу кэша компиляции (`liftoff_wasm::private_dir`: `0700`, владелец
демон, не симлинк). `update` (`PATCH`)
сливает новый конфиг с сохранённым по полям (`null` удаляет поле),
проверяет целиком, запечатывает и загружает модуль заново без миграции;
`enabled: true` снимает ошибку (и загружает модуль, упавший на старте),
`false` отключает; загрузка через `enable` заново проверяет `requires`
(`ModuleRequired`). `remove` отказывает `Conflict`, пока другой
установленный модуль его требует (по манифесту в реестре, а у модуля,
не загрузившегося на старте, по манифесту его сохранённого файла); иначе `unload`, удаление файла,
`purge` только с `--purge` (`ModuleStore::module_purge`: таблицы
`m_<name>_*` в одной транзакции с `defer_foreign_keys` и ключи модуля,
REG-07) и удаление строки.

`HttpModuleSource` понимает `[modules] registry` двояко: URL, который
кончается на `.json`, это сам индекс; иначе это API релизов Forgejo
(значение по умолчанию), и индекс это ассет `index.json`
последнего релиза. `modules/registry/release.sh` подписывает один релиз
модуля (`minisign -S`), считает SHA-256 и дописывает его в `index.json`
предыдущего релиза, отказывая, если эта версия там уже есть; манифест
модуля читается из самого компонента (пример `module-manifest` крейта
`liftoff-wasm`: `ModuleHost::inspect` демона с его лимитами
`[modules]` по умолчанию и его импортами, хранилище ничего не хранит, так
что компонент принимается ровно тогда, когда его принял бы `liftoffd`),
`liftoffd_min` из `[package.metadata.liftoff]` его `Cargo.toml`,
`wit_version` из строки `package` в `wit/liftoff-module.wit`. Релиз
делает `.github/workflows/modules.yml` по тегу `modules/<крейт>/vX.Y.Z`
(крейт это каталог под `modules/`): сборка под `wasm32-wasip2`, подпись
ключом из секрета `MODULES_SIGNING_KEY`, релиз `<крейт>-vX.Y.Z` в
`<owner>/liftoff-modules` с `.wasm`, `.minisig` и новым `index.json`
(индекс последнего релиза плюс эта версия; без доступа к нему релиз
останавливается). Релиз остаётся черновиком, пока загружаются его ассеты,
и публикуется последним шагом (`gh release create --draft` и
`gh release edit --draft=false`):
`releases/latest` никогда не указывает на релиз без `index.json`, а
оборвавшийся прогон оставляет черновик, который подхватит следующий. `modules/registry/check.sh` (часть `just ci`)
прогоняет то же для всех модулей с одноразовым ключом. Ответы
больше 64 МиБ отвергаются.

CLI (`crates/liftoff`): `lof module ls [--available]|install|remove
[--purge]|enable|config`. `install` читает `GET /modules/available` и
`GET /modules`, отказывает до отправки, если зависимость не установлена,
принимает `--set k=v` и `--set k=@file` (содержимое без последнего
перевода строки), типизирует значения по схеме (поле `array` или
`object` из JSON, массив строк ещё и `a,b`) и спрашивает на
терминале обязательные поля (вопрос это `description`, секретные без
эха), а в пакетном режиме перечисляет их как `--set <поле>` (64).
Слово, которое clap не знает как команду, попадает в `crate::dynamic`:
он читает `GET /modules` и строит дерево clap включённого модуля с этим
именем (`--<арг>` по типу: `integer` число, `boolean` флаг, плюс
`--json` и `--no-input`); обязательный аргумент без значения
спрашивается с `hint` манифеста или перечисляется (64), затем `POST
/api/v1/m/<модуль>/<команда>` с аргументами JSON-объектом, ответ
печатается как пришёл, а с `--json` отказ печатается объектом
problem в stdout, как у встроенных команд (REG-08). Поэтому неизвестная
команда стоит одного `GET /modules`; без сессии или без связи с
сервером она всё равно ошибка использования (64) с причиной, по которой
команды модулей не удалось узнать. Команды модуля стоят на верхнем
уровне (`lof protect`) и могут состоять из нескольких слов
(конец раздела «Модуль `tenants`»).

#### Диспетчер хуков и маршруты модулей (HOOK-01…06)

`liftoff_core::module::ModuleHooks<M>` реализует порт `Hooks` над
`ModuleRegistry` и `M: ModuleCalls` (вызов одного модуля; в демоне это
`liftoff_wasm::ModuleSet`, в тестах `fakes::FakeModules`). Правила живут
в ядре, адаптер только переводит в WIT и обратно:

- хуки спрашивают включённых подписчиков в порядке установки; первый
  `Deny` побеждает и останавливает обход, `Allow` не мешает следующему
  модулю отказать, без отказа любой `Allow` даёт `Allow`,
  иначе `Continue`;
- сбой модуля в решающем хуке (`authorize`, `before_deploy`, `gate`) это
  отказ «module X failed to answer» (сломанный `tenants` не открывает
  чужой проект); в уведомлениях (`after_deploy`, `domain_*`) сбой
  пишется в журнал и пропускается; после трёх сбоев подряд модуль
  выключен (MOD-09) и его хуки не зовутся (HOOK-05);
- `authenticate`: первый модуль, назвавший актора или отказавший,
  решает, и bearer тогда не проверяется (HOOK-02); сбой пропускается, и
  ядро проверяет сессию;
- `render_page(kind)` (`login`, `forbidden`, `unavailable`, `offline`):
  первый ответ модуля (HTML или,, редирект: панель
  `liftoff-web` модуля `web`) заменяет встроенную страницу (HOOK-04);
  сбой модуля пропускается ради следующего. Встроенные HTML-страницы входа,
  403 и «офлайн» и их подмена через `render_page` появляются 
  (`crates/rest/src/pages/`); 503 домена без окружения — статический
  ответ Caddy (`crates/proxy`), 503 функции, которая не запущена, —
  текст listener'а или `render_page(unavailable)`;
- маршрут модуля сопоставляется по пути без строки запроса, модуль
  получает путь со строкой запроса (OIDC-колбэку нужны `code` и
  `state`); сегмент `{name}` в объявленном пути совпадает с любым
  непустым сегментом запроса.

`variant hook-result` не несёт данных, поэтому `authenticate`
и `render_page` хост зовёт через `handle-route` без актора на
зарезервированных путях `POST /.hook/authenticate` (тело: запрос как
JSON `{method, path, headers}`; `200` с ID пользователя это актор, `401`
или `403` с причиной отказ, иначе `Continue`) и `GET
/.hook/render_page/<kind>?host=&return=` (`200` с HTML, `302` с
`location` — редирект, иначе страницы нет; редирект ядро принимает
только от модуля `web` и только на origin его панели, `web`
спрашивается первым). Манифест
не может объявить маршрут ниже `/.hook` (`RouteSpec::RESERVED`), а
`RouteSpec::matches` не сопоставляет с маршрутом ни один запрос, первый
сегмент которого `.hook` (без учёта регистра и с `%2E` как `.`), даже
маршрут с параметром вроде `/{a}/{b}`: так что снаружи эти вызовы
недостижимы, `/api/v1/m/<module>/.hook/…` всегда 404. JSON-полезные нагрузки `on-hook` описаны
в rustdoc `ModuleSet`.

`/api/v1/m/{module}/{*path}` (любой метод) стоит вне bearer-слоя:
`ModuleRouteService` ищет маршрут через `Hooks::module_route` (404 для
неизвестного модуля или маршрута, HOOK-06; 424 для выключенного,
HOOK-05), маршрут `public` зовёт `handle-route` без актора (OIDC-колбэк,
страница входа), любой другой аутентифицирует запрос как все маршруты
(`AuthService::authenticate`, 401 без сессии, HOOK-03) и зовёт модуль от
имени актора. Маршрут сопоставляется по пути без query, сегмент `{id}`
совпадает с любым одним сегментом; модуль видит путь с query и те же
заголовки, что хуки (без учётных данных liftoff и cookie). Демон
собирает `ModuleHooks` над `ModuleSet` своего хоста: туда модули кладут
установка (`ModuleService::install`) и восстановление на старте
(`ModuleService::restore`).

#### Манифест и `wasi:http` (MOD-06, MOD-07)

`ModuleManifest::parse` проверяет значения на границе, домен их больше
не проверяет: `version` это semver, маршрут это известный метод и путь
под префиксом модуля (сегменты без пустых, `.` и `..`, без `?` и `#`),
имена команд, аргументов, инструментов и `replaces` это строчные слова,
хосты `git_provider` это хосты без порта, схемы это объект или boolean
JSON Schema (компиляция схемы через `jsonschema` и проверка конфига по ней это установка). Запись `http_allow` это `HttpAllow`:
DNS-имя или IP-литерал, с портом или без; `*`, схема, путь и пустая
запись это отказ. Запись без порта разрешает только порт схемы запроса
(80 для `http`, 443 для `https`), с портом только этот порт.

Хук `wasi:http` (`crates/wasm/src/http.rs`) пропускает запрос, только
если его хост и порт есть в `http_allow`, иначе `HTTP-request-denied`
до соединения. Кроме того, по умолчанию хост разрешается в адреса один
раз, и если хоть один не публичный (loopback, частные сети, link-local,
`100.64.0.0/10`, unspecified, multicast, ULA `fc00::/7`, их
IPv4-mapped формы), запрос отклоняется с `destination-IP-prohibited`:
REST демона и admin API Caddy слушают loopback, и модуль не
должен до них дотянуться ни IP-литералом, ни своим DNS-именем.
`ModuleHost::allowing_private_networks` снимает это ограничение (тесты
контракта, установки со службами в частной сети).
`ModuleHost::routing_https_to(authority, addr)` (только фича
`test-support`) отправляет запросы модуля к `https://<authority>` по
обычному HTTP на `addr` без разрешения имени и проверки адреса;
`http_allow` проверяется по-прежнему по authority запроса. Так тесты
`dns.cloudflare` ставят фейк Cloudflare на loopback, хотя `http_allow`
модуля зафиксирован как `api.cloudflare.com`.

Разрешённый запрос отправляет свой клиент (`crates/wasm/src/send.rs`),
а не `default_send_request` из `wasmtime-wasi-http` (фича
`default-send-request` выключена): тот соединяется по имени из URI и
разрешает его второй раз, и DNS-сервер с rebinding и TTL 0 мог бы
ответить проверке публичным адресом, а соединению loopback. Свой клиент
это тот же код, но TCP идёт на проверенные адреса, TLS (`rustls`, корни
`webpki-roots`) проверяет сертификат по хосту запроса, HTTP/1.1 через
`hyper`, те же таймауты. Зависимости (`hyper`, `rustls`,
`tokio-rustls`, `webpki-roots`, `http-body`, `bytes`) те же, что тянул
`wasmtime-wasi-http`, новых крейтов в дереве нет. Тест
`connects_to_checked_address_when_name_would_resolve_elsewhere`
подставляет резолвер, который второй раз ответил бы другим адресом.

Планировщик: одна tokio-задача на модуль (`scheduler::run`), живёт, пока
жив `Module`. Импорт `schedule` меняет таблицу «имя → период» модуля
(`Schedules`) на месте и будит задачу; канала с очередью запросов нет.
На каждое имя у задачи ровно один ожидающий сон на `Timer`: когда он
кончается, задача вызывает `tick(name)`, ждёт его конца и только потом
начинает следующий период, так что вызовы одного имени не пересекаются и
не копятся, пропущенные периоды не догоняются. Все вызовы одного
инстанса (инструменты, хуки, `tick`) идут по одному через его мьютекс.
Повторный `schedule` того же имени заменяет период (старый сон
прерывается), `every-secs = 0` снимает расписание. Не больше
`MAX_SCHEDULES` (32) имён на модуль, имя не длиннее 64 байт, период не
короче `MIN_PERIOD_SECS` (10 с); остальное игнорируется с
предупреждением в журнале (у `schedule` в WIT нет ошибки).

#### `store-query` и авторизатор SQLite

`SqliteStore::module_query` (`crates/store/src/module_store.rs`) ставит
на время одного оператора `Connection::authorizer` и снимает его до того,
как соединением снова пользуется ядро. Разрешено только в базе `main`:
чтение, вставка, изменение и удаление в таблицах модуля
(`ModuleName::owns_table`: префикс `ModuleName::table_prefix`, то есть
`m_` + имя с `.` → `_` + `_`, и одно слово из строчных латинских букв и
цифр: `m_tenants_memberships`, `m_dns_cloudflare_records`), `CREATE`/`DROP
TABLE`, `CREATE`/`DROP INDEX` там же (имя индекса по тому же правилу,
плюс `sqlite_autoindex_*`, который SQLite сам создаёт для ключа своей
таблицы), `ALTER TABLE` своей таблицы,
`REINDEX` своего индекса, `SELECT`, функции, рекурсивные CTE. Отказ
(ошибка модулю с кодом `forbidden`, демон живёт) на всё остальное, и
явно на то, что авторизатор видит отдельным действием: `ATTACH`,
`DETACH`, любой `PRAGMA` (и читающий), транзакции и savepoint
(соединение общее с ядром), временные объекты, представления, триггеры,
виртуальные таблицы (и табличные функции `pragma_*`), `ANALYZE`, чтение
`sqlite_master` и `sqlite_sequence` (общая для всех `AUTOINCREMENT`,
поэтому модули используют `INTEGER PRIMARY KEY` без него).

Чего авторизатор не видит, и как это закрыто:

- SQLite сам пишет и читает `sqlite_master` вложенными операторами при
  `CREATE`/`DROP`/`ALTER`, и авторизатор не отличает их от SQL модуля.
  Поэтому запись схемных таблиц разрешена (SQL модуля писать их не может:
  `writable_schema` это `PRAGMA`), а чтение только в операторе, который
  меняет схему. Единственный такой оператор с подзапросом модуля это
  `CREATE TABLE … AS SELECT`; он запрещён (сначала `CREATE TABLE`, потом
  `INSERT … SELECT`).
- Новое имя в `ALTER TABLE … RENAME TO` не аргумент авторизатора.
  Оператор, меняющий схему, идёт внутри savepoint; если после него
  появился или исчез объект схемы не модуля, savepoint откатывается и
  модуль получает `forbidden`.
- `REFERENCES` тоже не действие авторизатора, а внешний ключ из таблицы
  модуля на таблицу ядра ломает ядро: SQLite отказывает в `DELETE` из
  родителя с «foreign key mismatch» (например, `sessions` при выходе).
  Поэтому в том же savepoint после изменения схемы проверяется
  `pragma_foreign_key_list` каждой таблицы модуля: родитель, который не
  таблица модуля (без учёта регистра, как сравнивает SQLite), даёт откат
  и `forbidden`.
- Один оператор на вызов: rusqlite узнаёт второй оператор, только
  подготовив его, поэтому `;` и хвост проверяются до установки
  авторизатора (`validation`).
- `VACUUM` (и `VACUUM INTO`, который записал бы копию всей базы в
  файл) авторизатор не спрашивает вовсе. Оператор, первое ключевое слово
  которого (после пробелов и комментариев) `VACUUM`, получает `forbidden`
  до подготовки, не полагаясь на то, что внутри savepoint `VACUUM`
  невозможен.
- Отпускание (`RELEASE`) внешнего savepoint это фиксация, и отложенный
  внешний ключ (`DEFERRABLE INITIALLY DEFERRED`) между своими таблицами
  модуля проверяется только в этот момент. Если `RELEASE` не прошёл,
  SQLite оставляет транзакцию открытой, и все следующие записи ядра
  (отзыв сессий, токены, секреты) не были бы зафиксированы. Поэтому
  неудачный `RELEASE` откатывается (`ROLLBACK TO`, затем `RELEASE`, в
  крайнем случае `ROLLBACK`), модуль получает `validation`, а после
  каждого `store-query` соединение снова в autocommit (иначе `internal`).
- Сообщение SQLite о чужом имени выдало бы, какие объекты ядра есть
  («no such table: x» против отказа на `users`). Поэтому отказ
  авторизатора на таблицу, индекс или колонку не называет объект, а
  сообщения SQLite «no such table/index/view/trigger», «… already exists»
  и «there is already another table or index» об имени вне префикса
  модуля превращаются в тот же `forbidden`. «no such column» SQLite
  выдаёт без таблицы, поэтому это всегда `forbidden` (и для опечатки в
  колонке своей таблицы); отсутствующая своя таблица остаётся
  `validation` с текстом SQLite.

Ответ это JSON-массив объектов по именам колонок: `NULL` → `null`,
целые и вещественные → числа, текст → строка, blob → строка base64.
Ошибки: `forbidden`, `validation` (SQL не разобран, несколько операторов,
нарушено ограничение, превышен лимит), остальное `internal` без
подробностей (они в журнале демона).

Лимиты (`ModuleQueryLimits`, по умолчанию 2 с и 4 МиБ; демон задаёт их
через `SqliteStore::with_module_query_limits`): оператор модуля держит
единственное соединение демона, поэтому на время оператора ставится
progress handler, который прерывает его по истечении времени, и
`SQLITE_LIMIT_LENGTH`, ограничивающий любое вычисляемое значение, и
`SQLITE_LIMIT_COLUMN` = `MAX_MODULE_COLUMNS` (64; колонки результата,
таблицы и индекса), чтобы одна строка не держала тысячи значений сразу.
Размер ответа считается значение за значением до их преобразования в
JSON (blob в base64 с запасом 4/3), так что даже одна широкая строка не
собирается целиком сверх лимита. Всё это `validation` модулю;
авторизатор, progress handler и лимиты снимаются до того, как
соединением снова пользуется ядро.

Пространства имён не пересекаются: одно имя может начинать другое
(`dns` и `dns.cloudflare`, префиксы `m_dns_` и `m_dns_cloudflare_`), но
после префикса стоит одно слово без `_`, поэтому `m_dns_cloudflare_records`
принадлежит только `dns.cloudflare`, а `m_dns_cloudflare` только `dns`.

#### Модуль `dns.cloudflare` (DNS-01…05)

`modules/dns-cloudflare` переносит Cloudflare-клиент v1 и
резервирование записей v1 в хуки доменов. Конфиг: `zone_id`, `api_token`
(секрет) и `ip`, адрес A-записей (публичный IP инсталляции: полезная
нагрузка хука не несёт настроек ядра, поэтому адрес задаётся в конфиге
модуля). `ip` проверяется на границе: схема конфига требует IPv4
(`pattern`, `module configure` отвергает иное), и модуль разбирает его
ещё раз перед каждым вызовом Cloudflare. Таблица `m_dns_cloudflare_records (host, domain_id,
cf_record_id)` хранит только записи, созданные модулем.

- `domain_created` (полезная нагрузка `{"domain":{"id","host","kind",
  "proxied"}}`): только `managed`; `GET dns_records?name=<host>`, чужая
  запись это `Deny` «taken outside liftoff» (DNS-02), своя запись,
  пережившая неудачное удаление, берётся снова; иначе `POST` A-записи и
  её id в таблицу (DNS-01); если id записать не удалось, запись
  удаляется и хук отказывает (компенсация, DNS-03). Любой сбой модуля в
  этом хуке тоже `Deny`: сбой уведомления ядро только пишет в журнал, и
  домен остался бы без записи. На `Deny` ядро забирает домен обратно.
  Поэтому сбой Cloudflare или неверный токен при `lof domain add` это
  `403` с текстом Cloudflare, а не `502`: у хука нет другого ответа,
  который остановил бы ядро. Это сознательный выбор; типизированный
  «upstream» в ответе хука не вводится, пока его не потребует второй
  модуль.
- `lof domain add <label> --proxied` (`POST /domains` с `"proxied":
  true`): ядро до записи строки спрашивает `Hooks::module_available
  (dns.cloudflare)` (нет или выключен: `ModuleRequired`), кладёт флаг в
  строку, и модуль создаёт запись сразу с `proxied` (DNS-01). Для
  `--external` флаг это `Validation`.
- Если `domain_created` отклоняет модуль, стоящий после
  `dns.cloudflare`, ядро после возврата строки и прокси шлёт
  `domain_removed`: модули, которые уже ответили `Continue`, откатывают
  своё, `dns.cloudflare` удаляет только что созданную запись.
  Модуль обязан принимать `domain_removed` домена, о котором не знает.
- `domain_removed`: `DELETE` по сохранённому id (404 значит уже удалена),
  затем строка таблицы; домен без сохранённого id не трогается (DNS-04).
- `lof domain set --proxied` идёт в `DomainService::set_proxied`: после
  `authorize` ядро зовёт `Hooks::handle_route(dns.cloudflare, actor,
  PATCH /records/<host> {"proxied"})`, модуль делает `PATCH
  dns_records/<id>`, ядро сохраняет `domains.proxied` (DNS-05).
  Отсутствующий или выключенный модуль это `ModuleRequired`, `404`
  модуля (домен без записи) это `Validation`, прочие ответы это
  `Upstream` с полем `detail` problem-ответа модуля (тело целиком
  только в журнал). Если после `PATCH` записи не удалось сохранить
  строку, ядро возвращает записи прежний флаг тем же маршрутом. Маршрут не объявлен в манифесте, поэтому
  `/api/v1/m/dns.cloudflare/...` до него не доходит (404): иначе любой
  пользователь с сессией менял бы флаг чужого домена мимо проверки
  владельца, и флаг в ядре разошёлся бы с записью.

#### Модуль `git.forgejo` (GITF-01…06)

Forgejo ушёл из ядра: крейт `crates/remote` удалён, порт `Git` в демоне
обслуживает `liftoff_core::module::ModuleGit` над `ModuleSet` хоста
модулей. Ядро по-прежнему хранит `git_connections` и само ведёт `lof git
connect forgejo --token` (`GitService::connect`); проверка токена идёт
через модуль.

- Выбор модуля: только модуль `git.<provider>` подключения, и только
  если `git_provider.hosts` его манифеста содержит хост подключения (без
  учёта регистра). Модуль с другим именем, заявивший тот же хост, вызова
  (и токена пользователя) не получает при любом порядке установки, так
  что два модуля на одном хосте не конфликтуют. Не установленный
  `git.<provider>` это `ModuleRequired` с его именем; хост, которого он
  не обслуживает, это `Validation` (GITF-06); выключенный, но
  обслуживающий хост, это `ModuleRequired` (MOD-09). До модуля вызов в
  этих случаях не доходит. Манифест не запрещает `git_provider` модулю
  с другим именем (его объявляет контрактный компонент `allimports`):
  такое объявление просто ни на что не влияет.
- Каждый метод порта это один `handle-tool` модуля без актора:
  `validate_token` (`account`), `repo_role`, `image_exists`,
  `put_ci_secret`, `list_templates`, `generate_from_template` (последние
  два для каталога и `lof create`). В аргументах JSON хост, логин
  и токен подключения открытым текстом: сервис расшифровал его только
  для этого вызова, модуль ничего не хранит (таблиц у него нет). Таблица
  аргументов и ответов в rustdoc `ModuleGit`. Эти инструменты не
  объявлены в `tools` манифеста, поэтому в MCP не попадают: только ядро
  может передать модулю чужой токен.
- Ошибки модуля идут по коду (`forbidden`, `validation`, `not_found`,
  `conflict`); новый код `upstream` хост переводит в `Upstream` системы
  `module` (502), а `ModuleGit` уточняет систему до `forge`. Модуль
  повторяет идемпотентный запрос (`GET`, `PUT`), ответивший `5xx`, один
  раз, затем `upstream` (GITF-04); сбой транспорта не повторяется.
  `POST /repos/{template}/generate` не повторяется: `5xx` (например,
  таймаут прокси) мог прийти уже после создания репозитория, и повтор
  получил бы `409`, то есть ложный `conflict`.
- `NewRepo` несёт проверенный `RepoRef` (`owner/name`): сырой ввод `lof
  create` становится доменным типом в адаптере, который его
  принял.
- `401` на сохранённом подключении (токен отозван или истёк) это
  `Forbidden` с подсказкой `lof git connect forgejo --host <host>`
  (GITF-05). Подключение с `expires_at` в прошлом (OAuth) ядро
  отклоняет с той же подсказкой ещё до модуля (`open_connection`).
- Модуль: конфиг `hosts` (JSON-массив хостов Forgejo); манифест объявляет
  их и в `git_provider` (`oauth: false`, только токены), и в
  `http_allow`; API хоста `https://<host>/api/v1`, всегда
  `Authorization: token <credential>` без `Sudo`. Манифест читается при
  загрузке, поэтому новый `hosts` действует после перезагрузки модуля.
  Тело запроса пишется после `outgoing-handler.handle`: хост читает тело
  только после отправки запроса, и тело длиннее одного куска записи
  (4096 байт, длинный CI-секрет) иначе не ушло бы.
- `DnsResolver` переехал в `liftoff-host` (вид хоста на публичный DNS;
  в таблице крейтов нет сетевого крейта, а CLI от `host` не
  зависит), `HttpModuleSource` в `liftoff-wasm` (откуда берутся
  загружаемые модули; CLI берёт его оттуда). Адаптеры
  по-прежнему не зависят друг от друга.
- Тесты: `crates/wasm/tests/git_forgejo.rs` грузит собранный
  `git_forgejo.wasm` (`just modules`, путь из `WASM_MODULES_DIR`),
  направляет `https://git.example.com` на фейковый API Forgejo на loopback
  (`ModuleHost::routing_https_to`) и гоняет через `ModuleGit` и сервисы
  ядра (`fakes::WithGit`) случаи DEP-07 и ENV-04; правила выбора
  модуля проверяются в `crates/core/src/module/git.rs` над
  `FakeModules`.

#### Модуль `git.github` (GIT-01…08)

Второй модуль порта `Git`, по образцу `git.forgejo`: обслуживает только
`github.com` (GitHub Enterprise не его), всегда от имени подключения
пользователя (`Authorization: Bearer <credential>`, `User-Agent`,
`X-GitHub-Api-Version`). `http_allow`: `api.github.com`, `github.com`,
`ghcr.io`.

- Конфиг `client_id` (секрет, необязателен): OAuth-приложение для device
  flow. С ним манифест объявляет `git_provider.oauth: true`, без него
  `false`. Порт `Git` получил `device_start` и `device_poll`; `ModuleGit`
  зовёт их только у модуля с `oauth: true`, иначе сам отвечает
  `Validation` с подсказкой `--token` (GIT-01), до модуля вызов не
  доходит. `GitService::device_poll` проверяет выданный токен тем же
  `account`, что и вставленный, и хранит подключение с видом `oauth`.
  REST: `POST /git/connections/{host}/device` `{provider}` (200, коды) и
  `…/device/poll` `{provider, device_code}` (202 `{"status":"pending"}`
  или `{"status":"slow_down","interval"}`, затем 201 `ConnectionView`).
  `device_code` на границе становится `DeviceCode` (непустой, до 512
  видимых ASCII-символов; `Debug` его скрывает: пока поток не завершён,
  код работает как учётные данные). На `slow_down` (RFC 8628 3.5) модуль
  передаёт новый `interval` GitHub, и CLI держит его для всех следующих
  опросов, а без него добавляет 5 секунд. CLI для `github`/`gitlab` без
  `--token` сразу начинает device flow: ответ `400` (у модуля нет OAuth)
  или `404` (сервер без маршрута) значит спросить токен с подсказкой о
  scope, другой отказ выводится как есть. `GET /modules` CLI для этого
  не читает: без модуля `tenants` его может звать только админ, а
  `git.connect` хук открывает и другим. Для Forgejo запросы CLI не
  изменились.
- Проверка токена: `GET /user`; заголовок `X-OAuth-Scopes` должен
  содержать `repo` и `read:packages` (или `write:packages`), иначе
  `Forbidden` с перечнем нужных scope (GIT-02). Токен без заголовка
  (fine-grained, права по репозиториям) принимается: за свои права
  отвечает каждый следующий вызов.
- `image_exists` спрашивает registry, а не packages API: токен на pull
  обменивается в `ghcr.io/token` (Basic `login:token`), затем `HEAD
  /v2/<owner>/<name>/manifests/<tag>`. Packages API разный у
  пользователей и организаций, постраничный и отвечает про
  опубликованное аккаунтом, а не про доступное ему.
- `put_ci_secret`: публичный ключ репозитория, затем libsodium sealed box
  (`crypto_box`, чистый Rust; случайность из `wasi:random` через
  `getrandom` 0.3 и адаптер `rand_core` 0.6). Живая проверка плана
  (сборка под `wasm32-wasip2`) пройдена как проверка сборки `just
  modules`; фейк в GIT-05 открывает ящик закрытым ключом репозитория.
- `generate_from_template`: `POST /repos/{t}/generate` с `owner`, `name`,
  `private`, `include_all: true` (копирует все ветки шаблона); не
  повторяется, как у Forgejo. Идемпотентные запросы и `POST` device flow
  повторяются один раз на `5xx`, затем `Upstream` (GIT-08).
- Тесты: `crates/wasm/tests/git_github.rs` (фейк GitHub на трёх
  loopback-слушателях, `routing_https_to` теперь принимает несколько
  authority), `crates/liftoffd/tests/git_github.rs` (REST над собранным
  компонентом), `crates/liftoff/tests/cli/git.rs`.

#### Модуль `git.gitlab` (GIT-09…14)

Третий модуль порта `Git`, по образцу `git.github`: обслуживает один
GitLab (`gitlab.com` или self-hosted) от имени подключения пользователя
(`Authorization: Bearer <credential>`), REST ниже `/api/v4`, проект
адресуется URL-кодированным путём (`acme%2Fweb`).

- Конфиг `host` (необязателен, по умолчанию `gitlab.com`): модуль кладёт
  его в `git_provider.hosts` и в `http_allow` при загрузке, поэтому
  self-hosted GitLab не требует правки хоста. Ограничение: только HTTPS
  на 443 и только публичный адрес (`http_allow` без порта, приватные сети
  хост модулю не открывает); один экземпляр обслуживает один GitLab.
  Смена `host` или `client_id` действует после перезагрузки модуля
  (`lof module config`).
- Конфиг `client_id` (секрет, необязателен): OAuth-приложение GitLab для
  device flow (`POST /oauth/authorize_device` со `scope=api`, опрос
  `POST /oauth/token`; GitLab ≥ 17.2). Ошибки OAuth GitLab (Doorkeeper)
  приходят с `400`/`401` и телом `{"error"}`; `slow_down` без нового
  `interval`, поэтому CLI добавляет 5 секунд. Токены OAuth GitLab живут
  два часа: модуль передаёт `expires_in` ответа (`{status: "granted",
  token, expires_in}`), порт отдаёт его в `DeviceGrant::Granted {token,
  expires_in}`, и `GitService` хранит `git_connections.expires_at` =
  время подключения + `expires_in` (у токена без срока, как у GitHub,
  `None`). Refresh token модуль отбрасывает, ядро его не хранит: после
  истечения подключение отвечает `Forbidden` с подсказкой `lof git
  connect gitlab` (рекомендуется personal access token; продление —
  решение владельца, см. CHANGELOG).
- Проверка токена: `GET /user`, логин из `username`; `401` и `403
  insufficient_scope` дают `Forbidden` с именем scope `api`. Scope токена
  из `/user` не виден, поэтому токен без `api`, но с `read_user`,
  проходит проверку, а отказывают его следующие вызовы: `403` с телом
  `{"error": "insufficient_scope"}` у `repo_role`, `image_exists`,
  `put_ci_secret`, `list_templates` и fork даёт `Forbidden` с именем
  scope и подсказкой переподключиться, а не тихое «нет доступа» (`none`,
  «образа нет», «нет манифеста»).
- `repo_role`: `GET /projects/{id}`, большее из
  `permissions.project_access` и `permissions.group_access`
  (`access_level`): ≥ 40 (Maintainer, Owner) `admin`, 30 `write`, 20 или
  публичный/внутренний проект `read`, иначе (Guest приватного проекта,
  `404`) `none`.
- `image_exists`: `GET /projects/{id}/registry/repositories?tags=true`
  (первые 100 репозиториев образов); образ `owner/name:tag` есть, когда у
  репозитория с `path` = `owner/name` есть тег `tag`. `location` не
  сравнивается: хост registry у `gitlab.com` и self-hosted разный.
- `put_ci_secret`: `POST /projects/{id}/variables` `{key, value, masked:
  true, protected: true}`; на `400` «has already been taken» `PUT
  /projects/{id}/variables/{key}` с теми же флагами. Нужен Maintainer.
  Ограничения флагов: GitLab маскирует только значение не короче 8
  символов из своего допустимого набора и иное отвергает (`400`), так
  что `lof secrets sync` секрета `ci = true` с таким значением падает с
  `Validation`, где сказано правило; без маски значение не пишется
  (иначе оно попало бы в логи job). Защищённую переменную получают
  только pipeline защищённых веток и тегов: environment с `--branch` на
  незащищённой ветке не увидит `LIFTOFF_TOKEN` в CI, ветку надо
  защитить в GitLab (модуль ветки не защищает).
- `generate_from_template`: fork с переименованием (`lof create`, шаг
  2:`POST /projects/{t}/fork` `{namespace_path, name, path,
  visibility}`), затем `DELETE /projects/{new}/fork`, чтобы проект не
  остался форком шаблона; неудача отвязки пишется в лог модуля, проект
  всё равно возвращается. GitLab копирует fork в фоне, а `lof create`
  сразу клонирует проект, поэтому до отвязки модуль ждёт `import_status`
  `finished` (или `none`): `GET /projects/{new}` раз в секунду, не больше
  шести раз (укладывается в `call_timeout` по умолчанию, 10 с).
  `failed` и копирование дольше ожидания дают `Upstream` с именем
  созданного проекта, отвязка тогда не шлётся. `POST /projects` с `import_url` отвергнут:
  импорт приватного шаблона потребовал бы токен пользователя в URL.
  Fork не повторяется; `409` или «has already been taken» дают
  `Conflict`, `404` `NotFound`. Права и время fork на живом GitLab
  проверяет владелец (CHANGELOG, «Open before the release»).
- `list_templates`: `GET /projects/{id}/repository/files/liftoff.toml/raw?ref=HEAD`,
  `404` значит «нет манифеста».
- Ошибки как у GitHub: `5xx` идемпотентного запроса (и `POST` device flow
  и переменной) повторяется один раз, затем `Upstream`; `401` хранимого
  подключения `Forbidden` с подсказкой `lof git connect gitlab --host
  <host>`.
- Подгруппы (`group/sub/app`) не представимы: `RepoRef` строго
  `owner/name`.
- Не решено (CHANGELOG, «Open before the release»): деплой считает образ
  внутренним, то есть проверяемым через `image_exists` модуля, только
  когда он записан `<git_host>/<owner>/<pkg>:<tag>`
  (`compose::images`). Образы GitLab лежат на отдельном хосте registry
  (`registry.gitlab.com`, у self-hosted свой хост или порт), образы
  GitHub на `ghcr.io`, поэтому деплой классифицирует их как внешние и
  проверку пропускает. Тесты GIT-10 зовут порт
  напрямую. Решение (например, `git_provider.registries` в манифесте и
  классификация по этим хостам) за владельцем.
- Долг: клиент `wasi:http` (`src/http.rs`) скопирован в `git-forgejo`,
  `git-github` и `git-gitlab` с мелкими отличиями (заголовки ответа,
  `DELETE`). До следующего модуля форжа его надо вынести в общую
  библиотеку workspace `modules/`; препятствие — типы `wasi:http`,
  которые `wit-bindgen` генерирует в каждом крейте свои (нужен общий
  крейт привязок и `with:` в `generate!`).
- CLI: `lof git connect gitlab` по умолчанию `gitlab.com` (self-hosted
  через `--host`), подсказка о токене называет scope `api`.
- Тесты: `crates/wasm/tests/git_gitlab.rs` (фейк GitLab на `gitlab.com`
  и `gitlab.example.com`, диспетчеризация между `git.gitlab` и
  `git.github`), `crates/liftoffd/tests/git_gitlab.rs` (REST над
  собранным компонентом), `crates/liftoff/tests/cli/git.rs` (`mod
  gitlab`).

#### Модуль `tenants` (TEN-01…10)

Тенанты, членства и квоты живут в таблицах модуля
(`m_tenants_tenants`: `slug`, `name`, `max_projects`, `env_cpus` в
сотых CPU, `env_memory_mb`, `created_by`; `m_tenants_memberships`:
`tenant_id`, `user_id`, `role`). Ядро хранит `projects.tenant_id`,
`domains.tenant_id` и `environments.protected`; писать их может только
модуль с именем `tenants` через `core-call` (`TenancyService`): эти
методы не спрашивают `authorize`, потому что решает сам модуль, а
`authorize` из его вызова пропустил бы его (повторный вход) и оставил бы
только админа. Миграция `from 0` создаёт таблицы, перенимает
`import_v1_tenants` с их id, членами, проектами и доменами (SQLite даёт
только модулю `tenants` и только чтение таблиц `import_v1_tenant*`) и создаёт
тенант `default` (владелец единственный пользователь, при нескольких
админы), которому достаются все проекты и домены без тенанта; после
импорта `default` остаётся, только если ему что-то досталось.

Хуки модуля: `authorize` (админ всё; списки, `module.list` и свои
подключения форжа всем; проект, environment, домен и области секретов
членам их тенанта, остальным отказ; новый проект в тенанте его членам;
прочее решает ядро), `before_deploy` (отказ environment, чьи лимиты
выше `env_cpus`/`env_memory_mb` тенанта, TEN-05), `gate` (админ и члены
тенанта хоста, TEN-07, TEN-08) и два хука данных. `tenant_quotas`
(`POST /.hook/tenant_quotas` с `{"tenant"}`; `200`
`{"max_projects","env_cpus_hundredths","env_memory_mb"}`, `404` нет
квот): ядро не даёт создать проект сверх `max_projects` и даёт новому
environment лимиты тенанта (TEN-04, TEN-05). `project_tenant`
(`POST /.hook/project_tenant` с `{"actor","tenant"}`, где `tenant` slug
или `null`; `200` `{"tenant"}`): тенант нового проекта, названный slug
(`POST /projects` с полем `tenant`, `lof create --tenant`), иначе
единственный тенант актора (админ без членства получает `default`);
несколько тенантов → `400` с их списком, ни одного → `403`, неизвестный
slug → `404`. Так каждый проект, созданный через REST при установленном
модуле, попадает в тенант, а `authorize` спрашивается о
`Resource::Tenant` (членство). Без модуля названный тенант это `424`.
Оба хука fail closed: модуль, который не ответил (trap, отключён,
непонятный ответ), даёт `Unavailable` той операции, что спрашивала,
а не «квот нет» (журнал пишет `ModuleHooks`, один раз). Хук `gate(host,
request)` получает и актора: сессию по cookie разрешает ядро, модуль
cookie не видит (их нет в заголовках, которые получают хуки).

Флаг `environments.protected` меняет только
`Store::set_environment_protected` (`core-call
environments.set_protected`); `update_environment` его не пишет, так
что запись устаревшей копии environment (завершившийся деплой, `lof
env set`) не снимает гейт. `lof protect` сначала сохраняет флаг, потом
отправляет Caddy конфигурацию, прочитанную из хранилища, как все
остальные изменения маршрутов; отказ Caddy возвращает флаг (v1 SSO-02).
Цели v1 за SSO-гейтом (`sso = 1`) импорт делает `protected` (и
`private`): после переезда они остаются за гейтом.

Аргумент команды модуля типа `project` CLI заполняет, если он не дан,
из глобального `--project` или из репозитория рабочего каталога
(`/projects/-/resolve`), как для встроенных команд: `lof protect --env
prod` в каталоге проекта.

Гейт (правила в `GateListener` ядра, HTTP в адаптере
`crates/rest/src/routes/gate.rs`: он читает `X-Liftoff-Host`, cookie и
строку запроса в доменные типы и рендерит решение `GateAnswer`; ошибки
идут через единственное отображение `ApiError`; гейт v1 SSO): Caddy для домена,
привязанного к запущенному защищённому environment, сначала
спрашивает listener `[gate] bind` (`forward_auth` на `GET /gate/check`
с `X-Liftoff-Host` и `X-Forwarded-Uri`; при `2xx` в запрос копируются
`X-Liftoff-User`, `X-Liftoff-Email`), а `/.liftoff/*` этого хоста
отдаёт гейту целиком. Нет cookie `__Host-liftoff_gate` этого хоста →
302 на `https://<api>/gate/start?host&rd`; `/gate/start` (на API) пускает
только защищённые хосты (иначе 404), `rd` только путь (адаптер
разбирает `rd` и `X-Forwarded-Uri` в `ReturnPath`, всё прочее
становится `/`); без браузерной
сессии (`__Host-liftoff_session`, её ставит колбэк `oidc` для входа с
`rd`) → вход `oidc` с возвратом сюда; хук отказал → 403 «signed in as
…»; иначе одноразовый код (10 минут, одно использование, `gate_codes`)
на `https://<host>/.liftoff/callback`, где он меняется на сессию хоста
(`gate_sessions`, `lfg_…`). Сессия хоста не является сессией API: её
cookie видит приложение за хостом, поэтому bearer из неё не получится.
Коды и сессии хоста удаляются вместе с браузерной сессией (выход
`/.liftoff/logout`, `lof admin sessions revoke`), а просроченные демон
удаляет при старте и каждый час (`GateListener::prune`,
`Store::prune_gate_records`, v1 SSO-13). Каждый `check` снова
спрашивает хук, кэша нет (TEN-08). Listener доверяет заголовкам Caddy,
поэтому `[gate] bind` обязан быть loopback (проверка конфига) и
отвечает только loopback-пирам (`ConnectInfo`, как `tls-ask`).

Вход в браузере привязан к браузеру (login CSRF, v1 SSO-05): `GET
/m/oidc/login` ставит cookie `__Host-liftoff_login_<8 символов state>`
со значением `state` (10 минут); модуль cookie не видит, поэтому
адаптер REST показывает маршрутам модулей значения таких cookie в
заголовке `x-liftoff-login-state` (`ModuleRequest::LOGIN_STATE_HEADER`;
одноимённый заголовок клиента отбрасывается), и колбэк без своего
`state` там отвечает `400`. Модули недоверенные, поэтому
`ModuleRouteService` показывает этот заголовок только модулю
браузерного входа (`ModuleRequest::BROWSER_LOGIN_MODULE`, `oidc`) и
отбрасывает из ответов модулей `Set-Cookie` с именами ядра
(`__Host-liftoff_*`, без учёта регистра); `__Host-liftoff_session` и
`__Host-liftoff_login_*` может ставить только модуль входа. Иначе любой
модуль мог бы навязать браузеру свою сессию, которой доверяет
`/gate/start`. Вход в браузере идёт web-клиентом
(`web_client_id`, `web_client_secret` в конфиге `oidc`, секрет уходит в
обмен кода), если он задан; `redirect_uri` по умолчанию
`https://<api>/api/v1/m/oidc/callback` из `[api] public_url`
(`core-call system.api_url`).

Команды модулей могут состоять из нескольких слов (`tenant members
add`): CLI находит команду по словам на верхнем уровне (и ниже `lof
admin`) и шлёт `POST /api/v1/m/<module>/<слова через />`.

### MCP по stdio (MCP-01…05)

Таблица `liftoff_api::OPERATIONS` (`crates/api/src/schema.rs`) — одна
строка `Operation {name, method, path, summary, status, request, query,
response}` на каждый эндпоинт под `/api/v1`. Из неё строятся и
`/openapi.json` (`crates/rest/src/openapi.rs` берёт `$ref` по имени
`utoipa::ToSchema`; вне таблицы остаются только корневые алиасы
`/healthz`, `/readyz`, `/openapi.json` и `/internal/tls-ask`), и
инструменты MCP (`crates/mcp`, `inputSchema` из `schemars::JsonSchema`
тех же типов). На каждом типе `api` два derive рядом; тесты
`operation_table` в `crates/rest` сверяют поля и `required` схемы
`schemars` с компонентом `utoipa`, а для каждого поля тела и query — его
JSON-типы (с `null` для `Option`) и значения enum. Тест
`every_routed_operation_in_table_when_router_serves_it` берёт пути
`.route(...)` из `crates/rest/src/router.rs` и проверяет, что каждый
метод, который отвечает роутер под `/api/v1`, есть в таблице (кроме
маршрутов модулей `/m/...`). Так что новый эндпоинт без строки в
таблице или с расходящейся схемой ломает CI.

Инструмент операции называется `liftoff_<name>`, его аргументы — один
плоский объект: параметры пути (обязательные строки), поля query и поля
тела рядом (имена не пересекаются — это проверяет тест
`argument_names_disjoint_when_operation_has_parts`; схема каждого поля
равна схеме `schemars` его поля в query или теле, а
`additionalProperties: false`). Вызов становится
запросом REST с `Authorization: Bearer` из сессии конфига CLI; ошибка
API — результат с `isError: true` и текстом `<status> <title>:
<detail>`. Операция, отвечающая событиями (логи окружения), всегда
вызывается с `follow=false`; `follow: true` отклоняется ошибкой
инструмента: инструмент отвечает один раз и не стримит. Параметр пути
кодируется как один сегмент (`%`, `/`, `?`, `#` экранируются), пустой,
`.` и `..` отклоняются: парсер URL свернул бы такой сегмент, и запрос
ушёл бы на другой маршрут. Клиент HTTP ждёт соединения 10 с, ответа —
3 минуты (как CLI) и читает не больше 4 МиБ ответа.

`crates/mcp` — адаптер на стороне клиента API: зависит от `api`,
`reqwest` и `rmcp`, но не от `core`. `lof mcp`
(`crates/liftoff/src/commands/mcp.rs`) берёт URL и сессию выбранного
сервера и отдаёт stdin/stdout `rmcp`; пока он работает, stdout — только
JSON-RPC.

Инструменты модулей приходят из `GET /modules` (только включённые) как
`liftoff_<module>_<tool>`, точка в имени модуля становится `_`
(`liftoff_dns_cloudflare_zones_list`). Имена инструментов в сервере
уникальны и не длиннее 64 символов (`^[a-zA-Z0-9_-]{1,64}$` у Claude
Code и Anthropic API), поэтому инструмент модуля не предлагается, если
его имя длиннее 64 символов, совпадает с инструментом API (API
выигрывает) или уже занято модулем раньше него в порядке `GET /modules`
(`dns.cloudflare` + `zones_list` и `dns` + `cloudflare_zones_list`:
остаётся первый, и вызов уходит только к нему); причина пишется в
stderr. Если `GET /modules` не ответил, `tools/list` отдаёт только
инструменты API, а вызов инструмента модуля — ошибка инструмента с
отказом API (`cannot list the modules: 401 …`), а не «no tool». Вызов идёт в `POST
/api/v1/modules/{name}/tools/{tool}` с телом `{"arguments": {...}}`
(`ModuleToolCall`) и отвечает `{"result": <JSON модуля>}`
(`ModuleToolAnswer`); строка `modules_tools_call` таблицы, так что этот
маршрут — и инструмент API. Дизайн требует, чтобы инструменты
модулей были в том же сервере: это
минимальный маршрут для этого, он отмечен для владельца. Правила — в
ядре: `ModuleRouteService::call_tool` через `Hooks::handle_tool`
(`ModuleHooks`: модуль установлен — иначе 404, включён — иначе 424,
инструмент объявлен в манифесте — иначе 404, аргументы проходят
JSON Schema инструмента из манифеста — иначе 400 `arguments.<поле>`:
модули недоверенные, так что модуль не видит аргументов,
которые его схема запрещает) запускает `handle-tool` от
имени вызывающего, как маршрут модуля за сессией: `core-call` модуля
несёт права актора, отдельного `authorize` ядро не спрашивает. Ответ
модуля, который не JSON, — 500.

Ресурсы: `liftoff://deploys/{id}/log` (`GET /deploys/{id}/log`,
`text/plain`) и `liftoff://projects/{p}/environments/{e}/log` (`GET
…/logs?tail=500&follow=false`, строки `data:` потока SSE). Каждая часть
URI — один сегмент пути: часть с `%`, `?`, `#`, пустая, `.` или `..` не
называет ресурс.

Критерий плана (`lof mcp` видит инструменты, `liftoff_projects_list`
отвечает) проверяет `crates/liftoff/tests/cli/mcp.rs`: тест запускает
сам бинарник `liftoff mcp` с конфигом вошедшего CLI, проводит
`initialize`, `tools/list` и `tools/call` по stdin/stdout и сверяет
bearer, который увидел стаб. Алиасы артефактов
(`crates/mcp/src/aliases.rs`) и HTTP-транспорт
 — разделы «Артефакты» и «MCP по HTTP» ниже;
руководство пользователя — `docs/MCP.md`.

### Function-listener (FN-11…13)

Caddy отдаёт домен запущенного function-environment на `127.0.0.1:<[functions]
bind>` с `Host` без изменений (`Route::Listener` в `ProxyConfig`, снапшоты
PRX-07); защищённый (`tenants`) домен функции сначала проходит гейт
(`forward_auth`), как compose. Listener (`crates/liftoffd/src/listeners/functions.rs`,
hyper http1) принимает только loopback-пиров, `[functions] bind` обязан быть
loopback (проверка конфига). Правила в ядре
(`liftoff_core::service::listener`): `ListenerRoute::environment_of` (`Host` →
домен → привязка → environment нужного вида; иначе 404), `forwards_header`,
`forwarded_cookies`, `is_liftoff_authorization` (что не доходит до кода
пользователя: `X-Liftoff-*` клиента, cookie `__Host-liftoff*`,
`Authorization` с любым токеном liftoff по префиксу). Порядок: хост → не
`running` → 503 → `Access::check` (до чтения тела; `Login` → 302 на
`https://<api>/gate/start?host&rd`, `Unauthenticated` → 401, `Forbidden` →
403) → тело (не больше 64 МиБ, иначе 413) → заголовки (минус перечисленное
и hop-by-hop, плюс `X-Liftoff-User`/`X-Liftoff-Email` пропущенного
человека) → `Functions::invoke` (рантайм `liftoff-wasm`, в тестах фейк) →
ответ функции как есть; `NotActive` → 503, `Timeout` → 504, trap и сбой
хоста → 500 без причины (причина в логе). Активные компоненты живут в
памяти демона: старт (`Reconciler::startup`) активирует текущий деплой
каждого function-environment заново (`DeployService::reactivate`), до
этого listener отвечает 503. `stop`/`start` функции меняют только
состояние и маршрут Caddy, компонент остаётся активным; `lof logs`
функции читает её stdout и stderr из памяти рантайма (последние 1000
строк на environment, строка до 16 КиБ, порт `FunctionHost::logs`).

### Рантайм функций (FN-01…10)

Деплой функции ведёт ядро (`DeployService`):
`POST /deploys` с полем `component` проверяет компонент портом
`FunctionHost::check` до записи файла и строки деплоя, пишет строки и
файл (`ComponentFiles`) под одной блокировкой, ставит деплой в очередь;
исполнение — хук `before_deploy`, `precompile`, сборка `wasi:config` из
секретов (`SecretsService::assemble`, тот же env, что у compose, без
`IMAGE_TAG`) и `activate`. Чистка файлов после каждого завершённого
деплоя функции оставляет то, на что ссылается история (20 деплоев)
любого environment. Старт демона (`Reconciler::startup` →
`DeployService::reactivate`) активирует текущие деплои функций заново:
активные компоненты живут только в памяти.

Рантайм (`crates/wasm/src/functions*`, только wasmtime и
`wasmtime-wasi*`, ядро о них не знает): `names.rs` читает имена
компонента `wasmparser` до компиляции (компонент, экспорт
`wasi:http/incoming-handler@0.2`, импорты только `wasi:`; размер до
`MAX_COMPONENT_BYTES` = 50 МиБ) — та же функция `check` у CLI перед
загрузкой; компиляция Cranelift с кэшем wasmtime по хешу и версии;
`pool.rs` держит до `instances_per_env` инстансов на environment, один
срок на запрос (ожидание инстанса включено, epoch interruption),
лимиты памяти и ресурсов как у модулей, без каталогов, сокетов и
переменных окружения; `config.rs` — `wasi:config` над `Env` активации;
`keyvalue.rs` — `wasi:keyvalue` над портом `Store` (правила длины ключа
и `kv_max_mb` — в ядре, `service::kv`); `http.rs` — исходящий
`wasi:http` без allow-list, но без loopback и частных адресов, тело
ответа до 64 МиБ; `logs.rs` — stdout и stderr environment, последние
1000 строк. Мир — `wit/function/` (`liftoff:function/function`);
контрактные функции на трёх языках (`modules/testing/functions/`,
`just functions`, `crates/wasm/tests/contract_functions.rs`) и шаблоны
`templates/fn-*` собраны под него. Руководство — `docs/FUNCTIONS.md`.

### Туннель и tunnel-listener (TUN-01…09, DEVT-01…13)

Три части, у каждой свой крейт:

- протокол — `crates/tunnel` (`liftoff-tunnel`, ни одного крейта
  workspace в зависимостях): кадры управления (`Hello`, `Shutdown`,
  `Evicted`) — текстовые сообщения WebSocket в JSON, байты `yamux` —
  бинарные; сервер открывает поток на каждый HTTP-запрос и пишет его как
  HTTP/1.1, клиент соединяет поток с `127.0.0.1:<PORT>`; keepalive,
  backoff, лимит полосы (`Bandwidth`), лимиты сообщения и окна;
- демон — `GET /api/v1/tunnel` (`crates/rest/src/routes/tunnel.rs`:
  `cli`-сессия, `X-Liftoff-Project`, `TunnelsService::open`, upgrade с
  `X-Liftoff-Login-Url` для `private`) отдаёт сырое соединение после
  `101` в `DaemonTunnels` (`crates/liftoffd/src/tunnels.rs`), тот — в
  `TunnelServer` под ключом id environment и сообщает ядру
  (`TunnelsService::connected`/`disconnected`/`expire`) начало и конец
  соединения с номером регистрации, так что конец заменённого
  соединения состояния не трогает; tunnel-listener
  (`crates/liftoffd/src/listeners/tunnel.rs`, `[tunnel] bind`, только
  loopback) находит environment по `Host`, спрашивает `Access::check` до
  первого байта, обслуживает `/.liftoff/callback|logout` и отдаёт запрос
  `TunnelServer::forward` со своими `X-Forwarded-*`;
- CLI — `crates/liftoff/src/commands/dev/tunnel.rs`: `TunnelClient`
  после запуска `[dev] command`, печать dev-адреса и ссылки входа по
  одному разу, `--share`, `Shutdown` на `Ctrl-C`.

Правила (имя `<project>-dev` или `<project>-<login>`, владелец,
`private`, managed-домен с хуком `domain_created`, `running`/`offline`,
`tunnel.offline_after`) — в `liftoff_core::service::TunnelsService`; на
fake clock ядра. Руководство — `docs/DEV.md`.

### Вход браузера и одноразовые ссылки (OTL-01…09, VIS)

`LinksService` (`core::service::links`) выдаёт ссылку
`https://<api>/api/v1/auth/links/lfl_…` (10 минут, одно использование,
своя цель, в хранилище только хеш) и гасит её в сессию `browser` на 24
часа. `crates/rest/src/routes/login.rs`: ссылка ставит cookie API-хоста
`__Host-liftoff_session` и сразу `302` на
`https://<хост цели>/.liftoff/callback?code=lfc_…` — передача сессии
одноразовым кодом гейта, как в v1 SSO; код на хосте цели гасит сам
function- или tunnel-listener (`GateListener::redeem`), cookie хоста —
`__Host-liftoff_gate`. `/login?host&rd` — встроенная страница
(`crates/rest/src/pages/`: вход, `403`, `dev offline`; HTML без
JavaScript, CSP с хешем стиля) или `render_page(login)` модуля.
`Access::check` (`core::service::visibility`) — одно правило для обоих
listener'ов: хост — адрес цели, `shared` пускает всех, `private` —
админа, владельца туннеля и тех, кого пускает хук `gate`, с сессией
хоста или `cli`-сессией в bearer; иначе `302` на вход, `401` JSON-клиенту
или `403`. Что скрыто от хуков и кода пользователя (cookie liftoff,
токены liftoff в `Authorization`) — одно правило `service::request`.

### Артефакты (ART, BLOB, VIEW, IMPA)

Руководство: `docs/ARTIFACTS.md`.

- **Сервис.** `ArtifactsService` (`core::service::artifacts`): `publish`,
  `update`, `rollback`, `share`, `unshare`, `delete`, `delete_version`,
  `list`, `get`, `versions`, для просмотра `view`, `served` и `content`, и
  `sweep`. Имена файлов — доменный тип `ArtifactPath`, проверенный у
  входа (REST, CLI, импорт); лимит размера, TTL, обрезка до `versions`,
  заголовок версии и правило «владелец или админ» после одного вопроса
  `authorize` — в сервисе. `login_url` у `publish` и ссылка `lof artifact
  open` — `LinksService` с целью `LinkTarget::Artifact`; ссылку на
  артефакт гасит `LinksService::open_own` (cookie сессии ставит сам
  API-хост).
- **Порт `Blobs` и адаптер.** Файлы версий — за портом ядра `Blobs`
  (`core::ports::blobs`), чтобы `liftoff-core` остался без I/O. Порядок
  записи: строки версии в store одной транзакцией, затем `put` блобов,
  `link` файлов и `release` каждого `put`, при сбое — откат строк и
  ссылок; удаление (`rm`, sweeper) снимает сначала файлы, потом строки,
  обрезка — наоборот. Реализация — `liftoff_store::FileBlobs`
  (`crates/store/src/blobs.rs`): `blobs/<sha256>` (запись во временный
  файл и `rename`), каталоги версий `<id>/<n>/` из hardlink'ов на блобы
  (или копий: `LinkMode::Detect` пробует hardlink один раз при открытии),
  счётчик ссылок `artifact_blobs` и ссылки `artifact_blob_links` в той же
  SQLite-базе, что `SqliteStore`; работа с файлами — под своей
  блокировкой, соединение store берётся только на короткие транзакции.
  При открытии собирается оставшееся от упавшего процесса. Контракт —
  `fakes::blobs_contract` (`FakeBlobs` и `FileBlobs` в обоих режимах).
  Демон открывает `FileBlobs` над `paths.data/artifacts` в
  `crates/liftoffd/src/daemon.rs`; у CLI порт — `Offline`.
- **REST.** `crates/rest/src/routes/artifacts.rs` под `/api/v1`, за
  сессией: версия в `multipart/form-data` или JSON (`ArtifactBody`,
  форма MCP), тело ограничено `ArtifactLimits`
  (`crates/rest/src/extract.rs`: `max_size_mb` плюс 256 КиБ, у JSON с
  поправкой на base64) вместо `max_body_bytes`, `long_request_timeout`.
- **Просмотр.** `crates/rest/src/routes/artifacts_view.rs` в корне
  API-хоста (не под `/api/v1`, без CORS): `GET /a/{id}`, `/a/{id}/v/{n}`
  (обёртка, `pages::artifact`: своя CSP со стилем по хешу и `frame-src
  'self'`, без скриптов), `/a/{id}/v/{n}/{*file}` (содержимое с CSP
  `sandbox …; frame-ancestors 'self'`, `nosniff`, типом по расширению и
  своим `Cache-Control`, которое общий слой заголовков оставляет только
  ответу с расширением `OwnCacheControl`), `POST /a/{id}/share|unshare`
  (кнопки обёртки, проверка `Origin`). Каждый запрос — `Access::check` с
  целью `artifact:<id>`. С `[artifacts] host` роутер `app_router`
  разводит запросы по `Host` (`by_host`): этот хост получает только
  просмотрщик без префикса `/a` и свой `/.liftoff/callback`
  (`GateListener::redeem`), API-хост — всё остальное без `/a/`; стек
  middleware навешан на каждый из двух роутеров. `ProxyConfig` получает
  `artifacts: Option<Host>`, и Caddy ведёт этот хост на `[api] bind`.
- **Sweeper.** `crates/liftoffd/src/sweep.rs`: задача рядом с очередью
  деплоев зовёт `ArtifactsService::sweep` при старте и раз в
  `SWEEP_ARTIFACTS_EVERY` (минута) до остановки демона; неудачный круг —
  в журнал. Время истечения решают часы ядра (`Clock`), так что тесты
  ART-08 идут на фейковых часах.
- **Импорт.** `liftoffd import-artifacts <dir>`
  (`crates/liftoffd/src/import_artifacts.rs`) читает базу
  предшественника через `rusqlite` только на чтение и его файлы; правила
  (владелец по e-mail, пропуски, повтор) — `ArtifactImport` в ядре.

### MCP по HTTP (MCPH-01…06)

Руководство: `docs/MCP.md`.

- **Транспорт** — `liftoff_mcp::http::StreamableHttp`: tower-сервис над
  `rmcp` (`StreamableHttpService`), те же `Server`, инструменты, алиасы и
  ресурсы, что у stdio. До `rmcp` он спрашивает свой `Backend` о сессии
  API по bearer; `Refusal` становится `401` с `WWW-Authenticate: Bearer
  resource_metadata=…`, `403`, `424` или `503`. Свои сессии транспорт
  держит в `LocalSessionManager` `rmcp` под обёрткой с потолками
  (`HttpConfig::max_sessions`, `max_sessions_per_principal`), простоем
  (`session_idle`), привязкой к `Grant::principal` и сроком
  `Grant::valid_for`; `allowed_hosts` — только хост API; `shutdown`
  закрывает потоки событий. `crates/mcp` по-прежнему зависит только от
  `api`.
- **Где собран.** `crates/liftoffd/src/mcp_http.rs`: `CoreBackend`
  переводит `McpAuthService::verify_oauth` ядра в `Grant` (сессия — URL
  своего loopback-листенера `[api] bind` и секрет сессии `mcp`),
  `mount` вешает транспорт на `/mcp` поверх уже собранного роутера
  `rest` (после его слоёв), и демон отдаёт результат в
  `liftoff_rest::serve_router_on`. Так `rest` не зависит от `mcp`: два
  адаптера соединяет корень композиции. Потолки: 1024 сессии, 16 на
  пользователя, 5 минут простоя.
- **Ядро.** `McpAuthService` (`core::service::mcp_auth`):
  `resource_metadata` для `GET /.well-known/oauth-protected-resource`
  (`crates/rest/src/routes/mcp.rs`) и `verify_oauth` для `/mcp`; оба —
  `424` без модуля `mcp`. Хук `mcp_resource` спрашивается только у
  модуля `mcp`, `verify_token` — только у модуля, заменяющего `login`
  (`oidc`). Секрет сессии `mcp` — `Secrets::mac` от токена;
  `AuthService::authenticate` принимает сессию `mcp` только при
  установленном и включённом модуле `mcp`.
- **Модули.** `modules/mcp` — только конфиг (`resource`,
  `allowed_audiences`, `client_id`) и хук данных `mcp_resource`;
  `requires: ["oidc"]`. `modules/oidc` получил хук `verify_token`
  (`jwt.rs`: тот же разбор и RS256, что у ID-токена, но `aud` — из
  списка ресурса и без `azp`; JWKS по неизвестному `kid` не чаще раза в
  минуту). Второй проверяющий JWT не нужен: токен проверяет `oidc`, а
  `mcp` его не видит. Оба требуют `liftoffd_min = "2.0.0"`.

### Панель `liftoff-web`: CORS и модуль `web` (WEB-01…05)

- **Ядро.** `WebPanelService` (`core::service::web_panel`) спрашивает хук
  данных `web_panel` у модуля с именем `web` на каждый запрос: origin
  панели (доменный тип `Origin`, `crates/core/src/domain/web.rs`) или
  «панели нет»; сбой — «панели нет» (fail closed). Из него же
  `same_site`: cookie сессии `browser` — `SameSite=None` при панели,
  иначе `Lax`.
- **CORS-слой** — `crates/rest/src/cors.rs`, `from_fn_with_state` только
  на роутере `/api/v1` (не на `/a/`, корневых страницах, `/mcp` и
  listener'ах): `Origin` панели получает `Access-Control-Allow-Origin`,
  `Access-Control-Allow-Credentials: true` и `Vary: Origin` на любой
  ответ, preflight слой отвечает сам (`204`); запрос с origin панели
  помечается `PanelRequest`, и только такой запрос (без
  `Authorization`) bearer-слой аутентифицирует cookie
  `__Host-liftoff_session`. Чужой origin получает только `Vary: Origin`.
  Установка и удаление `web` действуют без рестарта; цена — вызов модуля
  на каждый запрос с `Origin`.
- **Страницы.** `render_page` с `take_pages = true` отвечает редиректом
  на `<origin>/<kind>?return=&host=`; ядро (`ModuleHooks::render_page`)
  спрашивает `web` первым и принимает редирект только от него и только
  на origin его панели (`WebUrl`), иначе страница модуля `oidc` или
  встроенная.
- **Модуль** `modules/web`: конфиг `origin` и `take_pages`, хуки
  `web_panel` и `render_page`, без маршрутов и таблиц.

### Исключения: процессы хоста вне `crates/host`

- CLI говорит с демоном только по HTTP, с одним исключением:
  `lof server upgrade` на хосте сервера запускает установщик релиза
  (`sh -s` со встроенным `scripts/install.sh`) и `systemctl restart
  liftoffd` через шов `HostCommand` (`crates/liftoff/src/host.rs`,
  `ProcessHost` в продакшене, фейк в тестах). Правила сервера при этом
  остаются в демоне.
- `lof artifact open <id> --browser` (`crates/liftoff/src/commands/artifact.rs`) через тот же шов
  `HostCommand` отдаёт свежую одноразовую ссылку браузеру машины:
  `xdg-open <ссылка>` (на macOS `open`), вектор аргументов, без shell,
  stdin закрыт, stdout в stderr CLI. Ссылка печатается до запуска; сбой —
  предупреждение в stderr и выход `0`. Без `--browser` процессов нет.
- Локальный режим CLI запускает `git`
  (`crates/liftoff/src/local/git.rs`): `clone`, `add`, `commit`, `push`,
  `status`, `diff --cached`, `ls-remote`, `symbolic-ref` в рабочем
  каталоге (тот же `GitRunner` клонирует, коммитит и пушит у `lof create` с сервером); в чужом рабочем дереве (`server connect`,
  `create` существующего репо) коммит берёт только
  пути, которые записал план (`git commit --only -- <пути>`). Всегда вектор аргументов, никогда
  shell; `-c protocol.ext.allow=never`, `GIT_TERMINAL_PROMPT=0`, `--` перед
  путями и URL, stdin закрыт, stdout забирается (stdout принадлежит CLI),
  stderr проходит насквозь. Токен подключения форжа попадает в дочерний
  процесс только через окружение (`GIT_CONFIG_*`: заголовок
  `Authorization` только для `https://<host>/`), не через аргументы.
- `lof dev` (`crates/liftoff/src/commands/dev.rs`)
  запускает `[dev] command` из `liftoff.toml` (иначе то, что предложила
  детекция стека по `package.json`) как `sh -c <строка>` в корне рабочего
  дерева. Это единственный shell у CLI: строку пишет сам человек в своём
  манифесте, она никогда не приходит с сервера, и пишется она как
  команда оболочки (`cd web && bun dev`); `--port N` для фреймворков,
  которые игнорируют `PORT`, дописывается в конец строки
  (`liftoff_core::dev::with_port`). `PORT` и секреты передаются только
  через окружение, не через аргументы. stdin закрыт, stdout и stderr
  проходят насквозь. Дочерний процесс — лидер своей группы процессов:
  `Ctrl-C` шлёт группе `SIGTERM`, через 5 секунд `SIGKILL` тому, что
  осталось (`rustix::process::kill_process_group`, без `unsafe`).
- `lof deploy` в function-проекте (`crates/liftoff/src/commands/functions.rs`) запускает `[function]
  build` из `liftoff.toml` так же, как `[dev] command`: `sh -c <строка>`
  в корне рабочего дерева, строка — из манифеста самого человека, никогда
  с сервера. Это второй и последний shell у CLI. stdin закрыт, stdout и
  stderr сборки идут в stderr CLI (stdout принадлежит CLI), секретов
  сборка не получает. Упавшая сборка — exit 1 без загрузки (FDEP-07).
  Сборка, как и `[dev] command`, — лидер своей группы процессов:
  `Ctrl-C`, `SIGTERM` и `SIGHUP` CLI завершают всю группу (`SIGTERM`,
  через 5 секунд `SIGKILL`), и выход `sh` тоже: оставленный сборкой в
  фоне процесс не держит её вывод, и `lof deploy` не зависает.
  Шаблоны функций (`templates/fn-*`) нового пути к процессам не
  добавляют: их `[function] build` и `[dev] command` (`cargo build`,
  `bun run build`, `componentize-py`, `wasmtime serve`) — те же две
  строки манифеста человека.
- `liftoffd setup` — одноразовый установщик, код корня композиции: он
  запускает команды хоста (`useradd`, `chown`, `systemctl`, `docker
  compose`, `visudo`) через свой трейт `Runner`
  (`crates/liftoffd/src/setup.rs`, `ProcessRunner` в продакшене) до того,
  как появляется демон и его адаптеры. Всё, что делает работающий демон,
  идёт через порты и `crates/host`; новые шаги установки (`--import` из
) используют тот же `Runner`, а не новый путь к процессам.

## Локальный режим CLI

Сервер прогона:
`--server`, иначе `.liftoff`, иначе `default_server` конфига, иначе
сервера нет и CLI работает локально. Сервер, названный в `--server` или
`.liftoff`, которого нет в конфиге (в том числе когда конфига нет
вовсе), это ошибка конфига (78, LCL-08), а не тихий переход в локальный
режим.

Локально CLI вызывает те же сервисы ядра (`GitService`, `ModuleService`,
`TemplateCatalog`, `CreateService` до `Push`), что и демон, над
`LocalPorts` (`crates/liftoff/src/local/ports.rs`). Всё живёт в каталоге
конфига (`~/.config/liftoff/`, `0700`):

| Файл | Что | Адаптер |
|---|---|---|
| `key` | ключ локального хранилища, 32 случайных байта hex, `0600` | `local::Home::master_key`, шифр `core::crypto::MasterKey` |
| `secrets.db` | секреты локальных проектов (по области: тенант проекта, проект, environment), подключения форжей, установленные модули, кэш шаблонов; все секреты запечатаны ключом | `LocalSecretStore`: свои методы секретов и порт `Store` |
| `modules/` | компоненты `git.*` из реестра (`<name>-<version>.wasm`) | `liftoff_wasm::HostRuntime` |
| `modules.db` | ключи и таблицы модулей | `LocalModuleStore` над `liftoff-module-sql` (тот же авторизатор, что у демона) |
| `cache/` | кэш компиляции wasmtime | хост модулей CLI |

- `LocalSecretStore` как `Store` хранит только записи локального
  режима; записи инсталляции (пользователи, сессии, проекты,
  environment, домены, деплои, токены, аудит) локально не существуют:
  поиск ничего не находит, запись отвечает `Unavailable` с подсказкой
  `lof server connect`. Единственный пользователь `local_user()` —
  админ машины, поэтому `Authorizer` с `NoHooks` пускает его, как админа
  одиночной инсталляции. Контракт `fakes::store_contract` описывает
  хранилище инсталляции и целиком к нему не применяется: его правила
  модулей (они не заводят пользователей) идут над `LocalSecretStore`
  как есть, правила подключений и кэша шаблонов проверяют его тесты для
  локального пользователя; `LocalModuleStore` проходит правила
  `ModuleStore` контракта.
- Секреты локального проекта не идут через `SecretsService`: у него
  нет записи проекта до `lof server connect`, а владелец области, аудит
  и тенанты — правила инсталляции. Правила самих секретов при этом из
  ядра: `ci` только у секрета проекта (`service::check_ci_scope`), порядок
  областей и слияние env (`env_scopes`, `merge_layers`: нижняя область
  сильнее, ключи только для CI не попадают), область секрета плана
  (`plan_secret_scope`); `LocalSecretStore` только хранит запечатанные
  строки. Локальный проект называется `--project` или именем
  репозитория `origin`; одна текущая версия на ключ.
- Хост модулей CLI (`local::cli_host`): `liftoff-wasm` без планировщика
  (`schedule` принимается и никогда не тикает) и с `LocalCoreCalls`:
  метод из таблицы ядра отвечает `unavailable` с подсказкой `lof server
  connect`, метод вне таблицы `forbidden`, как у моста демона (LCL-07).
  Контракт хоста модулей проходит над `LocalModuleStore` (LCL-06);
  контракт строит свой `ModuleHost`, поэтому ловушки и таймауты
  проверяются ещё и через сам `cli_host`.
- Модули ставит `ModuleService::install` над `HttpModuleSource` (индекс,
  подпись ключом `REGISTRY_PUBLIC_KEY`, SHA-256, версия WIT);
  `lof git connect` сам ставит нужный `git.<provider>`. При каждом
  запуске, которому нужны модули, `ModuleService::restore` загружает
  установленные.
- `lof create <name> --template` локально строит план
  (`plan_from_template`), генерирует репозиторий (`CreateService::generate`,
  через подключение локального пользователя), клонирует в `./<name>`,
  подставляет только `{{project}}` (`Substitution::apply_project_to`:
  домен знает сервер), кладёт `[secrets]` в локальное хранилище,
  коммитит и пушит, и останавливается перед серверными шагами (LCL-03).
  `WriteCiSnippet` локально пропускается: workflow называет API сервера, и
  его пишет `lof server connect`. Базовый домен локальных настроек
  `liftoff.localhost` — заглушка, никуда не уходит.
- `lof server connect <url>` (после `lof login <url>`) просит у сервера
  план существующего репозитория (`POST /git/create-plan`) и исполняет
  его шаги: локальные в рабочем каталоге, серверные через их
  эндпоинты, затем `.liftoff` и `.gitignore` (LCL-04). `ask_secrets`
  берёт значения сначала из локального хранилища. `lof secret push
  [--move]` переносит остальные локальные секреты (LCL-05). `lof secret
  sync` есть только с сервером: `POST /secrets/sync` отправляет секреты
  `ci = true` в CI репозитория через модуль `git.*`; без сервера команда
  отказывает, как `push`.
- `lof templates` без сервера это `TemplateCatalog` над локальными
  подключениями и `[local] templates` конфига CLI, в том же типе провода,
  что `GET /git/templates`. `lof dev` без сервера берёт секреты из
  локального хранилища (`env_scopes`, `merge_layers`) и добавляет
  `LIFTOFF_PROJECT`, `LIFTOFF_ENV` и пустой `LIFTOFF_DOMAIN`, как сервер;
  туннеля нет. Детекция стека (`StackDetector`) и выбор `--port`
  (`core::dev`) читают рабочее дерево через трейт `SourceTree`: CLI даёт
  им `DirTree` (чтение каталога через `std::fs`, оно живёт в ядре рядом с
  трейтом), тесты ядра — фикстуры `tests/fixtures/detect/`; в сеть
  детекция не ходит.
- Исходники шаблонов, которые идут с liftoff, лежат в `templates/<имя>/`
  (`fn-ts`, `fn-rust`, `fn-python`), публикуемых отдельными
  репозиториями. `crates/core/tests/templates.rs` разбирает каждый
  `liftoff.toml` через `ProjectManifest::parse`. Как сделать шаблон:
  `docs/TEMPLATES.md`.

## Request flow (REST example)

1. `SetRequestId` assigns `x-request-id` if absent; `TraceLayer` opens a span
   carrying method, path and id.
2. `CatchPanic`, `Timeout`, `RequestBodyLimit` bound failure modes.
3. The handler extracts JSON (`deny_unknown_fields`), parses raw strings into
   domain types (`ProjectName::new`, ...) — the only place validation
   happens — and calls a service.
4. `liftoff_core::Error` maps to `ApiError`, serialised as
   `application/problem+json`. Internal causes are logged inside the span,
   not returned.

## Error vocabulary

`liftoff_core::Error` is `#[non_exhaustive]`. The REST adapter matches it with
a trailing `_ => internal` arm so a new variant can never leak an unreviewed
message. The CLI sees only problem+json and prints `error: <detail>`.

| Variant           | REST |
|-------------------|------|
| `Validation`      | 400  |
| `Unauthenticated` | 401  |
| `Forbidden`       | 403  |
| `NotFound`        | 404  |
| `Conflict`        | 409  |
| `Gone`            | 410  |
| `ModuleRequired`  | 424  |
| `Internal`        | 500  |
| `Upstream`        | 502  |
| `Unavailable`     | 503  |
| `Offline`         | 503  |

## Adding a driven port (database, queue, HTTP client)

1. Declare the trait in `core::ports`, returning `impl Future` and domain
   errors, plus a fake under `core::fakes` (`test-support` feature).
2. Make the service generic over it.
3. Implement it in the matching adapter crate (`store`, `host`, `proxy`,
   `wasm`).
   Keep SQL/HTTP/process details there.
4. Construct it in `liftoffd` and pass it to the service.
5. Test the service with an in-memory fake; test the adapter against a real
   dependency behind a feature or `#[ignore]`.

## Observability

- `tracing` spans per request with `request_id`; JSON lines by default.
- Health: REST `/healthz` (liveness) and `/readyz` (readiness; extend to ping
  dependencies).
- Next steps when needed: `tracing-opentelemetry` exporter in `app::telemetry`,
  `metrics` + Prometheus endpoint on a separate admin port.

## Build profiles

| Profile     | Use                      | Notes                                                 |
|-------------|--------------------------|-------------------------------------------------------|
| `dev`       | iterate                  | deps at `opt-level = 2` so tests run at useful speed  |
| `release`   | ship                     | fat LTO, 1 CGU, stripped, overflow checks on          |
| `profiling` | perf / flamegraphs       | `release` + line tables, not stripped                 |
