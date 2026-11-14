# Cambios de teseladb-tls

## 0.2.0

- `ServerConfig::sni` y `SniIdentity`: un servidor envía a un cliente que pide un nombre (SNI) la cadena y la clave
  que sirven ese nombre. A cualquier otro le envía la de `identity`. Un nombre como `*.example.com` cubre una sola etiqueta y
  no cuenta la caja. Con TLS 1.2 el servidor lee el nombre antes de elegir la clave.
- `ServerConfig::identity_for` y `name_matches`: la identidad que corresponde a un nombre y la regla que lo decide.
- Quien construye `ServerConfig` con todos sus campos tiene que añadir `sni`; `ServerConfig::new` lo deja vacío.

## 0.1.0

- TLS 1.3 y 1.2 sin I/O, del lado del cliente y del servidor.
