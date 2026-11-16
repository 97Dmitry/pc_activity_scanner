# I/O процессов и накопителей

I/O процесса и активность устройства — независимые измерения. Приложение
не распределяет трафик диска между PID и не устанавливает, на каком диске
процесс выполнял конкретную операцию.

| Источник | Смысл | Ограничения |
|---|---|---|
| Linux `/proc/PID/io`, `read_bytes`/`write_bytes` | Байты storage I/O, учтённые ядром процессу; дельта / монотонный интервал | Не `rchar`/`wchar`; page cache, writeback и отменённые записи не совпадают с физическими операциями диска; чужие PID могут быть закрыты правами |
| Windows `GetProcessIoCounters` | `ReadTransferCount`/`WriteTransferCount`, дельта / интервал | Все операции I/O процесса, включая файловые, сетевые и device. Подпись «I/O», а не «физический диск» |
| Linux `/proc/diskstats` | 512-байтные секторы чтения/записи; доля времени I/O по `io_ticks` | Только целые leaf block devices с sysfs `device`; без разделов, loop/RAM, stacked aggregates. В WSL/VM устройства виртуальные, физический диск хоста не определяется |
| Windows PDH `PhysicalDisk(*)` | `Disk Read Bytes/sec`, `Disk Write Bytes/sec`, `100 - % Idle Time` | `_Total` исключается; счётчики могут быть отключены ОС/недоступны. Активность не доказывает насыщение параллельного SSD |

Работа без elevation: Linux чтение procfs/sysfs; Windows
`PROCESS_QUERY_LIMITED_INFORMATION` и обычный PDH query. Нет запуска помощников,
установки драйверов или изменения настроек счётчиков. Отказ доступа означает
неизвестный показатель. Ошибка открытия PDH повторяется не чаще 30 секунд.

Процессные baseline ограничены текущими успешно прочитанными процессами.
Идентификатор включает PID и native creation token: Linux start ticks, Windows
FILETIME. Linux читает `stat` до и после `io`, Windows читает время создания и
I/O через один открытый process handle. Неизвестный замер удаляет baseline.
Пауза/старт/пропуск более 15 секунд дают разрыв; первый следующий интервал
не создаёт скорость. Сброс каждого направления обрабатывается отдельно.

Linux device identity включает major/minor, имя и `diskseq`, если ядро его
предоставляет. Отсутствующие устройства удаляются из baseline. Значения
переполненных/сброшенных счётчиков не превращаются в скачок скорости.
PDH управляет своими интервалами и возвращает валидность экземпляров; wrapper
принимает только `VALID_DATA`/`NEW_DATA`, конечные неотрицательные значения.
Буфер PDH ограничен 8 МиБ, число экземпляров — 65536, устройств в снимке — 256.

## Выполненная проверка 2026-11-16

- Linux: пять тестов parser/lifetime/delta/pause/reset/gap; реальный короткий
  файловый write fixture 65536 байт в `target`, `write_bytes` вырос на 65536.
  `/tmp` здесь tmpfs: его запись закономерно не создаёт storage I/O. Счётчики
  шести виртуальных block devices WSL доступны, это не проверка физических
  дисков хоста из Linux.
- Windows: actual `src/pdh.rs` в нативном console harness возвращает диски
  `0 D:` и `1 C:` с read/write rates и idle time. Wildcard English paths
  с `PdhGetFormattedCounterArrayW` проверены на текущей Windows.
- Нативные тесты Windows процесса/устройства выполняются отдельно от ручной
  приёмки GUI и не меняют Task Scheduler или системную конфигурацию.

Источники семантики:
[Linux I/O statistics](https://docs.kernel.org/admin-guide/iostats.html),
[Linux procfs](https://docs.kernel.org/filesystems/proc.html),
[GetProcessIoCounters](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getprocessiocounters),
[PDH wildcard arrays](https://learn.microsoft.com/en-us/windows/win32/api/pdh/nf-pdh-pdhgetformattedcounterarrayw),
[PDH English counters](https://learn.microsoft.com/en-us/windows/win32/api/pdh/nf-pdh-pdhaddenglishcounterw).
