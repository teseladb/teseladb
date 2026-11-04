# TeselaDB

Base de datos documental compatible con el protocolo de RefDb 9.0 y con RefDb Search, distribuida en rangos
replicados con Raft, con un hilo por núcleo y en un único binario. Está escrita en Rust sin dependencias externas.

Las especificaciones están en `../teseladb-specs` (constitución, decisiones, divergencias y las especificaciones 001 a
019 con sus tareas). Este repositorio las implementa en el orden de su `README.md`.

## Estructura

```text
crates/   bibliotecas del workspace (lógica sans-I/O, E/S, servidor)
tools/    herramientas de desarrollo que no se distribuyen (simulador y mediciones)
scripts/  puertas de calidad
spec/tla/ modelos TLA+
```

## Desarrollo

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`ORIGIN.md` registra el código copiado de su proyecto de origen y `TRACKING.md` la evolución de las tareas.

## Licencia

TeselaDB se distribuye con la [Business Source License 1.1](LICENSE). Puedes usarlo gratis para desarrollar y probar,
y en producción si sus nodos suman 64 GB de RAM o menos y tu uso no es comercial o tu organización facturó menos de
1.000.000 € en su último ejercicio, siempre que no lo ofrezcas a terceros como servicio alojado o gestionado. Para
cualquier otro uso en producción hace falta una licencia comercial de VIUZI ESP, explicada en
[COMMERCIAL.md](COMMERCIAL.md). Cuatro años después de publicarse, cada versión pasa también a Apache 2.0 o GPL 2.0 o
posterior.
