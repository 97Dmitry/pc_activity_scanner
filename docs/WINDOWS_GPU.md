# GPU и GPU-память Windows

Сборщик читает штатные WDDM performance counters через PDH, без администратора
и без установки драйверов:

- `\GPU Engine(*)\Utilization Percentage`;
- `\GPU Process Memory(*)\Dedicated Usage`;
- `\GPU Process Memory(*)\Shared Usage`.

Имена счётчиков передаются в `PdhAddEnglishCounterW`; имена экземпляров содержат
PID, LUID адаптера, индекс физического GPU и (для загрузки) движок. Сбор, чтение
и освобождение PDH выполняются вне GUI. Буферы PDH ограничены 8 МиБ и 65536
экземплярами на счётчик.

## Смысл показателей

GPU% процесса — максимум **доступных** процентов движков по всем адаптерам.
Например, 60% 3D и 40% Copy означают 60%, а не 100%. Это представитель наиболее
занятого движка, а не доля суммарной мощности нескольких GPU. Неисправный или
неподдерживаемый источник не заменяется нулём. Отсутствующие/невалидные
экземпляры PDH не доказывают отсутствие использования устройства.

Dedicated/shared — раздельные счётчики байтов. Каждый уникальный адаптер
`(LUID, physical GPU)` учитывается один раз. Повторённый экземпляр не увеличивает
сумму. Если у известного адаптера отсутствует значение данного счётчика, итог
этой метрики неизвестен. Legacy GPU RAM в Windows соответствует dedicated;
shared не добавляется к VRAM. На интегрированных GPU dedicated может быть нулём,
если это явно сообщает источник.

Эти значения — отчёт WDDM о ресурсах процесса. Разделяемые GPU-ресурсы могут
учитываться у нескольких процессов, поэтому сумма процессов не равна уникальной
физической памяти. Microsoft документирует ошибки GPU Process Memory на некоторых
конфигурациях Windows 10; рост такого счётчика сам по себе не доказывает утечку.

Первое измерение, пауза, большой разрыв, недоступная идентичность, смена PID lifetime
и выход процесса оставляют `—`. FILETIME создания проверяется до и после сбора;
нагрузка принимается только при совпадении с предыдущим измерением и строкой
процесса. Недоступная память не уничтожает независимо доступную GPU-загрузку.

История и CSV используют те же снимки. GPU% сохраняется существующим полем;
dedicated/shared — типизированными дополнительными метриками с независимым
покрытием. Отсутствующие значения не превращаются в нули.

## Источники

- [Microsoft DirectX: GPUs in the task manager](https://devblogs.microsoft.com/directx/gpus-in-the-task-manager/)
- [PdhAddEnglishCounterW](https://learn.microsoft.com/en-us/windows/win32/api/pdh/nf-pdh-pdhaddenglishcounterw)
- [Displaying performance data](https://learn.microsoft.com/en-us/windows/win32/perfctrs/displaying-performance-data)
- [Microsoft KB4490156: incorrect GPU process memory values](https://learn.microsoft.com/en-us/troubleshoot/windows-client/performance/gpu-process-memory-counters-report-wrong-value)

## Проверка

Нативная проверка источников 2026-11-16: Windows host с NVIDIA GeForce RTX 3070 Ti
Laptop и AMD Radeon. English wildcard counters доступны обычному пользователю;
две короткие выборки показали 800 engine и 62 memory экземпляра за sample,
валидные ненулевые проценты и раздельные dedicated/shared значения.
Нативный Rust-тест actual sampler + PDH затем получил 814 engine и по 63
memory экземпляра, 45 PID; второй sample выдал lifetime-проверенные GPU-метрики
для 32 из 32 доступных выбранных процессов. Шесть тестов модуля прошли,
включая этот native smoke. Linux pure tests модуля: пять прошли;
полный текущий Linux suite: 127 passed, 1 ignored.
Нагрузочные тесты, проверка всех драйверов и ручная приёмка GUI этим не заменяются.
