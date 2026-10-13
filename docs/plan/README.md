# Piano: condivisione vNext (share di thread, subtree e workspace)

Specifiche fornite da Alessandro Bruno. Ordine di lettura:

1. [01 — Requisiti, lessico e ambiti di condivisione](01-requisiti-lessico-scope.md)
2. [02 — Architettura target e flussi](02-architettura-flussi.md)
3. [03 — Modello dati, eventi, API e manifest](03-modello-dati-eventi-api-manifest.md)
4. [04 — Snapshot Git completo, trasporto incrementale e restore](04-snapshot-git-trasporto-restore.md)
5. [05 — Sincronizzazione, concorrenza, offline e conflitti](05-sync-concorrenza-offline-conflitti.md)
6. [06 — Sicurezza, privacy, ACL ed E2EE](06-sicurezza-privacy-acl-e2ee.md)

## Esempi

- [`examples/share-manifest.example.json`](examples/share-manifest.example.json) — manifest illustrativo (non firmato) citato in 03 §2.
- [`examples/collaboration-servers.example.toml`](examples/collaboration-servers.example.toml) — profili server e policy enterprise concettuali (01 §1.7, 06 §2); non è ancora uno schema `config.json` supportato.

## Decisioni prese

- **UI:** si mantiene GPUI (anche per Linux e Windows).
- **Crittografia E2EE (M4):** Messaging Layer Security, [RFC 9420](https://www.rfc-editor.org/rfc/rfc9420), tramite un'implementazione esistente (es. `openmls`); niente protocollo fatto in casa. Resta obbligatoria la security review prima di dichiarare Cloud E2EE.
- **Piattaforme:** macOS, Linux e Windows sono tutti target di prima classe.

## Stato dell'implementazione

Requisito trasversale aggiunto: l'app deve essere **cross-platform**
(macOS, Linux, Windows), non macOS-first.

| Milestone | Contenuto | Stato |
|---|---|---|
| M1 | Contratti di dominio (`trama-domain::share`, `checkout_state`), snapshot/restore Git esatto (`trama-infra::share_git`), migration `0012_share_vnext`, CI matrice ubuntu/windows/macos sui crate core | fatto |
| M2 | Build Linux/Windows dell'intero workspace; astrazione IPC daemon (UDS ↔ named pipe Windows); CI workspace su 3 OS | in corso |
| M3 | Repository DB `trama-db::share`, manifest firmato (`SignedEnvelope`), protocollo vNext/discovery | da fare |
| M4 | Trasporto, E2EE MLS/openmls (dopo security review), wizard Share/Join | da fare |
