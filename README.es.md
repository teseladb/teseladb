# TeselaDB

**Español** · [English](README.md)

TeselaDB es una base de datos documental distribuida compatible con el protocolo de MongoDB 9.0. Tus aplicaciones siguen
usando los drivers oficiales de MongoDB, `mongosh` y las herramientas de base de datos de MongoDB, sin cambiar su código
ni sus opciones de conexión más allá de la dirección del nodo.

Un solo nodo ya es una base de datos completa. Añades nodos cuando necesitas más capacidad o quieres seguir trabajando
mientras uno de ellos está caído. TeselaDB se distribuye como un único binario para Linux y macOS y como imagen de
Docker.

La web es [teseladb.com](https://teseladb.com), con la [documentación](https://teseladb.com/docs) y las
[descargas](https://teseladb.com/download). La imagen de Docker es
[`teseladb/teseladb`](https://hub.docker.com/r/teseladb/teseladb) en Docker Hub y
[`ghcr.io/teseladb/teseladb`](https://github.com/teseladb/teseladb/pkgs/container/teseladb) en GitHub.

## Inicio rápido

En unos cinco minutos instalas TeselaDB, arrancas un nodo en tu máquina y abres una sesión de `mongosh` en él. Un solo
nodo ya es una base de datos completa que guarda tus colecciones y atiende a los drivers de MongoDB.

Para seguirla necesitas Linux en x86_64 o aarch64 con glibc 2.28 o posterior, o macOS con Apple Silicon, con `curl` y
[`mongosh`](https://www.mongodb.com/docs/mongodb-shell/install/). En cualquier otro sistema, ve directamente a
[Con Docker](#con-docker), que solo necesita Docker y `mongosh`.

### Paso 1. Instala el binario

El script de instalación descarga el binario de tu sistema y tu CPU, comprueba su SHA-256 y lo copia a
`/usr/local/bin`. Si tu usuario no puede escribir en esa carpeta, el script usa `sudo` y te pide la contraseña. Pon una
carpeta tuya en `TESELADB_INSTALL_DIR` para instalarlo allí, o una versión en `TESELADB_VERSION` para instalar esa.

```sh
curl -fsSL https://downloads.teseladb.com/install.sh | sh
```

### Paso 2. Comprueba la versión

Esta orden escribe la versión que instalaste, como `teseladb 0.1.10`. Si instalaste el binario en una carpeta tuya, esa
carpeta tiene que estar en tu `PATH` para que tu shell encuentre la orden.

```sh
teseladb --version
```

### Paso 3. Arranca el nodo en segundo plano

El nodo guarda sus datos en `./data` y escribe su log en `teseladb.log`. No necesitas ningún fichero de configuración,
porque el nodo se protege solo en su primer arranque en una carpeta vacía.

```sh
teseladb start --dbpath ./data > teseladb.log 2>&1 &
```

### Paso 4. Espera a que esté listo

Esta orden mira el log una vez por segundo y termina cuando el nodo escribe `ready`. En un portátil tarda uno o dos
segundos.

```sh
until grep -q '^ready' teseladb.log; do sleep 1; done
```

### Lo que prepara el primer arranque

En su primer arranque en una carpeta vacía, el nodo activa TLS y los usuarios, crea una autoridad de certificación para
el clúster y una primera contraseña para el usuario `root`, y escribe las dos cosas en `./data`, donde están los dos
ficheros que usan los pasos siguientes para conectarse. Escucha solo en `127.0.0.1`, así que ninguna otra máquina llega
a él. La segunda tabla da los cuatro puertos en los que escucha.

| Fichero | Qué contiene |
| --- | --- |
| `pki/ca.pem` | La autoridad de certificación del clúster. Los clientes la necesitan para confiar en el nodo. |
| `root-credentials.txt` | El usuario `root` y su primera contraseña. Solo tu usuario puede leer el fichero. |

| Puerto | Para qué |
| --- | --- |
| 27017 | Clientes y drivers, siempre con TLS |
| 27100 | Tráfico entre nodos |
| 27200 | Administración con `teseladb admin`, `/metrics` y `/healthz` |
| 27080 | API REST y servidor MCP |

### Paso 5. Lee la contraseña de root

Esta orden lee la contraseña del usuario `root` de `root-credentials.txt` en una variable de la shell, así que la
contraseña no aparece en tu pantalla.

```sh
ROOT_PASSWORD=$(sed -n 's/^password=//p' ./data/root-credentials.txt)
```

### Paso 6. Abre mongosh

Esta orden abre `mongosh` en el nodo con el usuario `root`, con TLS y con la autoridad de certificación del nodo.

```sh
mongosh "mongodb://root:$ROOT_PASSWORD@127.0.0.1:27017/?tls=true&tlsCAFile=./data/pki/ca.pem&authSource=admin"
```

### Paso 7. Pruébalo

En la sesión de `mongosh`, inserta dos pedidos y busca los que pasan de 20. El `find` devuelve el pedido de Ana, el único
con un total mayor que 20.

```js
db.orders.insertMany([
  { customer: "Ana", total: 30 },
  { customer: "Luis", total: 12 },
]);
db.orders.find({ total: { $gt: 20 } });
```

### Conecta tu aplicación

Tu aplicación usa el driver oficial de MongoDB de su lenguaje con esta cadena de conexión, rellena con tu usuario, su
contraseña, la dirección del nodo y la ruta de `ca.pem`. Es la cadena de conexión de cualquier despliegue de MongoDB
con TLS.

```text
mongodb://<usuario>:<contraseña>@<dirección del nodo>:27017/?tls=true&tlsCAFile=<ruta de ca.pem>&authSource=admin
```

La cadena necesita `tls=true`, porque el puerto de clientes solo acepta TLS. El fichero `ca.pem` no es secreto, así que
puedes copiarlo a cada máquina donde corren tus aplicaciones. Para una aplicación, crea un usuario con solo los roles
que necesita y deja `root` para la administración.

### Con Docker

Con Docker no necesitas el binario ni glibc, porque el nodo corre en un contenedor de la imagen `teseladb/teseladb`, y
puedes seguir estos pasos en cualquier sistema con Docker. Cada paso ejecuta una sola orden en la misma terminal.

#### Paso 1. Arranca el contenedor

Esta orden arranca un nodo en un contenedor llamado `teseladb`, guarda sus datos en el volumen `teseladb-data` y publica
el puerto de clientes. La imagen es `teseladb/teseladb` en Docker Hub, y la misma imagen está en
`ghcr.io/teseladb/teseladb` en GitHub. Con `-p 27017:27017` el puerto queda abierto en todas las interfaces de tu
máquina, de modo que otras máquinas de tu red llegan al nodo, y para dejar el puerto solo en tu máquina escribes
`-p 127.0.0.1:27017:27017` en su lugar cuando ejecutas la orden.

```sh
docker run -d --name teseladb \
  -p 27017:27017 \
  -v teseladb-data:/data \
  teseladb/teseladb:latest \
  start --dbpath /data --bind_ip 0.0.0.0
```

#### Paso 2. Espera a que esté listo

Esta orden mira el log del contenedor una vez por segundo y termina cuando el nodo escribe `ready`.

```sh
until docker logs teseladb 2>&1 | grep -q '^ready'; do sleep 1; done
```

#### Paso 3. Lee la contraseña de root

Esta orden lee la contraseña del usuario `root` del fichero de credenciales del contenedor en una variable de la shell.

```sh
ROOT_PASSWORD=$(docker exec teseladb cat /data/root-credentials.txt | sed -n 's/^password=//p')
```

#### Paso 4. Copia la autoridad de certificación

`mongosh` corre fuera del contenedor. Necesita su propia copia de `ca.pem` para confiar en el nodo, y esta orden saca
el fichero del contenedor a la carpeta en la que estás.

```sh
docker cp teseladb:/data/pki/ca.pem ./ca.pem
```

#### Paso 5. Abre mongosh

Esta orden abre `mongosh` en el nodo con el usuario `root`, con TLS y con tu copia de `ca.pem`. Desde aquí puedes probar
los pedidos del [paso 7](#paso-7-pruébalo) de arriba.

```sh
mongosh "mongodb://root:$ROOT_PASSWORD@127.0.0.1:27017/?tls=true&tlsCAFile=./ca.pem&authSource=admin"
```

### Para y limpia

Sal de `mongosh` con `exit` y sigue la parte que corresponde a cómo arrancaste el nodo, con el binario o con Docker.
Las órdenes quitan todo lo que creó este inicio rápido, así que no queda nada de la prueba en tu máquina.

#### Si usaste el binario

La primera orden para el nodo y la segunda borra su carpeta de datos con su log, de modo que no queda nada de esta
prueba en tu disco.

```sh
pkill -f "teseladb start --dbpath ./data"
rm -rf ./data teseladb.log
```

#### Si usaste Docker

Las órdenes quitan el contenedor, después su volumen con los datos y por último tu copia de `ca.pem`.

```sh
docker rm -f teseladb
docker volume rm teseladb-data
rm ./ca.pem
```

### Si algo falla

Si `mongosh` no se conecta, lee `teseladb.log`, o `docker logs teseladb` si usaste Docker. El nodo escribe ahí lo que
le falta. Si la conexión se rechaza, comprueba que la cadena tiene `tls=true` y la ruta correcta de `ca.pem`. Si el
script de instalación dice `there is no binary for` tu sistema, TeselaDB aún no tiene binario para él. En ese sistema
arrancas TeselaDB con Docker, como explica la sección anterior.

## API REST y servidor MCP

Cada nodo sirve también una API REST y un servidor MCP en el puerto 27080, con las mismas órdenes y los mismos permisos
que el protocolo. Este ejemplo lo hace todo con Docker: arranca un nodo con ese puerto publicado, crea una clave de API
y llama a los dos.

### Arranca un nodo con el puerto REST

```sh
docker run -d --name teseladb -p 27017:27017 -p 27080:27080 -v teseladb-data:/data \
  teseladb/teseladb:latest start --dbpath /data --bind_ip 0.0.0.0
until docker logs teseladb 2>&1 | grep -q '^ready'; do sleep 1; done
docker cp teseladb:/data/pki/ca.pem ./ca.pem
```

### Crea una clave de API

`teseladb admin` corre dentro del contenedor y escribe la clave nueva, que esta orden guarda en una variable de la shell.
La clave tiene los roles del usuario `root`; para una aplicación, crea un usuario con solo los roles que necesita y una
clave para él.

```sh
export TESELADB_API_KEY=$(docker exec teseladb teseladb admin --dbpath /data apikey create --user root --db admin)
```

### Llama a la API REST

Cada ruta es la orden del mismo nombre. La primera llamada inserta un pedido y la segunda lo encuentra.

```sh
curl --cacert ca.pem -X POST https://127.0.0.1:27080/v1/databases/shop/collections/orders/insertOne \
  -H "Authorization: Bearer $TESELADB_API_KEY" -H "Content-Type: application/json" \
  -d '{"document": {"customer": "Ana", "total": 30}}'
curl --cacert ca.pem -X POST https://127.0.0.1:27080/v1/databases/shop/collections/orders/find \
  -H "Authorization: Bearer $TESELADB_API_KEY" -H "Content-Type: application/json" -d '{"filter": {}}'
```

### Conecta un agente de IA por MCP

El servidor MCP tiene las herramientas de base de datos de MongoDB MCP Server, con los mismos nombres y argumentos
(`find`, `aggregate`, `count`, `collection-schema` y el resto). Arranca en solo lectura: las herramientas que escriben
aparecen cuando las activas con el parámetro de clúster `mcpOptions`. Un cliente MCP puede hablar con
`https://<nodo>:27080/mcp` con la clave en `Authorization: Bearer`, o lanzar el servidor por stdio. Con Docker, esta es
la entrada para los `mcpServers` de tu cliente, como Claude Desktop, Claude Code o Cursor; cambia `tdb_…` por tu clave.

```json
{
  "mcpServers": {
    "teseladb": {
      "command": "docker",
      "args": ["exec", "-i", "-e", "TESELADB_API_KEY", "teseladb", "teseladb", "mcp",
               "--uri", "https://127.0.0.1:27080", "--tlsCAFile", "/data/pki/ca.pem"],
      "env": { "TESELADB_API_KEY": "tdb_…" }
    }
  }
}
```

La [referencia de la API REST](https://teseladb.com/es/docs/reference/rest-api) y la
[página del servidor MCP](https://teseladb.com/es/docs/ai/mcp-server) explican cada ruta y cada herramienta.

## Instalación

| Plataforma | Cómo instalarlo |
| --- | --- |
| Linux x86_64 o aarch64 con glibc 2.28 o posterior, macOS con Apple Silicon | El script de arriba, o el archivo de tu plataforma en [Releases](https://github.com/teseladb/teseladb/releases) |
| Cualquier sistema con Docker | `docker pull teseladb/teseladb:latest` desde Docker Hub, o `docker pull ghcr.io/teseladb/teseladb:latest` desde GitHub |

Cada archivo de Releases va con su fichero `.sha256`. Para comprobar un archivo a mano, ejecuta esto en la carpeta
donde descargaste los dos ficheros.

```sh
shasum -a 256 -c teseladb-0.1.10-linux-x86_64.tar.gz.sha256
```

## Tus clientes de MongoDB funcionan tal cual

Para tu aplicación, un nodo de TeselaDB es como el router de un clúster de MongoDB con sharding. Los drivers lo
encuentran con el mismo saludo `hello` y le mandan los mismos comandos, así que el código que escribiste para MongoDB
funciona sin cambios. Cada comportamiento se mide contra un servidor MongoDB 9.0 real, comando a comando y código de
error a código de error. Donde TeselaDB responde distinto, la documentación da la diferencia y su motivo en sus
[páginas de compatibilidad](https://teseladb.com/docs).

## Cómo está hecha y cómo se compara con MongoDB

TeselaDB se escribió desde cero en Rust. Su motor de almacenamiento, el consenso Raft, el motor de consultas, TLS, X.509
y BSON son código propio, hecho a partir de los estándares y las especificaciones públicas, sin nada tomado del
servidor de MongoDB y sin bibliotecas de terceros.

Cada colección se divide en rangos de claves. Cada rango es un pequeño grupo replicado con su propio log de Raft, así
que una escritura va al líder de su rango y se aplica cuando la mayoría de las copias de ese rango la tiene. Dentro de
cada nodo corre un hilo en cada núcleo de la CPU, dueño de sus rangos, sin locks compartidos entre núcleos. Toda la
lógica son máquinas de estados sin entrada ni salida propias, y eso permite que un simulador determinista ejecute un
clúster entero con el código de producción, repita cualquier fallo desde su semilla y compruebe el resultado.

El diseño interno se aparta de MongoDB en algunos puntos que puedes notar.

| | MongoDB | TeselaDB |
| --- | --- | --- |
| Papeles | Un replica set tiene un primario; un clúster con sharding añade routers `mongos` y servidores de configuración | Todos los nodos ejecutan el mismo binario con el mismo papel. Cualquier nodo atiende clientes |
| Replicación | Un oplog por replica set, copiado desde el primario | Un log de Raft por rango, así que los líderes de rangos distintos pueden estar en nodos distintos |
| Crecer | Eliges una clave de sharding y pones en marcha el balanceador | Los rangos se dividen al crecer o al tener mucha carga y se fusionan al quedarse pequeños. Sus copias y sus líderes se reparten entre los nodos por carga |
| Lecturas con `readConcern: "local"` | Pueden devolver una escritura `w: 1` que una conmutación por error deshace después | Solo devuelven escrituras que tiene la mayoría de las copias |
| Índices de búsqueda | Se actualizan después de la escritura, con consistencia eventual | Leen la misma instantánea que los documentos que indexan |
| Primer arranque | Sin TLS ni usuarios hasta que los configuras | TLS y usuarios activos desde el primer arranque |

Cada nodo sirve también una API REST y un servidor MCP, con los mismos comandos y los mismos permisos que el protocolo
de MongoDB.

## Funciones y hoja de ruta

Aquí tienes el motor por dentro, capa a capa, con el estado de cada parte. «0.1.13» quiere decir que está en la
versión que puedes descargar hoy. «Próxima versión» quiere decir que está escrito y probado y saldrá en la siguiente.
«En curso» quiere decir que se está escribiendo ahora.

### Ejecución y entrada/salida

Un hilo por cada núcleo de la CPU es dueño de sus datos y habla con los demás por colas sin cerrojos, así que un núcleo
nunca espera a otro y sus datos se quedan en las cachés de su CPU. El motor hace su propia entrada/salida de disco y de
red con `io_uring`, con los ficheros abiertos con `O_DIRECT` y su propia caché de bloques, para que la caché de páginas
de Linux no añada una segunda copia ni una pausa inesperada.

| Característica | Estado |
| --- | --- |
| Un hilo por núcleo, sin cerrojos compartidos entre núcleos | 0.1.13 |
| Entrada/salida propia con `io_uring`, ficheros con `O_DIRECT` y caché de bloques por núcleo | 0.1.13 |
| Colas de disco separadas para el log y para los datos, con el log primero y un reparto del ancho de banda para el trabajo de fondo | Próxima versión |
| Control de admisión que rechaza trabajo nuevo cuando la cola de un núcleo espera más de 5 ms; las operaciones cortas pasan antes que los recorridos largos | Próxima versión |
| Conexiones de los clientes en anillos de `io_uring` (aceptación y lectura multishot, anillos de búferes provistos), con `SQPOLL` opcional | Próxima versión |
| Búferes y caché de bloques en páginas grandes de 2 MiB | Próxima versión |
| Páginas de 1 GiB para la caché de bloques, y un tope a la escritura de fondo mientras esperan lecturas de clientes | En curso |
| Red sin pasar por el kernel con `AF_XDP` | En curso |

### Durabilidad y replicación

Cada rango de claves es su propio grupo de Raft, así que los líderes de rangos distintos se reparten entre los nodos y
una escritura solo espera a las copias de su rango. Una escritura se confirma cuando la mayoría de las copias la tiene
en disco.

| Característica | Estado |
| --- | --- |
| Multi-Raft por rangos con lecturas con concesión | 0.1.13 |
| Rangos que se dividen y se fusionan según su tamaño y su carga | 0.1.13 |
| Freno gradual de escritura (−20 % / +25 %) según la deuda de compactación | 0.1.13 |
| Ficheros enteros borrados de golpe cuando un rango deja un núcleo | 0.1.13 |
| Un log compartido por disco para todos los núcleos, con un `fsync` por grupo de escrituras y el grupo siguiente escrito mientras se sincroniza el anterior | Próxima versión |
| Rangos movidos entre núcleos con enlaces duros a sus ficheros, sin copiar | Próxima versión |
| Ficheros de un rango enviados a otro nodo con `SEND_ZC`, sin copiarlos en el proceso | Próxima versión |

### Motor de almacenamiento

TeselaDB guarda los documentos en su propio árbol LSM. Las escrituras son secuenciales, lo que va bien a los discos
NVMe y a las cargas de mucha ingesta. La compactación es por tamaños en los niveles de arriba, para absorber los picos,
y por niveles abajo, para que las lecturas y el espacio en disco se mantengan bajos.

| Característica | Estado |
| --- | --- |
| Árbol LSM con versiones MVCC y compresión de bloques (zlib por defecto) | 0.1.13 |
| Compactación por tamaños arriba y por niveles abajo; LZ4 y zstd con un diccionario por colección | Próxima versión |
| Bloques uno detrás de otro en el disco, sin rellenar cada uno hasta una página | Próxima versión |
| Versiones viejas guardadas como diferencia frente a la versión siguiente | Próxima versión |
| Valores de más de 4 KiB en ficheros de valores aparte, para que la compactación no los reescriba | Próxima versión |
| Filtros Ribbon, un índice aprendido (PGM) y Elias-Fano sobre los bloques de cada fichero, y mapas de zonas por bloque que permiten a los recorridos saltar bloques | Próxima versión |
| Memtable sobre un árbol radix adaptativo | Próxima versión |
| Documentos guardados con una tabla de desplazamientos de sus campos, un diccionario de nombres de campo por colección y empaquetados según el esquema dominante de la colección | Próxima versión |

### Motor de consultas

Una consulta lee los campos directamente de los bytes guardados, sin construir documentos en memoria. Sus filtros se
compilan una vez por forma de consulta a un bytecode de registros y se ejecutan sobre lotes de documentos. El
planificador elige entre los planes candidatos como MongoDB, así que `explain` muestra las mismas formas de plan.

| Característica | Estado |
| --- | --- |
| Planificador con caché de planes, intersección de índices y la elección de plan de MongoDB | 0.1.13 |
| Lectura de BSON sin copias, directamente del búfer de red y de los bloques guardados | Próxima versión |
| Máquina de bytecode de registros para filtros y `$expr`, guardada por forma de consulta, sobre lotes de 64 documentos | Próxima versión |
| Núcleos SIMD (NEON, SSE2, AVX2) para comparaciones y `$group`, radix sort y top-k | Próxima versión |
| Agregaciones repartidas entre los núcleos y los líderes de otros nodos; filtros y búsquedas por índice enviados al nodo que tiene los datos | Próxima versión |
| Roaring bitmaps para `$and`, `$or` e `$in` | Próxima versión |
| `$project` en la máquina de bytecode, el resto del planificador por coste, una copia columnar de los bloques fríos para analítica y la precarga en los recorridos de índices | En curso |

### Búsqueda

Un documento se puede buscar en cuanto su escritura se confirma, porque la búsqueda de texto completo y la vectorial
van dentro del mismo binario y leen la misma instantánea que los documentos. Cada copia de un rango construye su propio
índice de búsqueda a partir del mismo log replicado, así que la búsqueda sigue funcionando cuando un líder pasa a otro
nodo.

| Característica | Estado |
| --- | --- |
| `$text`, `$search`, `$searchMeta` y `$vectorSearch` | 0.1.13 |
| Segmentos de búsqueda inmutables por rango, diccionario de términos como transductor de estados finitos (búsqueda exacta, por prefijo, difusa y por expresión regular), BM25 con Block-Max WAND y HNSW para los vectores | 0.1.13 |
| Descompresión SIMD de las listas de documentos, un tokenizador que no reserva memoria por término, una tabla de términos frecuentes, filtros indexados aplicados dentro de la búsqueda de los mejores resultados y resultados entregados por lotes | En curso |

### Verificación

Cada capa es una máquina de estados sin entrada/salida propia, así que el mismo código de producción corre en
un simulador determinista que provoca particiones de red, mensajes perdidos y desordenados, escrituras a medias y
caídas. Cualquier fallo se repite a partir de su semilla. Son herramientas de desarrollo y no van en el binario.

| Herramienta | Estado |
| --- | --- |
| Simulador determinista con inyección de fallos y comprobador de linealizabilidad | En uso |
| Modelos TLA+ de los protocolos del sistema, comprobados con TLC | En uso |
| Fuzzing diferencial de consultas contra un servidor MongoDB 9.0 real | En uso |
| Banco de carga a tasa fija con histogramas HDR hasta p99,99, en el mismo hardware que MongoDB | En uso |

## Estructura del repositorio

```text
crates/   las bibliotecas del workspace: lógica sin E/S, entrada y salida, y el servidor
tools/    herramientas de desarrollo que no van en el binario: el simulador, el benchmark y los generadores de tablas
scripts/  puertas de calidad y scripts de publicación
spec/tla/ modelos TLA+
docker/   la imagen de publicación
```

## Compilar y probar

`rust-toolchain.toml` fija Rust 1.98.0, que `rustup` instala por ti en la primera compilación. El workspace no tiene
dependencias de terceros, así que compila sin red. Estas tres órdenes compilan el servidor, pasan los tests y pasan las
puertas rápidas.

```sh
cargo build --release -p teseladb-server
cargo test --workspace
scripts/ci.sh --quick    # formato, Clippy, comprobación de dependencias y tests
```

`scripts/ci.sh` sin argumentos pasa todas las puertas: formato, Clippy en el host y en x86_64, la comprobación de
dependencias, `shellcheck`, los tests, los cortes sobre un disco simulado y sobre uno real, la puerta de cobertura del
95 % por crate, el simulador y TLC. La ejecución completa usa Docker para los tests de x86_64 y para `shellcheck`.

Para informar de un problema o hacer una pregunta, abre una issue en este repositorio. [CONTRIBUTING.md](CONTRIBUTING.md)
explica (en inglés) cómo proponer un cambio.

## Licencia

TeselaDB tiene el código disponible con la [Business Source License 1.1](LICENSE). Fuera de producción puedes cambiar el
código y redistribuirlo sin límites. El uso en producción es gratis, también el comercial, mientras tus nodos sumen 64 GB
de RAM y 16 vCPU o menos y tu organización haya facturado menos de 1.000.000 € en su último ejercicio. El uso en
producción por encima de esos límites necesita una licencia comercial de VIUZI ESP S.L. Ofrecer TeselaDB a terceros como
servicio de base de datos la necesita siempre, sea cual sea tu tamaño. Cuatro años después de publicarse cada versión,
esa versión pasa también a la Apache License, Version 2.0. [COMMERCIAL.md](COMMERCIAL.md) explica las condiciones en
lenguaje llano (en inglés).
