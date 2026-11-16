# Память и доступность CPU процессов

CPU и resident RAM — `Option`, а не обязательный ноль. Первый интервал,
пауза, смена native lifetime и сброс счётчика оставляют CPU неизвестным;
текущее доступное значение памяти при этом показывается сразу. CPU процесса
по-прежнему нормализован по всем логическим процессорам хоста.

## Windows

`NtQuerySystemInformation(SystemProcessInformation)` даёт единый снимок имён,
родительских PID, FILETIME создания, CPU-time и памяти. Этот источник заменяет
строки sysinfo, в том числе у минимальных VM; измерения не требуют открытия
защищённых VM-дескрипторов или повышения прав. `vmmemWSL` сохраняет отображение
`WSL2 (vmmemWSL)`, остальные `vmmem*` не называются WSL.

- **RAM / resident** — `WorkingSetSize`, байты текущего working set.
  Может включать разделяемые страницы; сумма процессов не является уникальной
  занятой физической памятью системы.
- **Private commit** — `PrivatePageCount`, байты private committed memory,
  проверяемые относительно `GetProcessMemoryInfo(...).PrivateUsage`.
  Это обязательство памяти, а не текущий расход физической RAM.
- **Page faults/s** — дельта `PageFaultCount`; общая частота minor и major.
  Windows этот источник не разделяет по видам. Переполнение/сброс дают разрыв.
- **Swap/shared resident** отдельно из этого источника не выводятся.
  Commit и `PagefileUsage` нельзя переименовывать в фактический swap.

Исполняемый путь читается заново через `QueryFullProcessImageNameW` с проверкой
FILETIME того же handle. Недоступный путь пуст; устаревший кеш sysinfo не
переносится на новый процесс. Ошибка нативного снимка делает CPU/RAM неизвестными,
не подменяет их нулём. Idle без валидной lifetime не считается workload-процессом.

Парсер читает только фиксированный заголовок до `Threads`: VM может не иметь
элементов потока. Проверяются длины, смещения, UTF-16 и границы буфера;
выделение ограничено 64 МиБ. Это частично документированный native API;
изменение контракта будущей Windows должно приводить к недоступности источника,
а не к повышению прав или догадкам о данных.

## Linux

Сборщик читает `/proc/PID/stat`, `/proc/PID/status` и ссылку `/proc/PID/exe`, затем
повторяет `stat`. Exact start ticks должны совпасть; имя и PPID берутся из
этого же native lifetime. Поля CPU/faults используют только counters процесса,
не суммируют waited-for children.

- **RAM / resident** — `VmRSS`, байты (`kB` procfs = 1024 байта).
  Это приближённый kernel accounting resident set, без дорогого чтения smaps.
- **Shared resident** — `RssFile + RssShmem`, file-backed/shared resident.
  Включает и file-backed страницы приватных mapping; это не точная оценка
  только страниц, реально разделённых с другим процессом. Не суммировать
  как уникальную физическую память.
- **Swap** — `VmSwap`, swap приватных анонимных страниц; shmem swap не включён.
- **Minor faults/s** — дельта `minflt`, без I/O загрузки страницы.
- **Major faults/s** — дельта `majflt`, с загрузкой страницы; не байты I/O.
- **Private commit** неизвестен: `VmSize` не является commit.

Ограничения доступа и отсутствующие поля сохраняются как неизвестные. Завершившийся
PID удаляется из выборки; недоступные счётчики прерывают их baseline. Статусы
ограничены 64 КиБ, ledger ограничен текущими process lifetimes. Данные не читаются
из GUI, а дополнительные метрики проходят существующие историю/CSV с отдельным
покрытием. Высокий commit или частота faults сами по себе не устанавливают причину
задержек или наличие утечки.

## Источники

- [Microsoft: NtQuerySystemInformation](https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntquerysysteminformation)
- [GetProcessMemoryInfo](https://learn.microsoft.com/en-us/windows/win32/api/psapi/nf-psapi-getprocessmemoryinfo)
- [PROCESS_MEMORY_COUNTERS_EX](https://learn.microsoft.com/en-us/windows/win32/api/psapi/ns-psapi-process_memory_counters_ex)
- [Linux kernel: proc filesystem](https://www.kernel.org/doc/html/latest/filesystems/proc.html)

## Проверка

Unit tests покрывают warmup VM с доступной RAM, CPU/fault reset, pause, PID reuse,
отсутствующие поля, правильные индексы stat и имя с `)` внутри. Нативные тесты
читают собственную память; Windows сравнивает byte accounting снимка с PSAPI,
Linux проверяет resident/shared/swap и exact lifetime через реальные procfs.
Наличие native тестов не заменяет ручную приёмку защищённых процессов, всех VM
и разных версий Windows на реальном рабочем столе.

Нативная проверка 2026-11-16 в текущем Windows host: 338 процессов в снимке;
собственный working set 12 443 648 байт против PSAPI 11 386 880, private commit
3 596 288 против PSAPI 2 543 616. Разница около 1 МиБ соответствует временному
буферу снимка; unit smoke ограничивает расхождение 4 МиБ и выявляет ошибку
«страницы вместо байтов». На первом sample WSL2 имел RAM 14 317 465 600 байт
и неизвестный CPU; `vmmemCmZygote` имел RAM 8192 байта и неизвестный CPU.
Шесть Windows-тестов модуля прошли без открытия VM handles и без администратора.
Linux own-process smoke: resident 9 674 752 байта, shared/file-backed 8 548 352,
swap 0 (из явно доступного поля), exact lifetime start ticks подтверждён.
