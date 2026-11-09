# Resultados de TLC

Generado por `scripts/tlc.sh --all` el 04-11-2026 con TLA+ tools 1.7.4 (docker, todos los workers).
Cada modelo explica en su cabecera qué comprueba; `scripts/ci.sh` vuelve a generar esta tabla en la puerta 7.

| Modelo | Configuración | Estados generados | Estados distintos | Profundidad | Tiempo |
|--------|---------------|-------------------|-------------------|-------------|--------|
| `ApiKeyRevoke` | `ApiKeyRevoke.cfg` | 5.050.685 | 1.098.568 | 23 | 45 s |
| `AuthzFence` | `AuthzFence.cfg` | 273.009.289 | 48.955.400 | 26 | 301 s |
| `AuthzFence` | `AuthzFence.liveness.cfg` | 59.020.633 | 11.502.600 | 24 | 1203 s |
| `ClosedTimestamp` | `ClosedTimestamp.cfg` | 158.317.457 | 17.315.656 | 25 | 254 s |
| `ClosedTimestamp` | `ClosedTimestamp.streams.cfg` | 503.370.352 | 51.869.224 | 32 | 535 s |
| `CoreRelocate` | `CoreRelocate.cfg` | 154.193 | 44.316 | 31 | 2 s |
| `HlcLatches` | `HlcLatches.cfg` | 235.682 | 57.254 | 19 | 1 s |
| `PipelinedWrites` | `PipelinedWrites.cfg` | 2.353 | 846 | 20 | 1 s |
| `CatalogPins` | `CatalogPins.cfg` | 517.500 | 125.226 | 28 | 3 s |
| `HlcLease` | `HlcLease.cfg` | 101.694.096 | 47.323.569 | 15 | 336 s |
| `HlcLease` | `HlcLease.back.cfg` | 238.565.421 | 113.603.871 | 15 | 1284 s |
| `HlcUncertainty` | `HlcUncertainty.cfg` | 70.912.878 | 39.096.622 | 19 | 327 s |
| `LeaseRead` | `LeaseRead.cfg` | 36.004.679 | 5.505.851 | 24 | 57 s |
| `Meta2Handoff` | `Meta2Handoff.cfg` | 122.689 | 27.216 | 17 | 2 s |
| `NodeJoin` | `NodeJoin.cfg` | 11.813 | 2.191 | 13 | 1 s |
| `OnlineIndexBuild` | `OnlineIndexBuild.cfg` | 669.176 | 239.462 | 20 | 4 s |
| `PrimaryReassign` | `PrimaryReassign.cfg` | 6.453.558 | 1.399.536 | 27 | 55 s |
| `RangeMerge` | `RangeMerge.cfg` | 199.924 | 45.414 | 16 | 2 s |
| `RangeSplit` | `RangeSplit.cfg` | 158.877 | 45.305 | 15 | 2 s |
| `RangeSplitMvcc` | `RangeSplitMvcc.cfg` | 422.892 | 199.812 | 17 | 3 s |
| `Reshard` | `Reshard.cfg` | 120.926 | 55.665 | 20 | 2 s |
| `SearchSegmentGc` | `SearchSegmentGc.cfg` | 21.830 | 6.097 | 9 | 1 s |
| `SingleServerChange` | `SingleServerChange.cfg` | 5.633.534 | 1.403.162 | 30 | 27 s |
| `Smoke` | `Smoke.cfg` | 5 | 4 | 4 | 8 s |
| `SnapshotAwait` | `SnapshotAwait.cfg` | 38.943 | 7.088 | 19 | 5 s |
| `SplitCompletion` | `SplitCompletion.cfg` | 1.043 | 559 | 24 | 6 s |
| `TxnCommit` | `TxnCommit.cfg` | 60.303.931 | 12.502.328 | 23 | 201 s |
| `TxnCommit` | `TxnCommit.pushread.cfg` | 64.061.211 | 13.222.728 | 23 | 199 s |
| `WriteConcernW1` | `WriteConcernW1.cfg` | 16.564.351 | 2.349.474 | 21 | 51 s |

Ninguno encuentra errores.
