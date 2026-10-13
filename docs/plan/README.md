# Piano: condivisione vNext (share di thread, subtree e workspace)

Specifiche fornite da Alessandro Bruno. Ordine di lettura:

1. [01 — Requisiti, lessico e ambiti di condivisione](01-requisiti-lessico-scope.md)
2. [02 — Architettura target e flussi](02-architettura-flussi.md)
3. [03 — Modello dati, eventi, API e manifest](03-modello-dati-eventi-api-manifest.md)
4. [04 — Snapshot Git completo, trasporto incrementale e restore](04-snapshot-git-trasporto-restore.md)
5. [05 — Sincronizzazione, concorrenza, offline e conflitti](05-sync-concorrenza-offline-conflitti.md)
6. [06 — Sicurezza, privacy, ACL ed E2EE](06-sicurezza-privacy-acl-e2ee.md)

## Stato dell'implementazione

Requisito trasversale aggiunto: l'app deve essere **cross-platform**
(macOS, Linux, Windows), non macOS-first.

| Milestone | Contenuto | Stato |
|---|---|---|
| M1 | Contratti di dominio (`trama-domain::share`, `checkout_state`), snapshot/restore Git esatto (`trama-infra::share_git`), migration `0012_share_vnext`, CI matrice ubuntu/windows/macos sui crate core | in corso (PR draft) |
| M2 | Build Linux dell'intero workspace; astrazione IPC daemon (UDS → named pipe su Windows) | da fare |
| M3 | Repository DB `trama-db::share`, manifest firmato (`SignedEnvelope`), protocollo vNext/discovery | da fare |
| M4 | Trasporto, E2EE (dopo security review), wizard Share/Join | da fare |
