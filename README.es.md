# TeselaDB

[English](README.md) · **Español**

TeselaDB es una base de datos documental distribuida escrita en Rust. Un solo nodo ya es una base de datos completa, y
añades nodos cuando necesitas más capacidad o quieres seguir trabajando mientras uno de ellos está caído. Se distribuye
como un único binario para Linux y macOS y como imagen de Docker.

La web es [teseladb.com](https://teseladb.com), con la [documentación](https://teseladb.com/es/docs), el
[inicio rápido](https://teseladb.com/es/docs/get-started/quickstart-single-node), las
[páginas de compatibilidad](https://teseladb.com/es/docs) y las [descargas](https://teseladb.com/es/download). La
imagen de Docker es [`teseladb/teseladb`](https://hub.docker.com/r/teseladb/teseladb) en Docker Hub y
[`ghcr.io/teseladb/teseladb`](https://github.com/teseladb/teseladb/pkgs/container/teseladb) en GitHub.

## Instalación

```sh
curl -fsSL https://downloads.teseladb.com/install.sh | sh
```

El script de instalación detecta tu sistema y tu CPU, descarga el binario que les corresponde, comprueba su SHA-256 y
lo copia a `/usr/local/bin`. Si tu usuario no puede escribir en esa carpeta, el script ejecuta `sudo` y te pide la
contraseña. Para instalarlo en una carpeta tuya, pon esa carpeta en `TESELADB_INSTALL_DIR`. Los archivos de cada
versión, cada uno con su fichero `.sha256`, también están en [Releases](https://github.com/teseladb/teseladb/releases).

## Cómo está hecho

Cada colección se divide en rangos de claves. Cada rango es un pequeño grupo replicado con su propio log de Raft, así
que una escritura va al líder de su rango y se confirma cuando la mayoría de las copias de ese rango la tiene en disco.
Dentro de cada nodo corre un hilo por núcleo de CPU, dueño de sus rangos, sin cerrojos compartidos entre núcleos. En
Linux el motor hace su propia entrada y salida de disco y de red con `io_uring`, con ficheros `O_DIRECT` y su propia
caché de bloques. Los documentos viven en su propio árbol LSM con versiones MVCC, y la búsqueda de texto y vectorial
corre en el mismo binario sobre la misma instantánea que los documentos.

Toda la lógica son máquinas de estados sin entrada ni salida propias. Gracias a eso, un simulador determinista corre un
clúster entero con el código de producción, inyecta particiones, mensajes perdidos, escrituras rotas y caídas, y repite
cualquier fallo a partir de su semilla. Los protocolos tienen además modelos TLA+ en `spec/tla`, comprobados con TLC.

El motor de almacenamiento, Raft, el motor de consultas, TLS, X.509, BSON, la compresión y los hashes son código de este
repositorio, hecho a partir de los estándares y las especificaciones públicas. El workspace no tiene dependencias de
terceros: `Cargo.lock` solo contiene sus propios crates y la compilación no necesita red.

## Estructura del repositorio

```text
crates/   las bibliotecas del workspace: lógica sin E/S, entrada y salida, y el servidor
tools/    herramientas de desarrollo que no van en el binario: el simulador, el benchmark y los generadores de tablas
scripts/  puertas de calidad y scripts de publicación
spec/tla/ modelos TLA+
docker/   la imagen de publicación
```

## Compilar y probar

Necesitas Rust 1.98.0, que `rust-toolchain.toml` elige por ti a través de `rustup`.

```sh
cargo build --release -p teseladb-server
cargo test --workspace
scripts/ci.sh --quick    # formato, Clippy, comprobación de dependencias y tests
```

`scripts/ci.sh` sin argumentos pasa todas las puertas: formato, Clippy en el host y en x86_64, la comprobación de
dependencias, `shellcheck`, los tests, los cortes simulados y reales, la cobertura del 95 % de líneas, funciones y ramas
por crate, el simulador y TLC. La ejecución completa necesita Docker.

Para informar de un problema o hacer una pregunta, abre una issue en este repositorio.

## Licencia

TeselaDB tiene el código disponible con la [Business Source License 1.1](LICENSE). Puedes leer, compilar, modificar y
redistribuir el código, y usarlo fuera de producción sin límites. El uso en producción es gratis, también el comercial,
mientras tus instancias sumen 64 GB de RAM y 16 vCPU o menos y tu organización haya facturado menos de 1.000.000 € en su
último ejercicio. Ofrecer TeselaDB a terceros como servicio de base de datos necesita siempre una licencia comercial de
VIUZI ESP S.L., sea cual sea tu tamaño, igual que el uso en producción por encima de esos límites. Cuatro años después de
publicarse cada versión, esa versión pasa también a la Apache License, Version 2.0. [COMMERCIAL.md](COMMERCIAL.md)
explica las condiciones en lenguaje llano (en inglés).
