# Piano: condivisione vNext (share di thread, subtree e workspace)

Specifiche fornite da Alessandro Bruno. Ordine di lettura:

1. [01 — Requisiti, lessico e ambiti di condivisione](01-requisiti-lessico-scope.md)
2. [02 — Architettura target e flussi](02-architettura-flussi.md)
3. [03 — Modello dati, eventi, API e manifest](03-modello-dati-eventi-api-manifest.md)
4. [04 — Snapshot Git completo, trasporto incrementale e restore](04-snapshot-git-trasporto-restore.md)
5. [05 — Sincronizzazione, concorrenza, offline e conflitti](05-sync-concorrenza-offline-conflitti.md)
6. [06 — Sicurezza, privacy, ACL ed E2EE](06-sicurezza-privacy-acl-e2ee.md)

## Esempi

- [`examples/share-manifest.example.json`](examples/share-manifest.example.json) — coppia di `SignedEnvelope` (history + content) firmata con una chiave di test pubblica, verificata dal test `trama-domain::manifest::example_tests` (03 §2).
- [`examples/collaboration-servers.example.toml`](examples/collaboration-servers.example.toml) — profili server e policy enterprise concettuali (01 §1.7, 06 §2); non è ancora uno schema `config.json` supportato.

## Decisioni prese

- **UI:** si mantiene GPUI (anche per Linux e Windows).
- **Crittografia E2EE (M4):** Messaging Layer Security, [RFC 9420](https://www.rfc-editor.org/rfc/rfc9420), tramite un'implementazione esistente (es. `openmls`); niente protocollo fatto in casa. Resta obbligatoria la security review prima di dichiarare Cloud E2EE.
- **Piattaforme:** macOS, Linux e Windows sono tutti target di prima classe.
- **Discovery/eventi vNext (M3, scelte conservative):** `GET /.well-known/trama-collab` è pubblico e oggi annuncia solo il protocollo collab `2..=2` senza capability vNext (una capability compare solo quando esistono le rotte); `server_id` è impostabile dall'operatore (`--server-id` / `TRAMA_COLLAB_SERVER_ID`, 1–128 caratteri `[A-Za-z0-9-_.:]`), altrimenti casuale e persistito nel DB del servizio; la risposta ha `Cache-Control: public, max-age=300` (5 minuti, confermato); un 404 equivale a server v2 legacy; il daemon negozia con capability client vuote fino a M4. Gli eventi `ShareEvent*` hanno metadati di routing in chiaro e corpo `cipher_payload` opaco; intake idempotente su `event_id` con catena `author_seq` stretta per device (gap → backfill, riuso → rifiuto). Nessuna emissione dal server finché le rotte `/v3` non esistono (M4). Kind riservati all'owner (`ShareEventKind::owner_only`, confermato): `share_created`, `scope_expanded`, `thread_removed_from_scope`, `membership_changed`, `key_epoch_advanced`, `share_paused`, `share_closed`; gli altri possono essere scritti da membri con la capability adeguata. Kind con `thread_gid` obbligatorio: `thread_added`, `thread_removed_from_scope`, `history_appended`, `comment_changed`, `review_verdict_posted`.

## Decisioni M4 v1 (approvate)

- **Scout fuori dallo share:** un thread `scout` esterno al perimetro condiviso viene rifiutato (non entra nello snapshot).
- **Autorizzazione Git:** unico provider in v1 è GitHub OAuth; l'attestazione enterprise è rinviata.
- **Index Git in conflitto:** la condivisione è bloccata quando l'index ha conflitti, con un messaggio d'errore chiaro.

## Stato dell'implementazione

Requisito trasversale aggiunto: l'app deve essere **cross-platform**
(macOS, Linux, Windows), non macOS-first.

| Milestone | Contenuto | Stato |
|---|---|---|
| M1 | Contratti di dominio (`trama-domain::share`, `checkout_state`), snapshot/restore Git esatto (`trama-infra::share_git`), migration `0012_share_vnext`, CI matrice ubuntu/windows/macos sui crate core | fatto |
| M2 | Full-workspace builds on Linux/Windows/macOS; `trama-ipc` (Unix sockets / Windows named pipes); ConPTY terminals on Windows; per-OS data dir; `dunce` paths; CI on 3 OSes | done |
| Restructure | Codebase grouped into area modules, large files split, transitional re-exports removed ([restructure.md](restructure.md)) | done |
| M3 | Repository DB `trama-db::share`, manifest firmato (`SignedEnvelope`), protocollo vNext/discovery | fatto |
| M4 | Trasporto, E2EE MLS/openmls (dopo security review), wizard Share/Join | da fare |

## Decisioni M3 (DB share e manifest firmati)

Scelte conservative prese dove la spec lascia aperto:

- **Firma:** Ed25519 (`ed25519-dalek`), unico `alg` ammesso; nessun `none` né downgrade negoziabile. Le chiavi device e la loro distribuzione/rotazione restano M4 (MLS); oggi il verificatore usa un `Keyring` locale che fallisce chiuso.
- **Encoding canonico:** "Trama canonical JSON v1" = sottoinsieme di RFC 8785 (JCS): chiavi solo ASCII ordinate per byte, niente spazi, solo interi (float rifiutati), escape minimi. Il verificatore ri-codifica e pretende bytes identici. La firma copre una PAE con lunghezze (`TRAMA-ENVELOPE-V1 len(type) type len(payload) payload`), così una firma non è riusabile su un altro tipo di manifest.
- **Manifest separati:** `HistoryManifestV1` (perimetro, thread, cursor) e `ContentManifestV1` (repository, checkout, object index, content policy). Gli `origin_hint` dei repository stanno solo nel content manifest, perché rivelano dati di codice a chi ha solo `read_history`. Il content manifest è legato all'history manifest tramite SHA-256 dei suoi bytes canonici, stesso `share_id` e `snapshot_version`.
- **Firmatario = publisher:** il device della chiave deve coincidere con `publisher_device_id`; il campo `publisher_signature` dell'esempio vive nell'envelope, non nel payload (campi sconosciuti rifiutati).
- **Rollback:** `open_history` rifiuta versioni sotto il pin locale; nel DB `advance_share_snapshot` è CAS e solo crescente; uno share `closed` non si riapre.
- **DB:** generazioni di checkout immutabili e appese con CAS su `previous_generation`; il fingerprint di un server già pinnato non si può cambiare né rimuovere con un upsert; `byte_len` di un oggetto noto è immutabile.
