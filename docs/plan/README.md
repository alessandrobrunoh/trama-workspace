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
- **M4a, scelte conservative (in corso):** crate `trama-e2ee` su `openmls` 0.8 + `openmls_rust_crypto` (puro Rust, nessun C/asm), unica ciphersuite `MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519` non negoziabile; un gruppo MLS per share (group id = `share_id`), una leaf per device (credential basic = device id); wire format solo ciphertext anche per i commit; ratchet tree nel Welcome; il commit viene applicato localmente subito (se il server lo rifiuta si ripristina lo snapshot precedente); niente proposal standalone; `cipher_payload` = application message con AAD = metadati di routing canonici dell'evento. Lo stato di epoca è uno snapshot segreto opaco dietro `EpochStateStore` (va cifrato a riposo, mai caricato). Secret scanner `trama-domain::secrets`: ogni finding blocca lo share (nessun override in v1), file > 8 MiB non scansionati = bloccati, i finding non contengono mai il segreto. Conflitti: `trama-infra::git::conflicts::unmerged_paths`. Aperti: binding device↔account GitHub, ordinamento dei commit lato server, rotazione key package, cifratura a riposo dello snapshot, security review.

- **M4d, wizard desktop (scelte conservative):** `apps/trama-desktop/src/collab/wizard/` (state machine pure + viste GPUI + `route` per le risposte del daemon, protocollo 14). Default: scope `thread_only`, invitati `reader` senza `read_code` (il codice va abilitato per persona), `owner` mai delegabile; ogni blocker del preflight (conflitti, segreti con hint redatto, scout fuori share, server/perimetro/checkout) blocca senza override e si ripete con «Re-run checks»; tornare indietro scarta il report; risposte stantie o di altro scope ignorate. Il join usa lo `share_id` (nessun formato link vNext ancora definito) e procede: chiave pubblicata → ammissione dell'owner (polling manuale «Check again», un errore lascia in attesa) → download/verifica → pronto; nulla di parziale è usabile. Le azioni Share/Join aprono i wizard solo se il profilo negoziato del server ha protocollo ≥ 3 e capability `e2ee` (discovery a ogni cambio di server/login; senza profilo o dopo il logout si torna ai dialog v2, che restano come fallback). Aperti: formato link/invito vNext, notifica push delle richieste di join (oggi Refresh manuale), progresso fine-grained (`JoinState`) dal daemon, correlazione degli errori (oggi per operazione pendente).

## Stato dell'implementazione

Requisito trasversale aggiunto: l'app deve essere **cross-platform**
(macOS, Linux, Windows), non macOS-first.

| Milestone | Contenuto | Stato |
|---|---|---|
| M1 | Contratti di dominio (`trama-domain::share`, `checkout_state`), snapshot/restore Git esatto (`trama-infra::share_git`), migration `0012_share_vnext`, CI matrice ubuntu/windows/macos sui crate core | fatto |
| M2 | Full-workspace builds on Linux/Windows/macOS; `trama-ipc` (Unix sockets / Windows named pipes); ConPTY terminals on Windows; per-OS data dir; `dunce` paths; CI on 3 OSes | done |
| Restructure | Codebase grouped into area modules, large files split, transitional re-exports removed ([restructure.md](restructure.md)) | done |
| M3 | Repository DB `trama-db::share`, manifest firmato (`SignedEnvelope`), protocollo vNext/discovery | fatto |
| M4 | Trasporto, E2EE MLS/openmls (dopo security review), wizard Share/Join | in corso: trasporto `/v3` (eventi, stream, manifest CAS, lease) fatto |

## Decisioni M3 (DB share e manifest firmati)

Scelte conservative prese dove la spec lascia aperto:

- **Firma:** Ed25519 (`ed25519-dalek`), unico `alg` ammesso; nessun `none` né downgrade negoziabile. Le chiavi device e la loro distribuzione/rotazione restano M4 (MLS); oggi il verificatore usa un `Keyring` locale che fallisce chiuso.
- **Encoding canonico:** "Trama canonical JSON v1" = sottoinsieme di RFC 8785 (JCS): chiavi solo ASCII ordinate per byte, niente spazi, solo interi (float rifiutati), escape minimi. Il verificatore ri-codifica e pretende bytes identici. La firma copre una PAE con lunghezze (`TRAMA-ENVELOPE-V1 len(type) type len(payload) payload`), così una firma non è riusabile su un altro tipo di manifest.
- **Manifest separati:** `HistoryManifestV1` (perimetro, thread, cursor) e `ContentManifestV1` (repository, checkout, object index, content policy). Gli `origin_hint` dei repository stanno solo nel content manifest, perché rivelano dati di codice a chi ha solo `read_history`. Il content manifest è legato all'history manifest tramite SHA-256 dei suoi bytes canonici, stesso `share_id` e `snapshot_version`.
- **Firmatario = publisher:** il device della chiave deve coincidere con `publisher_device_id`; il campo `publisher_signature` dell'esempio vive nell'envelope, non nel payload (campi sconosciuti rifiutati).
- **Rollback:** `open_history` rifiuta versioni sotto il pin locale; nel DB `advance_share_snapshot` è CAS e solo crescente; uno share `closed` non si riapre.
- **DB:** generazioni di checkout immutabili e appese con CAS su `previous_generation`; il fingerprint di un server già pinnato non si può cambiare né rimuovere con un upsert; `byte_len` di un oggetto noto è immutabile.

## Decisioni M4 (trasporto, scelte conservative)

- **Rotte `/v3/shares`:** registrazione (chi registra è owner), grant `writer`/`reader` + `read_code` solo dall'owner (un solo owner, non trasferibile), eventi sequenziati, stream WebSocket, commit manifest CAS, fetch manifest, writer lease. Discovery annuncia `2..=3` e solo `event_stream`, `manifest_store`, `writer_leases`; niente `e2ee`, `content_transfer`, `chunk_resume`, `direct_transport` finché non esistono.
- **Ammissione eventi lato server:** stesse regole di forma e replay del client (catena `author_seq` per device, idempotenza su `event_id`, batch tutto-o-niente); un device è legato al primo utente che lo usa; kind owner-only solo dall'owner; i reader possono pubblicare solo `peer_acknowledged`; `history_appended` è rifiutato (409) se un altro device detiene la lease del thread.
- **Lease:** esclusiva per (share, thread), TTL 1–600 s, rinnovabile dal detentore, rilascio solo dal detentore; scade da sola.
- **Manifest:** history e content sono blob base64 opachi separati; il content è servito solo con `read_code`; versione `expected + 1` con CAS; il pin locale avanza solo dopo l'accettazione del server. La verifica della firma resta al chiamante (keyring).
- **Cifratura:** trait `PayloadCipher` (seal/open per share, epoch e tipo di payload) dove si innesta MLS; default `NoCipher` che rifiuta tutto, quindi nessun manifest esce in chiaro. Il corpo degli eventi (`cipher_payload`) è già opaco.
- **Daemon:** uno stream per share non chiusa/in pausa legata al server connesso (`shares.server_id` → `collaboration_servers.base_url`); cursore e head per device persistiti (migration `0013_share_transport`); gap/reject → riconnessione dal cursore salvato con backoff 1–60 s. Gli eventi arrivano via IPC come `ShareEvent` al thread locale mappato (o alla radice dello share). L'API publish/manifest/lease del daemon non è ancora esposta via IPC: la userà il flusso Share/Join.

## Decisioni M4c (integrazione daemon, scelte conservative)

- **Cifratura**: il daemon usa `MlsPayloadCipher` (trama-e2ee) per eventi e manifest; `NoCipher` resta solo nei test. Un'identità MLS per installazione (`dev_<install id>`), un gruppo per share.
- **Stato a riposo**: identità, stati di epoca, join pendenti, chiave di firma dei manifest e pin sono in `e2ee_sealed` (migrazione 0014), sigillati con ChaCha20-Poly1305 (AAD = nome del blob). La chiave da 32 byte sta nel portachiavi del sistema (`keyring` v3: Keychain, Credential Manager, Secret Service). Se il portachiavi non è raggiungibile (Linux headless, CI) o `TRAMA_E2EE_KEY_FILE` è impostata, si usa `<app_dir>/e2ee-at-rest.key` (0600): protegge copie del solo DB, non un'app dir rubata. Se il file esiste, vince sempre (niente stato orfano).
- **Join**: key package → l'owner aggiunge il device, pubblica Welcome (`/v3/shares/{id}/welcomes`), invia il commit come evento `membership_changed` (payload = commit MLS grezzo) e ripubblica lo snapshot alla nuova epoca (MLS non decifra epoche precedenti al join). Se l'invio del Welcome fallisce dopo il merge locale, il gruppo va ricreato: niente rollback automatico in v1.
- **Firma eventi**: `signature = "mls"`; l'autenticità viene dal framing MLS e dal binding device→utente del server. Una firma per evento separata è rimandata.
- **Manifest**: solo history (`HistoryManifestV1`, firmato Ed25519 per device). `key_id = ed25519:<chiave pubblica>`, fissata per share al primo uso (TOFU); uno snapshot firmato con un'altra chiave è rifiutato. Content manifest ed export Git (bundle/oggetti) sono rimandati: manca la route di upload oggetti. `message_slice` è vuoto: la history passa solo dagli eventi live.
- **Preflight**: blocca su conflitti nell'indice (`unmerged_paths`), segreti nel working tree (tracciati + non tracciati non ignorati), nei blob dell'indice e nei blob dei commit non presenti su nessun remote (`rev-list HEAD --not --remotes`; oltre 500 commit si blocca invece di scansionare), scout il cui checkout appartiene a un thread fuori dallo share, checkout illeggibili. Nessun override. Scope `workspace` non ancora supportato dal flow.
- **Eventi ricevuti → history locale**: `history_appended` (corpo `{messages:[{origin, role, text, created_at}]}` in `vnext::bodies`) aggiunge messaggi `collab:<device>` al thread mappato; `review_verdict_posted` diventa una notice di sistema; `thread_added` crea una riga di perimetro non mappata; `membership_changed` applica il commit MLS (chiude lo share localmente se il device è stato rimosso). Gli altri tipi non producono history. Applicazione at-most-once (head registrato prima degli effetti). L'emissione automatica di `history_appended` dai turni locali non è ancora collegata.
- **Ruolo locale** degli share salvato in `collab_settings` (`share_role:<id>`).

## Decisioni M4d (history in uscita, revoca, cache snapshot)

- **History in uscita**: a ogni segnale di modifica, un device writer/owner che detiene il lease del thread pubblica i nuovi messaggi locali user/assistant (solo testo) come `history_appended`, a blocchi da 64. Watermark per thread in `collab_settings`, fissato a create/join alla fine della history attuale (la history precedente non diventa eventi). I messaggi scritti senza lease vengono saltati per sempre (il watermark avanza comunque); i messaggi mirrorati `collab:` non vengono mai ripubblicati.
- **Revoca** (`RemoveShareMember`, protocollo 15): il server cancella il grant, i key package e i lease dell'utente e restituisce i suoi device (`POST /v3/shares/{id}/members/{username}/revoke`); i binding device→utente restano, così un id revocato non può essere riassegnato. L'owner rimuove ogni device dal gruppo MLS (un commit `membership_changed` per device) e ripubblica lo snapshot alla nuova epoca. Il device revocato non riceve quell'evento (grant già revocato): localmente lo share resta aperto finché le chiamate falliscono. Un'eventuale UI di chiusura locale va decisa.
- **Cache snapshot**: l'ultimo snapshot history decifrato (e quello appena pubblicato dall'owner, che MLS non può decifrare) è salvato sigillato in `e2ee_sealed` (`snapshot:<share>`), indicizzato per versione; la firma e il pin si ricontrollano a ogni lettura.

## Decisioni M4g (robustezza share, binding chiavi, contenuto)

- **Share revocato**: se `GET /v3/shares/{id}` risponde not found per uno share locale aperto (dopo un errore di comando share o alla connessione dello stream), lo share diventa `Closed` localmente e lo stream viene chiuso. Forbidden non chiude (può essere un ruolo insufficiente).
- **Stato per messaggio** (protocollo 16): i turni senza lease restano saltati, ma ogni messaggio valutato ha uno stato `shared` / `not_shared_no_lease` in `share_message_states`, letto con `GetShareHistoryStatus` e notificato con `ShareHistoryStatus`.
- **History pre-share**: i messaggi di testo locali fino al watermark (fissato ora *prima* della prima pubblicazione) vanno in uno slice per thread, oggetto cifrato classe `history`; `message_slice` = `v1:<content_id>:<parts>:<sha256>:<chiave>`. Caricato una volta e riusato (`share_slice:<share>:<gid>`); i thread mappati dopo non hanno slice. Il joiner importa lo slice come messaggi `collab:<device publisher>`; un errore di import non blocca il join (log).
- **Retry invece di rollback**: commit MLS `membership_changed`, Welcome dell'approve e ripubblicazione snapshot entrano in `share_pending_ops` e vengono rispediti in ordine (ogni 20 s e prima di ogni emit); uno share si blocca al primo op fallito, gli emit non durevoli falliscono finché la coda non è vuota.
- **Binding chiave di firma**: l'owner registra (idempotente) `device_id → chiave ed25519` sul server per il proprio account (`POST /v3/signing-keys`; il primo binding vince, conflitti 409). Il joiner accetta un manifest solo se la chiave è legata dal server a `info.owner` e al device publisher; il pin TOFU locale resta in aggiunta. Il server diventa fidato per l'identità del publisher (non per il contenuto, che resta E2EE).
- **ListShareMembers**: unisce grant del server, membri del gruppo MLS (`in_group` solo sull'owner) e binding di firma.
- **Correlation id**: ogni errore share produce `shr_<id>`, loggato, inviato in `ShareFailed` e come suffisso `[ref …]` nell'`Error` classico (compatibilità desktop).
- **Oggetti cifrati**: `PUT/GET /v3/shares/{id}/objects/{content_id}/parts/{n}?class=history|content`, parti ≤ 1 MiB, immutabili, scritte solo dall'owner; `content` richiede `read_code`. Ogni oggetto ha una chiave casuale ChaCha20-Poly1305 (nonce = indice parte, AAD = content_id + indice) trasportata solo dentro manifest sigillati MLS (`ManifestObject.object_key`).
- **Contenuto negli snapshot**: a ogni pubblicazione l'owner riesegue il preflight (secret scan, conflitti) e rifiuta di pubblicare se non è pulito; per ogni checkout pianificato cattura `refs/trama/share/<share>/v<n>/…`, esporta un bundle Git senza gli oggetti raggiungibili dai ref remoti, lo cifra (max 256 MiB) e firma un `ContentManifestV1` legato alla history. Il joiner con `read_code` verifica manifest, hash e lunghezza e salva i bundle in `<app dir>/share-content/<share>/`.
- ~~RELEASE-BLOCKER restore~~: chiuso in M4i (vedi sotto).

## Decisioni M4i (restore contenuto, codici errore, share per thread)

- **Restore sul joiner**: dopo il mapping dei thread, un joiner con `read_code` verifica il content manifest e per ogni checkout crea uno staging in `<app dir>/share-checkouts/<share>/<thread_gid>/<repo_gid>-v<generation>`: `git init` con l'object format del manifest; se c'è `origin_hint`, `git fetch origin` con la configurazione e le credenziali Git dell'utente (prompt interattivi disabilitati); poi `share_git::restore` (nessun hook, `autocrlf=false`, verifica HEAD/index/working tree byte per byte). Il percorso è registrato in `share_checkout:<share>:<thread_gid>:<repo_gid>`. Uno staging fallito viene cancellato; il join resta valido (solo history) e l'errore va in `share_content_error:<share>` e nel log. Il collegamento dello staging come checkout operativo del thread è rimandato.
- **Bundle completi senza origin**: l'owner esclude gli oggetti raggiungibili dai ref remoti solo se il repository ha un remote canonico (hint); altrimenti il bundle è completo e si ripristina da solo.
- **Preflight non pulito**: la ripubblicazione per cambio membri (coda retry) pubblica comunque la history senza codice, con warning nel log, in `share_content_warning:<share>` e come `ShareFailed{code: "content_skipped"}` ai subscriber del thread root; create e `PublishShareSnapshot` continuano a rifiutare.
- **Codici errore share** (`ShareFailed.code`, e `Error.code`): `access_revoked` quando lo share è già chiuso localmente o il server risponde not found per lo share (che viene chiuso localmente); altrimenti il codice generico del daemon (`not_found`, `invalid`, `collab`, `busy`, …). Solo not found chiude; forbidden no. `not_a_member` non viene emesso dal daemon (il desktop lo tratta come `access_revoked`). `content_skipped` è solo un warning.
- **Share per thread** (protocollo 17): `GetThreadShare{thread_id}` → `ThreadShare{share: Option<ThreadShareInfo{share_id, status, role, thread_gid, lease_holder}>}`; `lease_holder` è best effort (assente offline). Basta per un "prendi lease" con un click (`AcquireShareLease`).
- **Warning inutilizzati**: `bind_signing_key`, `put_part`, `get_part` sono ora usati (binding chiave, slice e bundle); nessun `allow(dead_code)` aggiunto.

### M4j desktop decisions (share lease and status)

- "Take lease" sits under not-shared messages only. It is offered when `GetThreadShare` reports a share and the lease is free or held by someone else. It sends `AcquireShareLease` with the default TTL, and the daemon arbitrates. If it fails, the reason shows inline.
- `GetShareStatus` is asked for after `ThreadShare` (selected thread), `ShareCreated`/`ShareJoined` and a `content_skipped` warning. Warnings ("Published without code: …") and restore failures show as transcript banners of the thread. The owner wizard does not repeat them.
- "Retry restore" is shown only when `restore` is `failed`, and only one request runs at a time. A `ShareFailed{retry_share_restore}` is shown inline next to the restore error and its follow-up `Error` toast is swallowed.
