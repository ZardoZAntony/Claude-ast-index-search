# HOWTO: работа с ast-index

[English version](howto.md)

Руководство по задачам для этого форка: PHP (включая Bitrix), JavaScript, TypeScript, Vue. Все команды запускаются
из корня проекта; `--format json` даёт тот же ответ для скрипта. Подробности по командам — в
[справочнике PHP](../plugin/skills/ast-index/references/php-commands.md) и
[справочнике JS/TS](../plugin/skills/ast-index/references/typescript-commands.md) (на английском).

- [Установка и первый индекс](#установка-и-первый-индекс)
- [Свежесть индекса](#свежесть-индекса)
- [Настройка проекта](#настройка-проекта)
- [Найти код](#найти-код)
- [PHP: безопасный рефакторинг](#php-безопасный-рефакторинг)
- [Bitrix: маршруты, события, реестры, ядро](#bitrix-маршруты-события-реестры-ядро)
- [JavaScript, TypeScript, Vue](#javascript-typescript-vue)
- [Фронтенд → бэкенд: кто вызывает эндпоинт](#фронтенд--бэкенд-кто-вызывает-эндпоинт)
- [Когда лучше rg](#когда-лучше-rg)
- [Агенты и скрипты](#агенты-и-скрипты)
- [Если что-то не так](#если-что-то-не-так)

## Установка и первый индекс

```bash
# Linux x86_64, glibc 2.39+ (Ubuntu 24.04+, WSL): готовый бинарь
mkdir -p ~/.local/bin
curl -fsSL https://github.com/ZardoZAntony/Claude-ast-index-search/releases/latest/download/ast-index-linux-x86_64.tar.gz | tar -xz -C ~/.local/bin
ast-index version

# в остальных случаях — сборка (нужен Rust)
cargo install --locked --git https://github.com/ZardoZAntony/Claude-ast-index-search ast-index
```

Плагин Claude Code — скилл, правила в начале сессии, хуки, которые держат индекс свежим:

```bash
claude plugin marketplace add ZardoZAntony/Claude-ast-index-search
claude plugin install ast-index@ast-index-php
```

Затем в проекте: `/initialize` (предложит `.ast-index.yaml` и построит индекс) или вручную:

```bash
ast-index rebuild      # весь индекс, от секунд до минуты
ast-index stats        # есть ли индекс и что в нём
```

Индекс лежит вне проекта (`~/.cache/ast-index/<hash>/` в Linux, `~/Library/Caches/ast-index/<hash>/` в macOS);
`ast-index clear` удаляет его.

## Свежесть индекса

Индекс, пропустивший изменение, отвечает уверенно и неверно.

| Файлы меняются через | Что обновляет индекс |
|---|---|
| Правки между сессиями | плагин запускает `update` в начале сессии; первый запрос его дожидается |
| Edit/Write Claude | хук плагина после каждой правки |
| `git switch/pull/rebase`, генераторы, форматтеры, IDE | `ast-index watch` — плагин запускает по одному на проект; `ast-index watch-status` скажет, работает ли он |
| Без плагина (другой агент, CI) | `ast-index update` после изменения или запущенный `ast-index watch` |

После установки новой версии ast-index остановите работающие watcher'ы (`pkill -f '^ast-index watch'`): они держат
старый бинарь. В Linux на большом дереве для `watch` может понадобиться увеличить `fs.inotify.max_user_watches`.

## Настройка проекта

`.ast-index.yaml` в корне проекта; пути — шаблоны в стиле gitignore.

| Ключ | Зачем |
|---|---|
| `exclude` | Не индексировать стили, сгенерированные файлы, сторонний код |
| `include_hidden` | Скрытые пути с настоящим кодом (Bitrix `.default/`, `.settings.php`, `.tests/`) — остальные пути с точкой пропускаются |
| `unused_ignore`, `unused_ignore_names` | Файлы и имена методов, которые фреймворк вызывает по соглашению (инсталляторы, `*Action`), — никогда не числятся мёртвыми |
| `duplicate_mirrors` | Фрагменты путей намеренных копий (`/Contracts/`) — `duplicates` выводит их в конце |
| `js_aliases` | Префиксы спецификаторов, которые добавляет сборщик или раннер тестов сверх `paths` из `tsconfig`/`jsconfig` (`"@ui/": src/ui/`) |
| `external` | Каталоги фреймворка и вендоров (`bitrix/modules/main/lib`, `vendor/symfony`) — только определения |

Полный пример — в [README](../README.md#configuration). После правки конфига — `ast-index rebuild`.

## Найти код

| Вопрос | Команда |
|---|---|
| Где живёт функциональность | `ast-index explore order cancel refund` — символы по релевантности, их код, вызывающие, тесты |
| Слово в именах файлов и символов | `ast-index search Basket` (`--fuzzy`, `--type class`) |
| Класс и его FQN | `ast-index class OrderService` |
| Любой символ вместе с кодом | `ast-index symbol calculateTotal --with-content` |
| Файл по имени | `ast-index file OrderService` |
| Структура большого файла | `ast-index outline src/Order/OrderService.php` — определения с диапазонами строк |
| Что импортирует файл | `ast-index imports src/pages/cart/Cart.vue` |

Правила, которые спасают от неверного ответа:

- **Не угадывайте FQN.** `ast-index class <КороткоеИмя>` его покажет; на FQN, которого нет в индексе, команда
  отвечает классами с таким коротким именем.
- **FQN — в одинарных кавычках** (`'App\Order\OrderDto'`); в циклах shell — `while IFS= read -r fqn`, иначе без `-r`
  обратные слэши съедаются.
- **Смотрите строку `coverage`.** В ней сказано, что проверено (тесты, конфиги, PHPDoc, шаблоны); повторять тот же
  поиск через rg не нужно.

## PHP: безопасный рефакторинг

Имена классов разрешаются так же, как это делает PHP: `namespace`, `use` (алиасы, групповые), `\FQN`, FQN в строках,
типы PHPDoc. Поэтому классы с одинаковым коротким именем не смешиваются.

| Перед тем как… | Команда | На что смотреть в ответе |
|---|---|---|
| переименовать класс, сменить конструктор или API | `ast-index impact 'App\Order\OrderDto'` | каждая ссылка с видом, тесты, DI-конфиги (`Foo::class => …`), `.neon/.yaml/.json/.xml` |
| перенести класс в другой namespace | `ast-index move-plan 'App\Order\OrderDto' 'App\Order\Dto'` | новый путь, строка namespace, импорты добавить и заменить, FQN в строках, строки конфигов |
| изменить или удалить метод | `ast-index callers 'OrderRepository::save'` | разделы **receiver not inferred** и **calls typed as a supertype** — проверить вручную |
| понять, что вызывает метод | `ast-index callees 'App\Order\OrderService::place' --depth 3` | типы получателей, где объявлен каждый метод, реализации методов интерфейса |
| убедиться, что класс используется только там, где вы думаете | `ast-index usages 'App\Order\OrderDto' --no-tests` | одноимённые классы из других namespace не подмешиваются |
| тронуть интерфейс или базовый класс | `ast-index implementations 'App\Base\Handler'` | наследники через промежуточные классы и алиасы |
| увидеть один уровень наследования | `ast-index hierarchy 'App\Base\Handler'` | родители и дети в виде FQN |
| изменить константу или case перечисления | `ast-index usages 'App\Cache\CacheEnum::LIST'` | использования через наследников, `self::`/`static::`, упоминания в PHPDoc — отдельно |
| прочитать, что перечисление даёт по каждому case | `ast-index enum 'App\Cache\CacheEnum' --method ttl` | значения по case, константы и арифметика свёрнуты |
| удалить мёртвые классы | `ast-index unused-symbols --module src/Order/ --export-only --limit 500` | **Registered in DI config only** — зарегистрированы, но никем не запрашиваются |
| найти скопированные классы | `ast-index duplicates --path src/` | `UNUSED` — кандидаты на удаление; `[same FQN]` — две копии одного класса |
| посмотреть, как ветка изменила API | `ast-index changed --symbols --base origin/main` | публичные методы и параметры конструктора добавлены, удалены, изменены — из git, без checkout |

`ast-index changed` без `--symbols` выводит файлы, изменённые веткой.

## Bitrix: маршруты, события, реестры, ядро

| Вопрос | Команда |
|---|---|
| Маршруты и действия компонентов с действующими префильтрами (CSRF, авторизация) | `ast-index routes` (`--kind route\|component`, `--path local/modules/x`) |
| Подписки на события и их обработчики | `ast-index events --event OnSaleOrder` (`--module <путь>`, `--no-tests`) |
| Реестры объектов-дескрипторов как данные | `ast-index new 'App\Action\Descriptor'` — каждый `new` со свёрнутыми аргументами |
| Код ядра или вендоров | `ast-index class CIBlockElement`, затем `ast-index outline <его файл>` |

`routes` собирает префильтры так же, как ядро: атрибуты фильтров или `configureActions()` поверх
`getDefaultPreFilters()` по цепочке родителей, неявный CSRF-фильтр, `-prefilters` и `+prefilters`; у каждого фильтра
указано, откуда он взялся.

Каталоги ядра и вендоров из `external:` находят `class`, `symbol`, `file`, `outline`, `hierarchy`;
`implementations` и `impact` проходят наследование через них. `search`, `usages`, `unused-symbols`, `duplicates`
их не включают без `--external`. Не ищите класс grep'ом по `bitrix/` или `vendor/`.

## JavaScript, TypeScript, Vue

Импорты разрешаются в файлы модулей так же, как это делают сборщики: относительные пути, `paths` ближайшего
`tsconfig`/`jsconfig`, `js_aliases`, расширения, `index.*`. Экспорт называется `путь#имя` (`#default` — экспорт по
умолчанию).

| Перед тем как… | Команда |
|---|---|
| переименовать или изменить экспорт | `ast-index impact 'src/shared/price.js#formatPrice'` — импорты, использования в коде, типах TS и шаблонах Vue, цепочки реэкспортов, моки, тесты |
| узнать, кто импортирует модуль | `ast-index impact 'src/ui/Popup.vue'` |
| перенести или переименовать файл | `ast-index move-plan 'src/ui/Popup.vue' 'src/ui/popup/'` — спецификаторы по файлам в том стиле, в котором они написаны |
| удалить мёртвые экспорты и файлы | `ast-index unused-symbols --module src/` |
| модуль неизвестен | `ast-index impact formatPrice` — ответит, если имя экспортирует один модуль, иначе перечислит их |

`unresolved:` в ответе — спецификаторы, которые не разрешились, но могут указывать на модуль: проверьте вручную.
`usages <имя>` без пути ищет по короткому имени.

## Фронтенд → бэкенд: кто вызывает эндпоинт

```bash
ast-index http-calls --path src/modules/orders
ast-index http-calls --format json | jq '.calls[] | select(.url // "" | startswith("/api/v1/orders/"))'
```

Все вызовы бэкенда из модулей JS/TS/Vue и встроенных скриптов PHP-шаблонов: `fetch`, axios и его экземпляры, обёртки,
клиенты-объекты фабрик (`createClient().get(url)`), jQuery, `BX.ajax`, действия компонентов. Для каждого — метод,
адрес (константы, карты и помощники свёрнуты), отправляемые ключи и ключи, которые читаются из ответа; плюс данные
`window.X`, которые шаблон передаёт скриптам. Вызов через обёртку показан там, где обёртку вызывают, с адресом,
который передаёт эта строка.

Вместе с `routes`: `http-calls` говорит, какие страницы вызывают маршрут и каким методом, а `routes` — какие
префильтры его охраняют. Например, так находится изменяющий метод, который сайт вызывает, а у маршрута нет CSRF-фильтра.

Подробно — что распознаётся, как сворачивается адрес, как проверить шаблон: [HOWTO по http-calls](http-calls.ru.md).

## Когда лучше rg

Индекс выигрывает, когда ответ охватывает несколько файлов: связи, одноимённые классы, скрытые каталоги, тесты,
мёртвый код. `rg` берите, когда ответ даёт одна буквальная строка:

- статический вызов legacy-класса (`rg 'CSaleOrder::Update'` — точно и коротко);
- текст, который не является кодом: сообщения, SQL, значения конфигов, закомментированный код, документация;
- имена, собираемые во время выполнения (`$prefix . 'Handler'`, ``import(`./${name}.js`)``), CommonJS `module.exports`;
- файлы, которых нет в индексе (`exclude`, игнорируемые git каталоги не из `external`).

«Как работает X» в одном известном файле — просто прочитайте его; `outline` даст номера строк.

## Агенты и скрипты

- **Claude Code.** Плагин в начале сессии кладёт в контекст короткие правила (только там, где есть индекс) и
  добавляет подсказку, когда `grep`/`rg` ищет символ кода. В `CLAUDE.md` ничего не пишется.
- **Другие агенты (Cursor, Codex, Cline…).** MCP-сервер `ast-index-mcp`: [настройка MCP](mcp-setup.md)
  (на английском).
- **Скрипты.** `--format json` у любой команды. SQL по индексу: `ast-index schema`, затем
  `ast-index query 'SELECT …'`; `ast-index db-path` печатает путь к файлу базы.

## Если что-то не так

| Симптом | Причина и что сделать |
|---|---|
| «Index not found» | `ast-index rebuild` в корне проекта |
| Ответ не видит недавнее изменение | `ast-index watch-status`; `ast-index update` |
| Результатов меньше, чем ожидалось | Запускайте из корня проекта: из подкаталога вывод ограничен им |
| Класс «не найден» по FQN | FQN угадан: `ast-index class <КороткоеИмя>`; FQN — в кавычках |
| Не находится класс фреймворка | Его каталога нет в `external:` в `.ast-index.yaml` |
| Импорты JS через алиас попадают в `unresolved` | Добавьте префикс в `js_aliases` |
| Нет кода из скрытого каталога | Добавьте его в `include_hidden`, затем `rebuild` |
| После обновления ast-index ответы прежние | Watcher держит старый бинарь: остановите его (`pkill -f '^ast-index watch'`); следующая сессия запустит заново, или `ast-index watch &` |
