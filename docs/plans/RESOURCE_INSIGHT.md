# Развитие измерений нагрузки

Объём: восемь этапов из задания 2026-11-16. Работа в текущем checkout;
существующие clipboard, группировка и Windows VM сохраняются. Без публикации.

## Решения

Сбор остаётся вне GUI. Дополнительные измерения имеют явные типы и единицы;
отсутствующий ключ означает неизвестное значение. I/O процесса не связывается
предположительно с устройством. Windows GPU% — максимум движков, память —
раздельные dedicated/shared. Группы имён и дерево родителей — разные режимы.
История использует те же снимки, длительности, границы непрерывности и worker.
Миграции выполняются кодом приложения. Лимиты очередей, 512 МиБ и 1/3/7 дней
сохраняются. Новые поля имеют независимое покрытие.

## Этапы и проверки

- [x] 1. I/O процессов и физических устройств: источники Linux procfs/Windows API,
  дельты по времени жизни; сортировка, UI, CSV, история; сбросы/пропуски/reuse.
- [x] 2. Windows PDH GPU Engine/Process Memory: движки и адаптеры, независимая
  доступность; история/CSV; нативная проверка доступных счётчиков.
- [x] 3. Раскрываемые группы и отдельное дерево родства: кэш, виртуализация,
  поиск до группировки, явные строки экспорта без двойного суммирования.
- [x] 4. Подробная память и неизвестные CPU/RAM: точная семантика resident,
  private commit, shared/swap/faults; защищённые процессы/VM; история/CSV.
- [x] 5. Соединения выбранного процесса: Linux procfs/Windows IP Helper,
  IPv4/IPv6 TCP/UDP, фоновый ограниченный запрос с проверкой lifetime.
- [x] 6. Логические CPU: sysinfo, компактная сетка и сопоставление среднего
  с общим CPU; семантика процентов процессов сохраняется.
- [x] 7. Датчики/троттлинг: Linux hwmon/thermal и достоверные счётчики,
  Windows только доступные поддерживаемые источники; никаких новых драйверов.
- [x] 8. Правила длительной нагрузки: opt-in, пороги/длительность/cooldown,
  ограниченная очередь событий, разрыв на паузах/пропусках/reuse.

После каждого этапа: тесты соответствующего контракта, документация,
Windows EXE с уникальным именем в Downloads. Нативные API-тесты отделены
от unit и ручной приёмки GUI/оборудования. Итоговые обязательные команды:
fmt, Clippy, Rust tests, release Linux/Windows, Python packaging tests.

Baseline: HEAD 4ec8862f68623ca9075f48990c38b6aebb48421c, main, 0.5.0;
13 исходных изменённых/новых файлов; Linux 109 passed, 1 ignored.

Этап 1: 120 Linux passed, 1 ignored; 4 native Windows source tests;
Clippy и release Linux/Windows. Реальный write delta 65536 bytes обе ОС.

Этап 2: 127 Linux passed, 1 ignored; 6 native Windows GPU tests;
Windows Clippy/release. Сборка stage2-gpu-20261116-114240.exe.

Этап 3: 136 Linux passed, 1 ignored; Clippy Linux/Windows, обе release.
Сборка stage3-tree-20261116-114832.exe.

Этап4: 155 Linux passed1ignored; WindowsnativeProcessDetails3/ProcessSampler6;
Clippy обеих платформ/release. Сборка stage4-memory-20261116-120409.exe.

Этап5:166Linuxpass1ignore;nativeWindows4connectiontests;bothClippy/release.
Сборка stage5-connections-20261116-121546.exe.

Этап6:167Linuxpass1ignore; nativeWin16logical mean/global10.803%;bothClippy/release.
Сборка stage6-cpus-20261116-121937.exe.


Этап 7 завершён: реальные ACPI/NVIDIA и ограничения источников проверены;
CLI parent-death lifecycle дополнительно исправлен и проверен на обеих ОС.
Этап 8 завершён: правила, настройки, журнал, desktop delivery, graceful quit;
два найденных ревью P2 воспроизведены тестами и устранены.
Итог: Linux 210 passed / 2 ignored, native Windows 194 / 4 ignored;
fmt, Clippy/release обеих платформ и Python packaging 5 passed.
Сборка и полные границы проверки: docs/RESOURCE_VERIFICATION.md.
