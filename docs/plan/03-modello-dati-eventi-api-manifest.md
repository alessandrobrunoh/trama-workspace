# 03 — Modello dati, eventi, API e manifest

**Stato:** proposta di contratto vNext. `COLLAB_PROTOCOL_VERSION=2` e `PROTOCOL_VERSION=12` sono le versioni **attuali** alla baseline; gli esempi v3 seguenti **non funzionano** nel software corrente.

## 1. Identità e cardinalità

- `server_id`: identità persistente del server, **non** il solo URL visuale. Evita confusione in caso di redirect/DNS e migrazione.
- `share_id`: ID globalmente unico, associato a server e policy.
- `share_root_id`: ID globale del thread scelto, anche se localmente è un subthread.
- `thread_gid`: identità stabile nello share, distinta dagli ID locali (`collab_links` fornisce già un mapping simile).
- `repo_gid`: identificazione all'interno dello share di un repository; il `remote_url` è un candidato, **non** un'autorizzazione e non la sola identità (fork, rename).
- `state_id`: ID immutabile di uno specifico checkout a una generazione; `thread_gid + repo_gid + state_generation`.
- `event_id`: ID immutabile, `author_device_id` e `author_seq`/nonce contro replay.
- `content_id`: hash/ID opaco di bytes cifrati o digest autenticato; non esporre hash di plaintext nel controllo pubblico se facilità correlazione o probing.

## 2. Struttura del manifest firmato

Esempio leggibile in [`examples/share-manifest.example.json`](examples/share-manifest.example.json).

Campi richiesti per `ShareManifestV1`:

| Campo | Regole |
|---|---|
| `schema_version` | Intero; il reader rigetta major sconosciute |
| `share_id`, `snapshot_version`, `scope`, `root_thread_gids` | Identità e perimetro autorizzato |
| `created_at`, `publisher_device_id`, `publisher_signature` | Firma su encoding canonico/bytes dell'envelope; il JSON di esempio non è firmato realmente |
| `repositories[]` | `repo_gid`, alias, origin **suggerito**, Git object format (`sha1`/`sha256`), commit ref valido, LFS/submodule flag |
| `threads[]` | `thread_gid`, `parent_gid` nullo al confine, kind, title, message slice, eventuale `fork_origin` opaco |
| `checkouts[]` | `thread_gid`, `repo_gid`, `head_commit`, `index_tree`, `working_tree`, branch/detached, base/fork, `state_version`, `objects[]`, `checkout_mode` |
| `object_index[]` | blob cipher content-addressed, dimensioni, chunk list, chiave/epoch, verifiche crittografiche |
| `history_cursor` | Watermark oltre il quale riprende live sync |
| `content_policy` | Include/exclude, max bytes, metadata di file esclusi senza esporre path sensibili a non autorizzati |

**Non** serializzare: token Git/Claude, API key, percorsi assoluti sorgente, `TRAMA_APPROVAL_TOKEN`, `.git/config` locali, id di processo, socket o sessioni Claude.

**Nota sul manifest:** un esempio JSON applicativo non basta come firma: definire separatamente un `SignedEnvelope { payload_bytes, alg, key_id, signature }` e un encoding canonico non ambiguo (es. protobuf/canonical CBOR o canonical JSON formalizzato). Il server conserva bytes cifrati; il client verifica firma sull'envelope ricevuto dopo decrypt.

## 3. Evoluzione delle tabelle locali (SQL *illustrativo*)

```sql
-- Non applicare direttamente: da tradurre in nuove migration append-only,
-- con foreign key, checksum, indici, vincoli, mapping e test.
CREATE TABLE collaboration_servers (
  server_id TEXT PRIMARY KEY,
  label TEXT NOT NULL,
  base_url TEXT NOT NULL,
  server_fingerprint TEXT,
  trust_policy TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE TABLE shares (
  share_id TEXT PRIMARY KEY,
  server_id TEXT REFERENCES collaboration_servers(server_id),
  scope TEXT NOT NULL CHECK (scope IN ('thread_only','subtree','workspace')),
  policy_json TEXT NOT NULL,
  root_gid TEXT,
  current_snapshot_version INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL
);
CREATE TABLE share_threads (
  share_id TEXT NOT NULL,
  thread_gid TEXT NOT NULL,
  local_thread_id TEXT,
  parent_gid TEXT,
  source_scope_root INTEGER NOT NULL DEFAULT 0,
  kind TEXT NOT NULL,
  PRIMARY KEY (share_id, thread_gid)
);
CREATE TABLE share_checkout_states (
  share_id TEXT NOT NULL,
  thread_gid TEXT NOT NULL,
  repo_gid TEXT NOT NULL,
  generation INTEGER NOT NULL,
  head_oid TEXT NOT NULL,
  index_tree_oid TEXT NOT NULL,
  working_tree_oid TEXT NOT NULL,
  manifest_content_id TEXT NOT NULL,
  status TEXT NOT NULL,
  PRIMARY KEY(share_id,thread_gid,repo_gid,generation)
);
CREATE TABLE share_object_inventory (
  share_id TEXT NOT NULL,
  content_id TEXT NOT NULL,
  epoch INTEGER NOT NULL,
  byte_len INTEGER NOT NULL,
  state TEXT NOT NULL,
  PRIMARY KEY (share_id,content_id,epoch)
);
CREATE TABLE share_peer_acks (
  share_id TEXT NOT NULL,
  device_id TEXT NOT NULL,
  event_cursor TEXT NOT NULL,
  content_watermark TEXT,
  PRIMARY KEY(share_id,device_id)
);
```

**Compatibilità:** non alterare o cancellare migration numerate 0001–0011; aggiungere `0012_*.sql` o successiva dopo verificare il numero reale al momento del merge. Le FK e i payload definitivi devono essere coerenti con `trama-db` e con la logica di event sourcing (nuove righe e proiezioni, nessuna mutazione del log esistente).

## 4. API server vNext (proposte)

REST per snapshot e controllo; WebSocket per stream. Rotte non ancora esistenti:

| Metodo | Path | Semantica | Authorization |
|---|---|---|---|
| `POST` | `/v3/shares` | Registra share, scope, policy ed envelope iniziale | Owner |
| `POST` | `/v3/shares/{id}/members` | Invito o grant di capability | Owner |
| `GET` | `/v3/shares/{id}/manifest/{version}` | Scarica manifest cifrato e firmato | `read_history` o `read_code` secondo policy; non esporre manifest che contiene dati oltre capability |
| `POST` | `/v3/shares/{id}/objects/prepare` | Init upload multipart/chunk cifrati; quota/TTL | `read_code + publish_changes` oppure owner |
| `PUT` | `/v3/shares/{id}/objects/{content_id}/{part}` | Upload chunk, idempotente | Writer |
| `GET` | `/v3/shares/{id}/objects/{content_id}/{part}` | Download chunk cifrato | `read_code` |
| `POST` | `/v3/shares/{id}/snapshots/commit` | Pubblica manifest versione N con CAS e verifica inventario | Writer autorizzato |
| `GET` | `/v3/shares/{id}/events?after=...` | Eventi durabili successivi | Capability appropriata |
| `GET` | `/v3/shares/{id}/stream` | WebSocket presence, eventi, ack, health | ACL |
| `POST` | `/v3/shares/{id}/members/{member}/revoke` | Revoca, nuova epoch chiavi | Owner |

**Separazione ACL:** un reader senza `read_code` non deve ottenere `object_index`, checkout snapshot e decrypt key relativa. Meglio envelope distinti `HistoryManifest` e `ContentManifest` che un unico manifest sottoposto a filtraggio server-side incompatibile con E2EE. Un metadata server opaco non può filtrare payload cifrati arbitrari: tale segmentazione deve avvenire **prima** di cifrare/pubblicare.

## 5. Eventi proposti

`ShareCreated`, `ScopeExpanded`, `ThreadAdded`, `ThreadRemovedFromScope`, `HistoryAppended`, `CommentChanged`, `ReviewVerdictPosted`, `CheckoutSnapshotPrepared`, `CheckoutSnapshotCommitted`, `CheckoutSnapshotSuperseded`, `MembershipChanged`, `KeyEpochAdvanced`, `PeerAcknowledged`, `ConflictDetected`, `SharePaused`, `ShareClosed`.

Tutti gli eventi durabili hanno `event_id`, `share_id`, `thread_gid?`, `author_device`, `author_seq`, `prev_author_event`, `parents[]`, `server_seq?`, `key_epoch`, `cipher_payload`, `signature`, `created_at`, `received_at`. I timestamp non sono l'ordinamento autorevole; su server v1 sequenza e causalità guidano i replay.

**Garanzie del delivery:**

- Retry safe usando ID stabili e idempotency keys.
- Prima si caricano gli oggetti e poi si pubblica il commit del manifest referenziante: nessun puntatore visibile a contenuti assenti.
- Un upload non committato è eliminato da GC con una finestra di sicurezza.
- Acknowledge locale solo dopo persist e verifica; ack di contenuto soltanto dopo restore valido.
- Snapshot CAS `expected_generation` per prevenire overwrites silenziosi.
- `read-only v2` per vecchi client **non va promesso** automaticamente: quando lo share è E2EE o trasporta oggetti v3, serve messaggio di upgrade obbligatorio.

## 5bis. Prova di accesso Git

Se policy `git_authorization_required`, il code grant include `repo_gid[]`, `identity_subject`, `authorization_attestation_id`, `expires_at`, `issuer` e firma del grantor. La proof deve provenire da Git provider o trust aziendale e **non** da un booleano inviato dal client. L'attestazione può essere rinnovata o revocata per future key epochs; non toglie il codice già ricevuto. Il server può gestire verifica dell'attestazione senza possedere chiavi plaintext, ma la distribuzione delle code key deve essere effettuata solo dal grantor/device attualmente autorizzato o da un key service aziendale con modello esplicito.

## 6. Capacità e version negotiation

Non riutilizzare `COLLAB_PROTOCOL_VERSION=2` con payload incompatibili. Proposta: discovery `GET /.well-known/trama-collab` con `server_id`, `min_client_protocol`, `max_client_protocol`, `capabilities` (es. `e2ee`, `content_transfer`, `subtree_share`, `workspace_share`, `chunk_resume`, `direct_transport`), massimali e policy. Questo endpoint può esporre solo metadati pubblici.

La sessione client negozia un profilo di capabilities *prima* di creare lo share. L'app non presenta funzioni non supportate. Downgrade crittografici non consentiti; il downgrade del transport deve rispettare policy enterprise.

## 7. Riferimenti del codice esistente

- [`crates/trama-db/migrations/0006_collab.sql`](https://github.com/alessandrobrunoh/trama-workspace/blob/558f0e4/crates/trama-db/migrations/0006_collab.sql) — `collab_links`, cursori e turn di autore.
- [`crates/trama-db/migrations/0011_collab_history.sql`](https://github.com/alessandrobrunoh/trama-workspace/blob/558f0e4/crates/trama-db/migrations/0011_collab_history.sql) — outbox, family, origine messaggi, remote changes.
- [`crates/trama-collab-protocol/src/lib.rs`](https://github.com/alessandrobrunoh/trama-workspace/blob/558f0e4/crates/trama-collab-protocol/src/lib.rs) — enum eventi v2 e limiti body.
- [`crates/trama-protocol/src/lib.rs`](https://github.com/alessandrobrunoh/trama-workspace/blob/558f0e4/crates/trama-protocol/src/lib.rs) — IPC UI/daemon v12.
