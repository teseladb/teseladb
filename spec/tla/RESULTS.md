# Resultados de TLC

Generado por `scripts/tlc.sh --all` el 31-10-2026 con TLA+ tools 1.7.4 (docker, todos los workers).
Cada modelo explica en su cabecera qué comprueba; `scripts/ci.sh` vuelve a generar esta tabla en la puerta 7.

| Modelo | Configuración | Estados generados | Estados distintos | Profundidad | Tiempo |
|--------|---------------|-------------------|-------------------|-------------|--------|
| `ApiKeyRevoke` | `ApiKeyRevoke.cfg` | 5.050.685 | 1.098.568 | 23 | 60 s |
| `AuthzFence` | `AuthzFence.cfg` | 273.009.289 | 48.955.400 | 26 | 749 s |
| `AuthzFence` | `AuthzFence.liveness.cfg` | 59.020.633 | 11.502.600 | 24 | 1265 s |
| `AuthzFenceLeaders` | `AuthzFenceLeaders.cfg` | 125.163.845 | 35.974.378 | 21 | 586 s |
| `ClosedTimestamp` | `ClosedTimestamp.cfg` | 158.317.457 | 17.315.656 | 25 | 385 s |
| `ClosedTimestamp` | `ClosedTimestamp.streams.cfg` | 503.370.352 | 51.869.224 | 32 | 1087 s |
| `CoreRelocate` | `CoreRelocate.cfg` | 154.193 | 44.316 | 31 | 5 s |
| `HlcLatches` | `HlcLatches.cfg` | 3.566 | 1.006 | 10 | 4 s |
| `HlcLease` | `HlcLease.cfg` | 101.694.096 | 47.323.569 | 15 | 1082 s |
| `HlcLease` | `HlcLease.back.cfg` | 238.565.421 | 113.603.871 | 15 | 2838 s |
| `HlcUncertainty` | `HlcUncertainty.cfg` | 70.912.878 | 39.096.622 | 19 | 231 s |
| `LeaseRead` | `LeaseRead.cfg` | 36.004.679 | 5.505.851 | 24 | 76 s |
| `Meta2Handoff` | `Meta2Handoff.cfg` | 122.689 | 27.216 | 17 | 3 s |
| `NodeJoin` | `NodeJoin.cfg` | 11.813 | 2.191 | 13 | 2 s |
| `OnlineIndexBuild` | `OnlineIndexBuild.cfg` | 669.176 | 239.462 | 20 | 4 s |
| `PrimaryReassign` | `PrimaryReassign.cfg` | 6.453.558 | 1.399.536 | 27 | 62 s |
| `RangeMerge` | `RangeMerge.cfg` | 199.924 | 45.414 | 16 | 2 s |
| `RangeSplit` | `RangeSplit.cfg` | 158.877 | 45.305 | 15 | 2 s |
| `RangeSplitMvcc` | `RangeSplitMvcc.cfg` | 422.892 | 199.812 | 17 | 3 s |
| `Reshard` | `Reshard.cfg` | 120.926 | 55.665 | 20 | 2 s |
| `SearchSegmentGc` | `SearchSegmentGc.cfg` | 21.830 | 6.097 | 9 | 0 s |
| `SingleServerChange` | `SingleServerChange.cfg` | 5.633.534 | 1.403.162 | 30 | 29 s |
| `Smoke` | `Smoke.cfg` | 5 | 4 | 4 | 3 s |
| `SnapshotAwait` | `SnapshotAwait.cfg` | 38.943 | 7.088 | 19 | 2 s |
| `SplitCompletion` | `SplitCompletion.cfg` | 1.043 | 559 | 24 | 1 s |
| `TxnCommit` | `TxnCommit.cfg` | 60.303.931 | 12.502.328 | 23 | 137 s |
| `WriteConcernW1` | `WriteConcernW1.cfg` | 16.564.351 | 2.349.474 | 21 | 31 s |

Ninguno encuentra errores.
