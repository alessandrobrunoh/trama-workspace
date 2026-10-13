# 01 — Requisiti, lessico e ambiti di condivisione

**Status:** proposta. **Priorità:** definisce i contratti per tutte le fasi.

## 1. Obiettivi

1. Uno sviluppatore con accesso Git valido deve poter importare un thread e **continuare dall'esatto stato di codice condivisibile** che il mittente possedeva al momento dello snapshot, inclusi commit locali, staged, unstaged e untracked non ignorati ammessi.
2. Un albero condiviso comprende **uno stato di checkout per ogni `(thread_id, repository_id)`** e può contenere stati divergenti: main, child, fork, review, land, worker e scout.
3. La condivisione può riguardare **un singolo figlio**, il **figlio con tutti i discendenti** o **un intero workspace multi-repository**; il confine di autorizzazione non dipende dal top-level thread.
4. La sincronizzazione dei contenuti non deve obbligare a copiare l'intera storia Git in Trama Cloud; il Git remote originale è la sorgente primaria per i commit pubblicati.
5. Trama Cloud (ufficiale), Trama Collab self-hosted (Docker) e Direct P2P futuro condividono un protocollo applicativo comune.
6. Nessun commit, merge, push, land, comando locale o avvio dell'agente deve partire implicitamente come effetto di Share o Join.
7. Aziende e utenti devono poter definire policy: server autorizzati, data egress, clone host allowlist, feature disabilitate, dimensioni, E2EE e retention.

## 2. Lessico rigoroso

- **Workspace/Project:** gruppo nominato di uno o più repository, come `trama-domain::workspace::Workspace`; non coincide con un repository.
- **Thread:** conversazione più set di checkout assegnati; parent/children costituiscono una **family**.
- **Share:** ACL, partecipanti, server profile, policy, radici incluse, manifest e log di eventi.
- **Share root:** thread selezionato come punto d'ingresso; può essere a qualsiasi profondità.
- **Share scope:** `thread_only` / `subtree` / `workspace`. Il terzo si riferisce all'insieme esplicitamente selezionato dei thread del workspace, non alle copie non correlate presenti sul disco.
- **Thread replica:** istanza locale importata da uno share; conserva ID globali di condivisione ma ID interni locali propri per compatibilità.
- **Checkout state:** `HEAD` + `index_tree` + `working_tree` + Git objects e metadata; non una `git worktree` fisica trasferita integralmente.
- **Repository source:** origin canonico (es. GitHub privato), verificato e clonato direttamente dall'utente destinatario.
- **Bundle/object transfer:** oggetti Git non disponibili dopo clone/fetch, compresi commit privati non pushati; contenuti potenzialmente sensibili e obbligatoriamente cifrati nel cloud pubblico.
- **Contributor vs reader:** chi ha diritto a vedere un transcript non acquisisce automaticamente diritto a leggere codice o fare clone.
- **Live share:** dopo lo snapshot iniziale, nuovi eventi e nuove versioni di checkout si sincronizzano nel tempo.

## 3. Scope inclusi

| Scope | Include | Esclude per default | Nuovi discendenti |
|---|---|---|---|
| `thread_only` | Thread scelto, relativi messaggi, review state, commenti, tutti i checkout del thread sui repo ammessi | Parent, siblings, children, contenuti non pertinenti | No |
| `subtree` | Thread scelto e ricorsivamente tutti i figli (qualunque profondità) con i propri checkout | Parent esterno, siblings esterni, altre famiglie | Sì, se flag `include_future_descendants=true` |
| `workspace` | Le radici scelte nel workspace e relativi discendenti, relazioni multi-repo e opzioni condivise | Altri workspace, thread esclusi esplicitamente, repo non ammessi | Da policy per root |

**Importare un child senza il parent:** la UI mostra quel child come nuova radice locale e, se necessario, un `fork_origin` tecnico opaco senza copia del transcript o dei file del genitore non autorizzato. Il child è già uno stato Git completo e non richiede l'accesso al suo parent per essere materializzato. Collegamenti ai thread non condivisi vengono soppressi o visualizzati come «thread non disponibile».

**Review/land:** il risultato, i commenti e le modifiche della review appartengono al thread di review; non vengono automaticamente applicati al parent sul computer ricevente. I verdict possono avere riferimenti al parent come metadata minimizzato, ma senza escalation di permessi.

**Scouts:** nel codice attuale condividono il checkout del parent; il manifest specifica `checkout_mode=alias` e l'owner nel perimetro. Se il parent non è condiviso, scegliere prima dell'export tra (a) materializzare snapshot indipendente nel child, oppure (b) impedire la condivisione con messaggio esplicito; non perdere mai i riferimenti o concedere permessi esterni al perimetro.

## 4. Matrice delle capability

| Capability | Può leggere transcript | Può ricevere contenuti Git | Può pubblicare turn | Può commentare/review | Può modificare scope/ACL |
|---|---:|---:|---:|---:|---:|
| Owner | Sì | Sì | Sì | Sì | Sì |
| Editor | Sì | Se autorizzato | Sì, se checkout disponibile | Sì | No |
| Reviewer | Sì | Se autorizzato | No, salvo override | Sì | No |
| Reader | Sì | No per default | No | No | No |

Le capability sono **separate**: `read_history`, `read_code`, `publish_turn`, `comment`, `review`, `export`, `manage_share`; l'interfaccia non può esibire `Continue coding` a chi non dispone di `read_code` e di un clone Git utilizzabile.

## 5. Scenari e criteri osservabili

### S1 — Share di `Auth Review` soltanto

Alice condivide un review child. Bob può clonare i repo ammessi, aprire `Auth Review`, vedere l'attuale diff, aggiungere commenti e — se `publish_turn` consentito — creare il proprio turno. Bob non vede il Main Thread, la branch sorella Dashboard o transcript dei parent. Se non ha permesso Git, legge solo quanto autorizzato dalla capability effettiva (potenzialmente solo transcript).

### S2 — Main con 15 discendenti e 2 repository

Alice condivide il Main come `subtree`. Bob vede albero, stati e ordini causali. Tutti i `(thread, repo)` che hanno checkout indipendente ricevono snapshot diversi. Gli alias scout puntano solo a checkout inclusi. Un subthread creato dopo lo share appare se consentito.

### S3 — Commit non pushati e file staged

Alice ha 2 commit locali, 1 file staged, 1 file unstaged e 2 file untracked. Dopo clone dal Git provider, Trama recupera gli oggetti Git mancanti dal trasporto cifrato e materializza stato HEAD/index/worktree; `git status --porcelain=v1 -z` sul clone di Bob deve essere semanticamente equivalente al checkout mittente salvo esclusioni esibite.

### S4 — Server non autorizzato

Un admin aziendale consente soltanto `https://collab.company.test` (esempio). Trama non effettua chiamate di test, discovery o fallback a endpoint pubblici. Share su Trama Cloud è disabilitato con motivazione. P2P è disabilitato se non autorizzato.

### S5 — Offline e conflitti

Bob lavora offline su un branch locale. Alice modifica lo stesso checkout. Al reconnect la nuova revisione non può sovrascrivere il lavoro locale: si crea una divergenza esplicita e si offre review/merge/fork controllato.

## 5bis. Autorizzazione Git vs autorizzazione Trama

Un `git clone` tentato e riuscito nel client è una buona verifica d'usabilità, **non** costituisce da solo un controllo d'accesso server-side: un membro con chiave di decrittazione e oggetti Git nello share può estrarne contenuto anche senza clonare. Occorre una scelta esplicita:

- **Modalità `git_authorization_required` (consigliata per enterprise):** prima di emettere un grant `read_code`/consegnare key material, il grantor o un servizio aziendale attestatore verifica per ogni repo l'accesso alla Git identity del destinatario tramite provider API/OAuth o attestazione aziendale firmata; permessi rinnovati/TTL e revoca documentati. Non consegnare tokens Git al server Trama; usare proof a scope limitato.
- **Modalità `share_grants_code_access` (opt-in):** l'owner autorizza espressamente il destinatario a ricevere codice indipendentemente dai permessi del provider; UI avverte che il codice nei Git bundle è direttamente leggibile. Non chiamarla "richiede clone permission".

La modalità enterprise non garantisce revoca retroattiva dei contenuti già ricevuti. Una key E2EE non va inviata a un partecipante finché la verifica obbligatoria non ha esito positivo.

## 6. Non-obiettivi del primo MVP

- Clone anonimo di repository privati o concessione implicita di credenziali Git.
- Live co-editing CRDT di ogni file.
- Migrazione della memoria privata/ID di sessione Claude da un account all'altro.
- Copia di `.env`, `node_modules`, `target`, credenziali, terminali live e processi attivi.
- Peer discovery globale / NAT traversal P2P nella prima release server-based.
- Remote runner SSH; esecuzione obbligatoria dei turn altrui o push/merge automatici.

## 7. Invarianti

- Il repository sorgente non è mai mutato; il destinatario non è mai costretto a clonare da un server Trama.
- Gli oggetti esportati non devono esporre percorsi assoluti, token, socket o ID locali sensibili.
- Ogni checkout mostra `sync_status` e `content_status`; una storia completa con contenuto mancante **non** può apparire pronta per l'esecuzione.
- È possibile esportare/ricostruire in modo deterministico a parità di manifest e oggetti; retry e reconnect sono idempotenti.
- Nessuna condivisione di siblings/parent/altre repo non esplicitamente in scope, inclusi backlink, annotation e comment anchors.
