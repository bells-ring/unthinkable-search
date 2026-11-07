# Встраивание unthinkable-search в свои программы

Поиск файлов по имени для Windows (NTFS: чтение `$MFT` + журнал USN), доступный из любого языка.
Пять способов, от простого к самому быстрому:

| Способ | Когда выбирать | Задержка запроса | Живые обновления |
|---|---|---|---|
| `usc search … --format json` | скрипты, разовые запросы | ~20 мс (загрузка индекса) + поиск | нет (снимок индекса на диске) |
| `usc serve` (stdin/stdout) | любое приложение, любой язык: запустить дочерний процесс | ~0,1–0,6 мс + IPC (≈0,05–0,15 мс) | да |
| `usc serve --pipe` | несколько программ делят один сервер и один запрос UAC | ~0,1–0,6 мс + IPC (≈0,05–0,15 мс) | да |
| `unthinkable_search.dll` (C ABI) | C/C++/C#/Python/Delphi/Go — внутри своего процесса | ~0,1–0,5 мс | да |
| крейты Rust (`us-core`, `us`) | программы на Rust | ~0,1–0,5 мс | да (`us::service`) |

Требования: Windows 10/11 x64. Проверено на Windows 11 24H2 (26100).
Файлы, которые нужно положить рядом со своей программой:

| Файл | Нужен для |
|---|---|
| `usc.exe` | CLI и `serve` |
| `us.exe` | брокер прав (живые обновления без запуска всей программы от администратора) и GUI |
| `unthinkable_search.dll` | встраивание через C ABI |
| `data\index.usdb` | создаётся сам; путь меняется через `--data DIR` / `us_open(dir, …)` |

Внешних зависимостей нет: ни VC++ Redistributable, ни .NET. Программа ничего не пишет в реестр, `%APPDATA%` и `%TEMP%`.

---

## 1. Синтаксис запросов (общий для всех способов)

| Запрос | Смысл |
|---|---|
| `foo bar` | имя содержит `foo` **и** `bar` |
| `foo\|bar` | `foo` **или** `bar` |
| `!foo` | **не** содержит `foo` |
| `"a b"` | строка с пробелом |
| `*.pdf`, `a?c.txt` | маска на **всё** имя |
| `ext:pdf;docx` | по расширениям |
| `file:` / `folder:` | только файлы / только папки (`folder:windows`) |
| `windows\system32` | терм с `\` или `/` ищется по **полному пути** |

Регистр и диакритика не учитываются (`é` = `e`, `ё` = `е`). Пустой запрос возвращает всё.

## 2. Поля результата

| Поле | Тип | Значение |
|---|---|---|
| `full` | string | полный путь, `C:\Windows\notepad.exe` (корень тома — `C:\`) |
| `name` | string | имя; у объекта в Корзине — исходное имя, а `full` — настоящий путь (`…\$Recycle.Bin\<SID>\$R…`) |
| `path` | string | папка, в которой лежит объект (`C:\Windows`; у корня тома пусто) |
| `size` | number | байты; **у папок — сумма всех вложенных файлов** (жёсткие ссылки считаются по разу на каждое имя) |
| `mtime` | время | дата изменения |
| `ctime` | время | дата создания |
| `attrs` | number | атрибуты Windows (`FILE_ATTRIBUTE_*`: 0x1 только чтение, 0x2 скрытый, 0x4 системный, 0x10 папка, 0x400 точка повторной обработки…) |
| `dir` | bool | папка |

Время (`time`): `iso` (по умолчанию, `2024-09-06T04:02:26Z`, UTC, секунды), `filetime` (точное: число 100-нс интервалов с 1601-01-01 UTC), `unix_ms`. Если время неизвестно — `null` (в TSV/CSV — пусто).

Сортировка (`sort`): `name` (по умолчанию, бесплатно — индекс уже отсортирован), `path`, `size`, `mtime`, `ctime`; `desc` — по убыванию.

---

## 3. CLI: `usc search`

```text
usc search <запрос> [--format text|json|jsonl|tsv|csv|nul] [--fields full,size,mtime,...]
                    [--sort name|path|size|mtime|ctime] [--desc] [--offset N] [-n|--limit N]
                    [--time iso|filetime|unix_ms] [--count] [--data DIR]
```

* `text` (по умолчанию) — полный путь на строку, не больше 30 строк; число результатов выводится в stderr.
* `json` — один объект `{"total":N,"offset":0,"ms":0.3,"items":[...]}`.
* `jsonl` — объект на строку. `tsv` и `csv` — с заголовком. `nul` — пути, разделённые `\0` (для `xargs -0`).
* У машинных форматов лимита по умолчанию нет. `-n 0` — тоже без лимита. `--count` печатает только число.
* stdout в кодировке UTF-8; в машинных форматах stderr пустой.
* Индекс берётся из `data\index.usdb`. Его создают и обновляют `us.exe` (GUI), `usc serve` или `usc index` (последней нужны права администратора, иначе — медленный обход каталогов).

```powershell
usc search "ext:pdf отчёт" --format json --fields full,size,mtime --sort mtime --desc -n 20
usc search "*.log" --format csv --fields full,size > logs.csv
usc search "folder:node_modules" --count
```

`usc info --json` — сводка об индексе (`entries`, `volumes`).

---

## 4. Сервер: `usc serve`

Индекс загружается один раз и обновляется вживую. Запросы и ответы — **JSON, один объект на строку** (UTF-8, `\n`).

```text
usc serve [--live auto|off] [--pipe [ИМЯ]] [--data DIR]
```

* `--live auto` (по умолчанию). Если процесс запущен от администратора, журналы USN читаются в нём самом. Иначе один раз появляется запрос UAC для брокера (тот же exe с ключом `--broker`): брокер читает MFT/USN и передаёт серверу только метаданные. Если UAC отклонён, сервер работает с сохранённым индексом без обновлений.
* `--live off` — запроса UAC не будет. Используется сохранённый индекс; если его нет, он строится обходом каталогов.
* Без `--pipe` протокол идёт через stdin/stdout. Сервер завершается при EOF или по команде `{"cmd":"quit"}`. Логи пишутся только в stderr.
* `--pipe` — именованный канал `\\.\pipe\unthinkable-search` (или `\\.\pipe\ИМЯ`) для нескольких клиентов. Доступ только у текущего пользователя и только с этой машины. Если имя уже занято, сервер не запустится — подменить его нельзя. Сервер останавливается командой `{"cmd":"shutdown"}`; `quit` закрывает только своё соединение.

Сразу после подключения сервер присылает приветствие:

```json
{"event":"hello","version":"0.1.0","protocol":1,"pid":1234}
```

### Команды

| Запрос | Ответ |
|---|---|
| `{"cmd":"ping"}` | `{"ok":true}` |
| `{"cmd":"info"}` | `{"ok":true,"version":"0.1.0","protocol":1,"entries":477296,"generation":3,"state":"ready","live":true,"status":"…","volumes":[{"root":"C:","kind":"mft"}]}` |
| `{"cmd":"search","query":"…", …}` | см. ниже |
| `{"cmd":"subscribe"}` / `{"cmd":"unsubscribe"}` | `{"ok":true}`; включает/выключает события |
| `{"cmd":"quit"}` | `{"ok":true}`, соединение закрывается |
| `{"cmd":"shutdown"}` | `{"ok":true}`, сервер останавливается (сохраняет индекс) |

К любому запросу можно добавить `"id"` — любое JSON-значение; оно вернётся в ответе. На одном соединении ответы приходят в порядке запросов.

**search** — параметры (все, кроме `query`, необязательны):

| Параметр | По умолчанию | |
|---|---|---|
| `query` | — | строка запроса (раздел 1) |
| `offset` | 0 | пропустить N результатов |
| `limit` | 100 | сколько вернуть; `0` — все |
| `sort` / `desc` | `name` / `false` | раздел 2 |
| `fields` | все поля | массив `["full","size"]` или строка `"full,size"` |
| `time` | `iso` | `iso` / `filetime` / `unix_ms` |
| `wait_ms` | 30000 | сколько ждать первый индекс (первая индексация, UAC) |

```json
{"id":1,"cmd":"search","query":"*.pdf","sort":"mtime","desc":true,"limit":2,"fields":["full","size","mtime"]}
{"id":1,"ok":true,"total":5120,"offset":0,"count":2,"ms":0.412,"generation":3,"items":[{"full":"D:\\docs\\a.pdf","size":10240,"mtime":"2026-10-31T09:12:44Z"},{"full":"D:\\b.pdf","size":2048,"mtime":"2026-10-30T18:01:02Z"}]}
```

`total` — общее число совпадений, `count` — сколько вернулось в `items`, `ms` — время поиска и сортировки внутри сервера, `generation` — версия индекса (увеличивается при каждом живом обновлении).

**Ошибки:** `{"id":1,"ok":false,"error":"<код>","message":"…"}`. Коды:

| Код | Причина |
|---|---|
| `bad_json` | строка не является JSON |
| `bad_request` | не хватает параметров или они неверные |
| `unknown_cmd` | неизвестная команда |
| `not_ready` | индекс ещё не готов за `wait_ms` |

**События** (после `subscribe`) приходят отдельными строками без `id`:

```json
{"event":"index","generation":4,"entries":477301}
{"event":"status","state":"ready","live":true,"status":"Индекс: … · живые обновления через брокер"}
```

`index` означает, что на диске что-то изменилось: повторите запрос, если показываете результаты. `state`: `loading` → `indexing` (первая индексация) → `ready`.

### Примеры клиентов

* Python: [`examples/python/us_serve_client.py`](../examples/python/us_serve_client.py) (только стандартная библиотека).
* PowerShell / .NET: [`tools/serve-smoke.ps1`](../tools/serve-smoke.ps1) — stdio и named pipe.
* Node.js:

```js
const { spawn } = require("child_process");
const rl = require("readline");
const us = spawn("usc.exe", ["serve", "--live", "off"]);
const pending = new Map(); let next = 1;
rl.createInterface({ input: us.stdout }).on("line", (line) => {
  const m = JSON.parse(line);
  if (m.id !== undefined && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
});
const call = (req) => new Promise((ok) => { req.id = next++; pending.set(req.id, ok); us.stdin.write(JSON.stringify(req) + "\n"); });
call({ cmd: "search", query: "*.mp4", sort: "size", desc: true, limit: 5 }).then((r) => { console.log(r.items); us.stdin.end(); });
```

---

## 5. DLL: `unthinkable_search.dll` (C ABI)

Сборка: `cargo build --profile release-dll -p us-ffi` → `target\release-dll\unthinkable_search.dll`.
Заголовок: [`crates/us-ffi/include/unthinkable_search.h`](../crates/us-ffi/include/unthinkable_search.h).
Для живых обновлений без прав администратора положите `us.exe` рядом с DLL: это брокер.

Как устроено:

* `us_open(data_dir, flags)` запускает фоновый поток: загрузка или построение индекса, затем живые обновления (`US_OPEN_LIVE`). `data_dir = NULL` означает `<папка DLL>\data`.
* `us_search` возвращает **неизменяемый снимок** результатов: строки и размеры остаются согласованными, даже если тем временем пришло живое обновление. Освобождайте его через `us_results_free`.
* Все функции потокобезопасны. Внутренняя ошибка никогда не роняет процесс-хост: функция вернёт `NULL`/0, а текст ошибки можно получить через `us_last_error` (для каждого потока свой).
* Строки в UTF-8. Функции, возвращающие текст, копируют его в `(buf, cap)` и возвращают полную длину. Вызов с `cap = 0` позволяет узнать нужный размер.
* `us_set_callback(e, cb, user)`: `cb(user, US_EVENT_INDEX)` вызывается при новом снимке индекса, `US_EVENT_STATUS` — при смене состояния. Callback выполняется в потоке движка.
* `us_request(e, json)` принимает тот же JSON-протокол, что `usc serve` (`ping`, `info`, `search`). Строку ответа освобождайте через `us_free_string`.

```c
#include "unthinkable_search.h"
#include <stdio.h>

int main(void) {
    us_engine *e = us_open(NULL, US_OPEN_LIVE);
    if (!e || !us_wait_ready(e, 30000)) return 1;
    us_results *r = us_search(e, "ext:pdf", US_SORT_MTIME, 1);
    char path[4096];
    for (uint32_t i = 0; r && i < us_results_count(r) && i < 10; i++) {
        us_results_full_path(r, i, path, sizeof path);
        printf("%12llu  %s\n", (unsigned long long)us_results_size(r, i), path);
    }
    us_results_free(r);
    us_close(e); /* сохраняет индекс */
    return 0;
}
```

Обёртки:

* C#: [`examples/csharp/UnthinkableSearch.cs`](../examples/csharp/UnthinkableSearch.cs) (.NET Framework 4.x и .NET 6+). Её проверяет [`tools/dll-smoke.ps1`](../tools/dll-smoke.ps1).
* Python (ctypes): [`examples/python/us_dll.py`](../examples/python/us_dll.py).

---

## 6. Rust

```toml
[dependencies]
us = { git = "…", package = "us" }        # сервис индекса, вывод, serve
us-core = { git = "…", package = "us-core" } # индекс, поиск, сортировка (без зависимостей от Windows)
```

```rust
use us::service::{LiveMode, Options, Service};
use us_core::sort::SortKey;

let mut opts = Options::next_to_exe();
opts.live = LiveMode::Auto;
let svc = Service::start(opts, |_event| { /* новый снимок или статус */ });
let ix = svc.wait_index(std::time::Duration::from_secs(30)).expect("index");
let (ids, ms) = us::output::query_sorted(&ix, "*.pdf", SortKey::Modified, true);
let mut path = String::new();
for &id in ids.iter().take(10) {
    ix.full_path_into(id, &mut path);
    println!("{path}  {} B  ({ms:.2} ms)", ix.total_size(id));
}
svc.stop(); // сохраняет индекс
```

---

## 7. Права, UAC и общий индекс

* Читать `$MFT` и журнал USN может только администратор. Программа не ставит службу. Если права нужны, она запускает на время сеанса отдельный повышенный процесс — тот же exe с ключом `--broker` (один запрос UAC). Брокер передаёт только метаданные (имена, размеры, даты) по каналу, доступному лишь текущему пользователю, и завершается вместе с клиентом.
* Без прав и без индекса строится обход каталогов: это медленнее, и живых обновлений нет.
* GUI, `usc serve` и DLL могут пользоваться одной папкой `data`: каждый процесс сохраняет индекс атомарно (через свой временный файл и переименование). Но у каждого процесса свой брокер и, значит, свой запрос UAC. Чтобы запрос был один, запустите один `usc serve --pipe` и подключайте к нему все программы.
* Индекс хранится только в `data\index.usdb` (≈ 90 байт на файл). При изменении формата он пересобирается автоматически.
