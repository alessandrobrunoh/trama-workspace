# 06 — Sicurezza, privacy, ACL ed end-to-end encryption

**Premessa fondamentale:** `trama-collab` attuale memorizza in chiaro transcript e contenuti condivisi; ADR 0014 lo dichiara esplicitamente. HTTPS protegge il traffico in transito, **non** dal gestore del server. Questa specifica propone un livello E2EE per la nuova condivisione: finché non esiste ed è verificato, non dichiarare Trama Cloud «zero knowledge».

## 1. Asset protetti e attaccanti

| Asset | Pericolo | Misure |
|---|---|---|
| Testo conversazioni/risposte/tool output | Esfiltrazione di segreti/proprietà intellettuale | E2EE, secret scan, capability `read_history`, log minimizzati |
| Diff, Git bundle e oggetti aggiuntivi | Estrazione codice e commit privati | E2EE, `read_code` separato, quote, consent preventivo |
| Manifest e metadati repo | Rivelazione nomi, URL e struttura team | Manifest cifrato, minimizzazione pubblico, scope isolation |
| Identità/inviti | Invito malevolo, replay, impersonazione | Device keys, firma, inviti one-use o TTL, phishing-safe UI |
| Checkout importati | Path traversal, symlink escape, esecuzione di script | Staging, path validation, no auto hooks, no executable post-join |
| Server/relay | Lettura dati, replay, data loss, metadata leakage | Server non possiede content keys, firme, policy/retention, backup |
| Computer partecipante compromesso | Lettura degli share accessibili | Fuori dalla garanzia E2EE; Keychain e revoca future epochs |

Threat model copre server curioso/malevolo, network attacker, utente invitato non fidato, collisione tra share, server self-hosted compromesso e partner che perde l'accesso. Il modello non protegge da chi è già autorizzato e salva copie del codice prima della revoca.

## 2. Politica enterprise fail closed

```text
allowed_collab_server_ids: [azienda-prod]
allow_trama_cloud: false
allow_public_discovery: false
allow_public_relay: false
allow_direct_p2p: false   # o true sulla LAN / VPN autorizzata
allowed_git_hosts: [git.company.example]
require_e2ee: true
require_git_provider_auth: true
allow_untracked_export: confirmation_required
max_share_bytes: policy-defined
```

Questa è una rappresentazione concettuale. Il daemon deve imporre la policy indipendentemente dalla UI. Nessun retry su un endpoint non autorizzato, nessuna query DNS/discovery a servizi pubblici quando disabilitati, nessun telemetry payload contenente dati del repo. Bloccare anche link `trama-join:` verso server non ammessi prima di contattarlo.

## 3. Gestione identità e chiavi

**Richiesta:** ogni installazione/device ha un'identità crittografica persistente conservata in Keychain; l'account enterprise collega dispositivi e persone in modo verificabile. L'utente deve verificare l'identità del destinatario tramite server aziendale o fingerprint/QR out-of-band, non fidarsi di un display name.

**Decisione crittografica da chiudere con security review:** valutare Messaging Layer Security (MLS, [RFC 9420](https://www.rfc-editor.org/rfc/rfc9420)) per gruppi asincroni, device multipli e membership changes. Evitare protocollo «fatto in casa». Un MVP limitato potrebbe usare contenuti cifrati con chiave di share e key-wrapping autenticato per dispositivi, ma non fornisce automaticamente tutte le proprietà di MLS (forward secrecy, PCS, epoch ordering). **Non pubblicare Cloud E2EE** senza design formale, test vectors e audit.

Il messaggio di bootstrap di un join deve concedere soltanto la chiave per capability autorizzata (`history` vs `code`), per le epoch ammesse e per lo share preciso. Rotazione dopo revoca e aggiunta; payload sempre autenticati e associati a `share_id`,`thread_gid`,`state_generation` mediante AAD, prevenendo cut-and-paste cross-share.

## 4. E2EE: cosa il server può e non può fare

Il server autorizza l'accesso all'**envelope e ai chunk cifrati** usando ACL lato server, ma **non** può filtrare semantica nascosta al suo interno. Quindi dividere risorse e chiavi: `history` / `code` / `member-control`; le ACL devono controllare l'accesso agli endpoint e le chiavi devono proteggere i payload anche in caso di compromissione storage.

Per ogni oggetto, definire `content_id` opaco, `ciphertext_hash`, size, chunk order, encrypted payload, key epoch e firma/proof. Il client verifica prima di importare Git. Evitare dedup globale per hash plaintext, che rivela se un file è comune tra share. La dedup sicura può essere limitata **al singolo share/epoch**.

Il server può vedere hostname, IP, orari, volumi di traffico, appartenenze e altro metadata necessario al routing. Non dichiarare anonimato o totale invisibilità di metadata.

## 5. Revoca e accesso per scope

- Revoca ACL server: stop immediato di nuove fetch, messaggi e turn, WebSocket invalidati come nell'attuale servizio.
- Rotazione epoch: cifrare i **nuovi** contenuti solo per i membri rimanenti. I contenuti già consegnati all'ex membro non possono essere recuperati.
- Per `thread_only`: neppure il titolo dei sibling, message pointers, `@thread`, changelog o revision ids dei parent devono trapelare. Adottare redazione **prima** della crittografia.
- `read_code=false`: nessuna chiave/manifest/oggetto Git per quel soggetto, anche se ha `read_history`. La history può comunque includere codice nei tool output: consentire configurazione `history_redaction` oppure informare che non è una boundary perfetta tra codice e transcript.
- URL Git privati: l'invito non attribuisce permesso provider; recipient deve autenticarsi autonomamente.

## 5bis. Enforcement reale dei permessi Git

**Caveat critico:** non è sufficiente che l'app richieda a Bob un clone da GitHub. Se Bob possiede una key `read_code` e scarica bundle cifrati che poi decritta, può estrarne codice anche se ignora il clone. Una vera policy «solo collaboratori con accesso Git» richiede un **Git Authorization Gate** prima della consegna delle chiavi codice: prova di membership/permesso del provider o attestazione firmata da infrastruttura Git dell'azienda, verificata dal grantor/authority prevista nel threat model. Usare scopes OAuth minimi, no token Git nel server di condivisione. L'identity claim va legato a repo precisi, share, destinatario e device con scadenza. Per accesso a Git provider non integrato, mostrare `Cannot verify Git permission` e offrire solo history-only oppure un grant code alternativo **esplicito** approvato dall'owner, mai bypass silenzioso.

La verifica una tantum non previene l'uso di copie già ottenute dopo revoca Git: è un limite intrinseco alla distribuzione di dati.

## 6. Secret egress: scanner + conferma, non falsa sicurezza

Prima di upload, consentire rules su percorsi (`.env*`, private keys, `*.pem`, `.aws/`, cache), pattern nel contenuto (credential-like tokens), dimensioni e repo inclusi. Scanner di segreti = riduzione rischio, **non garanzia**. UI deve riepilogare il numero di file/bytes e segnalare possibili credenziali; admin può vietare qualsiasi export di untracked o di path matching. Evitare l'upload di output di terminale non autorizzato e degli snapshot degli ambienti ignorati. Escludere di default `.git/config`, `.trama/`, `.env*`, keychain data, ignored files.

**Importante:** Trama usa attualmente APFS copy-on-write per clonare anche ignored files nel checkout locale; questa comodità per build/cache **non** implica che tali file debbano essere esportati o ereditati dal destinatario. Import di share in clone pulito o con scrub verificato.

**Segreti in vecchi commit:** non basta una denylist di path del filesystem. Il tree dell'HEAD, i commit non pushati e i bundle supplementari possono raggiungere blob con credenziali anche se ora sono cancellati o ignorati. Lo scanner preflight deve controllare gli oggetti che saranno effettivamente esportati e **bloccare l'export** quando un requisito di exactness e una regola di esclusione sono incompatibili. Non promettere di alterare la storia Git e mantenere OID identici nello stesso tempo.

## 7. Sicurezza del restore

- Clone URL mostrato e approvato: vietare URL/protocolli che eseguono codice (`ext::`, helper custom, `file://` fuori policy), opzioni `git -c` injection e shell interpolation.
- Contenere tutte le scritture nella staging directory. Non seguire symlink dall'archive, non dereferenziare oggetti Git speciali e verificare i path su case-insensitive APFS.
- Limitare decompression ratio, object count, pack size, nesting, path lengths e timeouts; verificare chunk integrity e Git object validation; scartare archivi malevoli.
- Nessun `prepare`, `linked`, `post-checkout` o script arbitrario all'import senza consenso. Valutare `GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_GLOBAL` e disabilitazione hook; non danneggiare le credenziali Git dell'utente durante clone legittimo.
- Esecuzione Claude Code solo dopo checkout Ready e approvazioni normali; mai avvio automatico da un link.

## 8. Compliance operativa

Log server: solo request-id, ID opachi, size/count, status, fail codes e audit membership; no token, path, content, transcript o URL Git completi. Retention configurabile per eventi e oggetti cifrati; purge di oggetti orfani con grace period, backup/restore, disaster recovery. Dashboard enterprise mostra chi ha condiviso con chi, scope e data policy, **non** plaintext code.

## 9. Threat model test cases

1. Read-only membro chiede object endpoint: `403/404`, nessuna chiave consegnata.
2. Child-only membro prova ID del parent: niente metadata, nemmeno search o diff.
3. Revoca durante stream: streaming bloccato, nuova epoch negata.
4. Server compromesso tenta sostituzione manifest/rollback: signature e version pin failure.
5. Chunk sostituito, reorder, duplicato e cross-share: decrypt/integrity failure.
6. Repo malevolo contiene symlink verso `~/.ssh`: restore non scrive fuori checkout.
7. Invito con Git URL malevolo: non esegue helper o comando di sistema.
8. Policy enterprise vieta Cloud: zero tentativi di connessione a servizi pubblici anche in caso di errore sul server interno.

## 10. Go/no-go pubblico

**No-Go:** se il server possiede chiavi di decrittazione, se il cloud vede diff in chiaro, se parent/sibling leaks passano, se un join può lanciare script non approvati, se un membro revocato può fetchare nuovi blob, se il client dice «Ready» con oggetti mancanti.
