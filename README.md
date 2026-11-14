# TeselaDB

Base de datos documental compatible con el protocolo y la búsqueda del servidor de referencia que nombran las
especificaciones, distribuida en rangos replicados con Raft, con un hilo por núcleo y en un único binario. Está escrita
en Rust sin dependencias externas.

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

La evolución de las tareas está en `docs/seguimiento-codigo.md` de `teseladb-specs`.

## Licencia

TeselaDB se distribuye solo como binario e imagen de Docker, con la [TeselaDB License](LICENSE). Es gratis para
desarrollar y probar, y también en producción, incluido el uso comercial, si tus nodos suman 32 GB de RAM o menos y tu
organización facturó menos de 1.000.000 € en su último ejercicio. Si superas cualquiera de los dos límites, hace falta
una licencia comercial de VIUZI ESP S.L. En ningún caso puedes ofrecerlo a terceros como servicio alojado o gestionado,
modificarlo ni redistribuirlo sin esa licencia, que explica [COMMERCIAL.md](COMMERCIAL.md).
