# 05 — Sincronizzazione, concorrenza, offline e conflitti

## 1. Due livelli da tenere distinti

**Collaboration event log:** messaggi, commenti, member changes, child creation, stati degli agenti e versioni Git pubblicate. **Checkout content:** oggetti immutabili che permettono materializzazione. Gli eventi possono arrivare prima o dopo il content fetch; si registra `ContentPending` e non si applica nulla prima della verifica degli oggetti.

Nel backend server v1 rimane un ordine `seq` per share/thread come in `collab_events`; ogni evento aggiunge `event_id` stabile e parent causali per rendere possibile il futuro backend P2P. Non mescolare `server_seq` con `author_seq`: uno si assegna nel service, l'altro nasce sul device ed è firmato.

## 2. Concorrenza: regola v1 = checkout a writer singolo

Un thread ha una **revisione pubblicata** e un `writer_lease` (per share + thread + repo o per thread intero). L'agente continua a girare sul device della persona che preme Send, ma **non** parte in modalità mutating se quel checkout è posseduto da un altro writer. Il lease è su **chi può pubblicare la revisione del checkout**; non concede privilegi Git o shell. Un partecipante può comunque commentare o fare review senza lease.

Stati: `unassigned`, `held`, `transfer_requested`, `expired`, `released`, `conflict`. Rinnovo durante la sessione; CAS su `base_state_id` al publish. I turn con esito ambiguo **non si rieseguono automaticamente**, coerentemente con l'attuale `collab/runner.rs`.

**Trade-off:** se Alice è offline e Bob ha un vecchio stato, Bob può scegliere **Fork locally** in un nuovo subthread, non aggiornare silenziosamente la revisione condivisa dello stesso checkout. Per mantenere la promessa “continue where I left off” la UI rende chiaro se Bob sta **continuando sul ramo condiviso** o **creando una divergenza**.

## 3. Versioning degli stati Git

Ogni checkout pubblicato ha `(state_id, previous_state_id, generation, publisher_device_id, manifest_content_id, expected_previous_generation)`. Il server e/o il coordinatore rifiutano CAS se non corrisponde. L'evento «Nuova versione» viene persistito solo dopo che tutti i blob sono presenti e autorizzati. Il ricevente applica solo versioni incrementali la cui dipendenza è soddisfatta.

Un partecipante che ha modifiche locali non pubblicate **non riceve mai `reset --hard` automatico**. Si propone:

- `Review incoming changes`: diff fra local e remoto;
- `Fork local changes`: crea un child con lo stato locale e allinea la copia condivisa;
- `Pull Changes`: usa il merge esistente e mostra conflitti;
- `Discard local changes`: azione distruttiva esplicitamente confermata e recuperabile da snapshot.

## 4. Offline queue e recovery

Ogni device mantiene un outbox **durabile** di metadata e contenuti, distinto per share e server. I chunk sono upload/download idempotenti con retry/backoff; gli ack sono persistiti. Dopo reconnect: handshake delle capabilities → confronto cursori → replay paginato eventi → fetch manifest/snapshot mancanti → verifica oggetti → materializzazione.

La semantica è **at-least-once delivery + exactly-once application locale tramite idempotenza**, non promette rete exactly-once. Se un event è ripetuto, la proiezione DB non cambia. Uno stato `Ambiguous` dell'agente richiede intervento umano, come nell'implementazione corrente.

**Offline senza relay always-on:** P2P puro non può consegnare eventi quando entrambi i peer non sono raggiungibili. Il prodotto deve visualizzare «sync pending until a peer is online». Se servono mailbox offline, usare un relay/host aziendale autorizzato con payload E2EE.

## 5. Selezione scope che cambia nel tempo

- Share `subtree`: i figli futuri entrano solo se `include_future_descendants`; il daemon filtra anche subagenti e fork.
- Share `thread_only`: un child nuovo non viene automaticamente esportato. La UI può proporre «Share new child» con consenso.
- `workspace`: aggiunta di repository o thread può richiedere nuova approvazione se varia la superficie dati; repo nuovi non entrano implicitamente se `auto_include_new_repositories=false`.
- `ScopeExpanded`: backfill cifrato dei nuovi thread; `ScopeReduced`: revoca futura con nuova epoch chiavi, pur senza poter cancellare copie già esportate.
- Gli ID opachi e i cross-reference verso thread esclusi non devono rivelare metadati (titoli, path, diff e snippet).

## 6. Gestione branch parent/child e pull

Ogni checkout indipendente ha uno `state_id` diverso. Le operations `Pull Changes` restano esplicite e idempotenti; si conserva `fork_point_oid` e `parent_state_id` per un merge 3-way. Un ReviewVerdict non modifica il checkout del parent e un Land thread non invia push/merge per il solo fatto di essere condiviso.

**Scouts:** se alias del parent, il lease segue il vero owner. L'alias è solo metadata, non un secondo writer.

## 7. Cronologia dei turn e ordering

La famiglia può avere molte stream concorrenti. Un evento globale share-log ordina annunci di nuovi thread e permessi; ciascun thread ha ordering causale per messaggi/turn. `snapshot_version` fotografa un watermark coerente; un client che non ha tutti i contenuti potrà mostrare il testo ma non dichiarare `CheckoutReady`.

Per il replay non si ricostruisce l'ordine dal timestamp. Mantenere `event_id`, `logical_parent_ids` e `server_seq` e scartare replay duplicati. Le annotazioni che puntano a elementi non in scope vanno redatte o marcate non disponibili, non auto-risolte attraverso la family completa.

## 8. Predisposizione P2P

Il protocollo portable conserva eventi con ID, autore, `parents[]` e payload E2EE. Un futuro transport diretto può scambiarsi head/cursori vettoriali e inventari chunk, ma **non** eredita automaticamente il sequenziatore del server. Si dovrà scegliere un ordinamento deterministico dei pari e la gestione degli owner/lease senza authority always-on (es. coordinatore eletto o fork offline). Designare questa scelta come ADR aperta; non implementare un CRDT implicito nel registro attuale.

## 9. Failure matrix sintetica

| Guasto | Comportamento corretto |
|---|---|
| Upload blob completato, publish manifest fallito | Oggetti orfani con TTL; retry stesso idempotency key |
| Manifest visibile, blob mancante (bug del server) | `IntegrityFailed`, nessun checkout operativo |
| Server restart dopo claim turn | Turn `Ambiguous`, mai doppia esecuzione |
| Revoca membro a sync in corso | Abort nuovo fetch/turn, rotazione chiave, audit |
| Client offline modifica stesso file | Non overwrite; fork e merge esplicito |
| Doppio evento | Apply una sola volta tramite `event_id` |
| Rinomina Git origin | Rebind repository con approvazione; verificare commit base |
| Nuovo child non autorizzato | Non includere in manifest, stream o object access |
