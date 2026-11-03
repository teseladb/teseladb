# Scripts

## Fase final: `final-phase.sh`

Lanza los trabajos largos de la fase final en el orden del plan de `TRACKING.md` (orden del propietario del 01-11-2026)
con los parámetros de las tareas planificadas para esa fase. Cada trabajo es una fase con su registro y sus resultados en
un directorio de ejecución con fecha. Si se corta o una fase falla, la siguiente llamada sigue en el mismo directorio y
repite solo las fases que no han pasado.

```sh
scripts/final-phase.sh --dry-run                 # lo que haría, en orden, con la duración estimada y el total
scripts/final-phase.sh                           # empieza o reanuda la ejecución
scripts/final-phase.sh --only fuzz,tlc           # solo esas fases
scripts/final-phase.sh --from e2e-battery        # desde esa fase hasta el final
scripts/final-phase.sh --new                     # una ejecución nueva aunque la última no haya terminado
scripts/final-phase.sh --stop-on-failure         # para en la primera fase que falla (por defecto sigue)
scripts/final-phase.sh --quick --only <fase>     # comprobación corta de una fase con parámetros mínimos
```

Sale con 0 si pasan todas las fases elegidas, 1 si falla alguna y 2 si la llamada está mal.

### Fases, por paso del plan

| Paso | Fases | Qué lanza |
|------|-------|-----------|
| 0 | `build` | compila en release el simulador, el banco y el servidor |
| 1 | `fuzz` | 24 h por objetivo (`TESELADB_FUZZ_SECS=86400`), en paralelo, un objetivo por núcleo: los federados, la configuración y OCSP de 013, `forward_fuzz`, 016 T009 y los cuatro de 019 T034. Con `FINAL_FUZZ_ALL=1` también los que ya pasaron sus 24 h (006, 007 y los primeros de 013) |
| 2 | `mutants` | `scripts/sim-mutants.sh` completo |
| 3 | `sim-<carga>-<plan>`, `tlc` | 1 000 semillas de cada carga con los planes `none`, `light`, `default` y `heavy`; 5 000 con `default` en `search`, `vector` (011 T042) y `special-collections` (015 T018); después `scripts/tlc.sh --all` con su test |
| 4 | `coverage-<crate>`, `coverage-clean`, `ci` | `scripts/coverage.sh` crate a crate con el umbral del 95 %, borrando los perfiles tras cada uno; después `scripts/ci.sh` sin e2e (`TESELADB_E2E=0`) y sin las pasadas largas que ya han corrido (`TESELADB_CI_LONG=0`) |
| 5 | `e2e-battery`, `e2e-tools`, `e2e-drivers`, `chaos-*` | `tests/compat/run.sh` (todos los escenarios con los cuatro clientes), `tests/compat/tools/run.sh`, `tests/compat/drivers/run.sh`, y el caos de `tests/compat/chaos/`: 100 `SIGKILL` del líder (005 T029), 100 del nodo del flujo de cambios (014 T013), particiones, pausas, disco lleno y cuatro nodos |
| 6 | `perf-*` | escenarios de `teseladb-bench` de las tareas abiertas (002 T034, 004 T037, 007 T026, 008 T014 y T025, 009 T014, 010 T027, 011 T032, 012 T015, 015 T015, 017 T011, 019 T033), `insert-spread.sh` (007 T018), `add-node.sh` a 100 GB (007 T022) y `ycsb-compare.sh` con 3 y 9 nodos y con io_uring frente a hilos (012 T014, T016, T020). El escenario `reshard` de 018 T006 entra solo cuando el banco lo tenga |
| 6 | `integrity-*` | cortes simulados del log (20 000 semillas), del LSM (100 000), 500 `SIGKILL` reales del log y del LSM, y 1 000 semillas de todas las cargas con `heavy` al 200 % de fallos |

### Medidas de rendimiento

Cada medida de `perf-*` espera a que la máquina esté en reposo (carga por CPU de 0,25 o menos, hasta 30 min; si no,
la fase falla), fija los núcleos con `taskset` dejando libres los dos primeros, descarta 30 s de calentamiento y repite
la medida: cinco veces, o dos en los escenarios que cargan decenas de GB. Cada repetición deja su JSON y `spread.txt`
da por métrica la media, el mínimo, el máximo y la dispersión. Antes de cargar datos comprueba el espacio libre en
`FINAL_DATA_DIR`. macOS no tiene `taskset`: ahí las medidas fallan salvo con `FINAL_PERF_UNPINNED=1`, que lo anota en
el registro (no valen como medidas finales).

### Resultados

En `target/final-phase/<AAAA-MM-DD_HHMMSS>/` (o en `FINAL_REPORTS`): `<fase>.log` con la salida, `<fase>/` con lo que
la fase guarda (informes de la batería, de los drivers y de cobertura, JSON del banco, un `.log` y un `.ok` por objetivo
de fuzzing), `state.tsv` con cada intento, `summary.txt` con el último estado de cada fase, `runs.txt` con el commit de
cada arranque y `finished` cuando han pasado todas.

### Parámetros

`FINAL_FUZZ_SECS`, `FINAL_FUZZ_PARALLEL`, `FINAL_FUZZ_ALL`, `FINAL_JOBS`, `FINAL_SIM_SEEDS`, `FINAL_SIM_TASK_SEEDS`,
`FINAL_MUTANTS_ONLY`, `FINAL_MUTANT_SEEDS`, `FINAL_COMPAT_ARGS`, `FINAL_DRIVERS_ARGS`, `FINAL_CHAOS_TIMES`,
`FINAL_PERF_REPEATS`, `FINAL_PERF_REPEATS_LONG`, `FINAL_PERF_DURATION`, `FINAL_PERF_WARMUP`, `FINAL_PERF_CPUS`,
`FINAL_PERF_UNPINNED`, `FINAL_IDLE_MAX`, `FINAL_IDLE_WAIT`, `FINAL_DATA_DIR`, `FINAL_REPORTS`. `--quick` los baja a
una comprobación corta (5 s de fuzzing, 2 semillas, una vuelta de caos, 300 registros y 2 s de banco).

Nada de lo que lanza sube cambios, crea etiquetas, abre PR ni despliega: las imágenes de Docker son locales y los nodos
escuchan en la máquina. `scripts/tests/final_phase_test.sh` lo comprueba junto con el orden, los parámetros y la
reanudación.
