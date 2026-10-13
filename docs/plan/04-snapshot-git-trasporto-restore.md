# 04 — Snapshot Git completo, trasporto incrementale e restore

**Stato:** proposta. Documento critico per la promessa «continua dal mio stesso progetto».

## 1. Perché il solo `git diff` è insufficiente

Un checkout può contenere: commit locali mai pushati, branch detached, staged e unstaged diversi, file untracked non ignorati, eliminazioni, rename, bit eseguibile, symlink e oggetti binari. Un diff testuale limitato UI non copre tutto. Gli attuali `changes::snapshot_tree`/`record_snapshot` catturano il **working tree** in un tree Git temporaneo, ma **non** conservano l'esatta distinzione staged/unstaged nel tree risultante.

## 2. Snapshot canonico di `(thread, repo)`

Registrare:

```
ShareCheckoutState {
  thread_gid,
  repo_gid,
  checkout_owner_gid,       // per scout e alias
  source_remote_fingerprint,
  source_object_format,      // Git SHA-1 / SHA-256
  base_commit_oid,
  head_commit_oid,           // comprende commit locali
  branch_ref | detached,
  index_tree_oid,            // contenuto STAGED del vero index
  working_tree_oid,          // contenuto del working tree (+ untracked ammessi)
  fork_point_oid?,
  checkpoint_refs?,
  state_generation,
  entry_metadata,
  content_policy_version,
  omitted_paths_summary,
}
```

In caso di **index in conflitto** (`unmerged entries`), `git write-tree` fallisce: bloccare con un errore assistito oppure definire in fase successiva un formato per stage 1/2/3. Non dichiarare «exact state» senza supportare quel caso. Similmente `intent-to-add`, sparse checkout, submodule dirty e configurazioni Git speciali richiedono policy esplicite e test.

## 3. Export algoritmico — proposta

1. Prenotare il checkout tramite i meccanismi esistenti (`SessionManager::reserve_checkouts`), verificare che nessuna scrittura di agente/fork/pull sia in corso; in presenza di modifiche utente esterne considerare retry se `HEAD`, index o tree cambiano durante la cattura.
2. Verificare che sia un checkout gestito validato, non il repository sorgente. Associare repo canonico e `objectFormat` e risolvere `HEAD`, `branch_ref`, `base`, `fork_point`.
3. Catturare **staged tree** dal vero index attraverso `git write-tree` senza mutare HEAD, index o working tree (Git può creare nuovi oggetti nello store del checkout gestito). Non utilizzare `git add` sul vero index.
4. Catturare **working tree** tramite `changes::snapshot_tree` (index temporaneo, `git add -A` solo sul temp index). Applicare filtri export **prima** di includere ogni blob sensibile; l'algoritmo attuale `snapshot_tree` non applica automaticamente il policy filter proposto.
5. Creare **due commit sintetici** per ciascuna revisione, uno con tree `index_tree` e uno con tree `working_tree`, entrambi con parent `head_commit`; aggiungere refs dedicate, per esempio `refs/trama/share/<share>/<state>/index` e `.../working`. **È essenziale che tutti e due i tree siano raggiungibili da refs esportate.** Un bundle che contiene solo il commit sintetico del working tree *non* garantisce la presenza dell'index tree nel destinatario. Non modificare `HEAD`/index/worktree e non fare push di queste refs su `origin`. La tecnica è stata verificata con una fixture Git locale (commit privato, modifica staged/unstaged allo stesso file, rename, staged delete, untracked).
6. Costruire inventario degli oggetti raggiungibili dai **due commit sintetici** e dal loro parent `HEAD`, incluso ogni commit locale dalla base Git remota fino a `HEAD`. Verificare quali commit sono recuperabili dal Git origin (non assumere che `origin/main` sia aggiornato o visibile). Richiedere un `fetch` autenticato oppure trasferire gli oggetti mancanti autorizzati.
7. Emettere un bundle o pack **di sole dipendenze necessarie**. Deduplicare per repo e hash quando più thread condividono la stessa base; policy di dimensione, avviso prima di trasferimenti grandi.
8. Criptare nel daemon chunk/manifest per gli endpointi partecipanti e caricare (oppure inviare P2P). Pubblicare `CheckoutSnapshotCommitted` soltanto dopo ack del content store.
9. Rilasciare la prenotazione dopo cattura locale, senza attendere upload remoto.

**Git bundle:** formato già usato per archiviazione checkout in [`crates/trama-infra/src/archive.rs`](https://github.com/alessandrobrunoh/trama-workspace/blob/558f0e4/crates/trama-infra/src/archive.rs). `git bundle verify` controlla prerequisiti: [documentazione Git](https://git-scm.com/docs/git-bundle). I bundle incrementali richiedono storia condivisa; se manca una base, non inventare un «patch fallback» che applica codice su un commit differente. Chiedere fetch della base o ricostruzione da oggetti aggiuntivi verificati.

## 4. Join/Restore algoritmico

1. Scaricare/validare il **manifest cifrato** e la firma del publisher. Costruire la mappa di **repository distinti** per tutto il perimetro dello share.
2. Per ogni repo: mostrare host, URL normalizzato e alias. Chiedere conferma prima di clonare; Git authentication del destinatario (SSH agent / credenziali esistenti) e **mai** transitare token Git dal server di collaborazione.
3. Clone/fetch in staging da Git origin, con protocolli ammessi da policy, senza shell interpolation, senza ereditarietà di opzioni Git pericolose. Se permesso negato, fermarsi con `NeedsGitAccess`; non provare a recuperare il repository privato dal server Trama.
4. Verificare `objectFormat`, base e `HEAD`, scaricare chunk cifrati mancanti e decriptare in storage locale privato. Verificare integrità, Git bundle prerequisites (`git bundle verify`) e object reachability prima dell'import.
5. Per ogni `thread × repo`, creare un checkout gestito in **staging isolato** usando la logica esistente di checkout (non generare nuovi stati casuali). Evitare di ereditare ignored secret files dal clone dell'utente tramite APFS COW: scegliere clone Git pulito per la replica o fare safe-exclude delle ignored.
6. Materializzare `HEAD`, ricostruire index da `index_tree` e working tree da `working_tree`. Strategia **solo nella staging clonata e isolata**: partire da `HEAD`, usare `git read-tree --reset -u <index_tree>` per allineare index **e file** allo stato staged (gestisce anche eliminazioni/rename), poi applicare al solo working tree il patch binario `git diff --binary <index_tree> <working_tree> | git apply --binary ...` senza `--index`. Gli untracked non presenti nell'index rimangono untracked. Non usare questa sequenza su repository utente o source; convalidare prima refs/percorsi e symlink e verificarne dopo gli OID.
7. Ricalcolare `HEAD`, staged `git write-tree`, working `snapshot_tree` e confrontarli con manifest. Verificare working status semanticamente; non cambiare il repository origin.
8. Ricollegare metadata checkpoint/fork, messaggi/commenti/annotation filtrati allo scope e mappa ID locali/globali. Materializzare aliases scout al checkout proprietario condiviso o snapshot indipendente preventivamente preparato.
9. Solo dopo verifica completa, rinominare directory staging nella destinazione gestita e cambiare DB da `Materializing` a `Ready` in sequenza recoverable. Per import multi-repo: usare un journal di recovery in caso di interruzione tra file system e DB, senza promettere transazioni distribuite.
10. Abilitare `Continue with Claude` solo esplicitamente: nuova sessione con transcript/digest della condivisione, permessi e setup locali. L'esecuzione automatica di `.agents/prepare` / `.agents/linked` importati deve richiedere consenso/policy enterprise.

## 4bis. Verifica permessi e rischio blob Git

- **Access proof:** il client che clona il Git origin non dimostra da solo che il recipient avesse diritto al codice prima di ricevere bundle/chiavi. In modalità enterprise `git_authorization_required`, la code key viene assegnata **solo dopo un'attestazione Git verificabile** sui repo inclusi; vedi documento 06. La necessità del clone è anche tecnica (recuperare base objects), ma non è un perimetro di sicurezza crittografico.
- **Secret storico/tracciato:** un bundle può contenere vecchie revisioni di file oggi assenti. Non basta filtrare working tree; controllare commit/oggetti raggiungibili nel pacchetto aggiuntivo. Se una regola nega un blob ma esso è richiesto da `head_commit` o dagli snapshot, **bloccare la condivisione di quello stato** e chiedere remediation (o diverso scope/stato): non promuovere falsamente un tree alterato come exact state.
- **Dimensione reale:** prima di cifrare e caricare calcolare content inventory, quantità di commit non presenti su Git origin e costo stimato; se servirebbe una storia Git troppo ampia, chiedere all'owner push su origin o conferma delle dimensioni consentite.

## 5. Casi speciali e limiti

| Caso | Policy MVP |
|---|---|
| Commit presenti su `origin` | Non trasferire nel blob store Trama; fare fetch autenticato |
| Commit locali non pushati | Trasferire bundle Git cifrato con prerequisiti espliciti |
| Staged / unstaged | Due tree distinti, round-trip verificato |
| Untracked non ignorati | Inclusi solo se policy consente, preferibilmente preview/size guard |
| Ignored `.env`, cache e output | **Esclusi di default**, non copiati nelle repliche importate |
| Binari, executable bit, symlink | Supportare e verificare hash/mode/containment; fail closed per entry non sicure |
| Symlink che punta fuori root | Non seguire, non dereferenziare; bloccare se il restore tenta accessi esterni |
| Git LFS | Git tree contiene pointer, non contenuto LFS: fetch separato solo con permessi Git e policy; non dichiarare completo prima di verificarlo |
| Submodules | Manifest dei gitlink, clone/fetch separati con credenziali proprie; no ricorsione automatica non autorizzata |
| Sparse checkout | V1: richiedere full materialization del perimetro Git autorizzato o dichiarare feature unsupported |
| Working tree in conflitto index | V1: block and explain; V2: staging multiplo con formato esteso |
| Checkouts `archived/removed` | Ripristinare da archive **locale** nel daemon mittente (senza eliminare backup), oppure esporre «non disponibile»; mai esportare stato inesistente senza controllo |
| 100+ GB di oggetti | Quote e consent esplicito; chunk/resume; eventuale policy fail |
| Repositories diversi in stessa family | Inventario e bundle per repo; alias e ID non ambigui |
| Clone fallito 403 | Nessun bypass da Trama server; invito non equivale a grant Git |

## 6. Identity mismatch e changed Git origin

L'URL Git nel manifest è un **suggerimento**. Un attaccante può inserirvi un URL malevolo; il destinatario deve autorizzare hostname, protocollo e repository. Verificare che i commit attesi e il fingerprint dell'origin corrispondano al manifest condiviso; un repository con stesso nome ma history differente non deve applicare patch alla cieca. Consentire mapping manuale a un clone esistente **solo se verificabile** mediante OID/object format/base, e senza mai sovrascrivere file dell'utente.

## 7. Test deterministici

- Repository di fixture con staged `A`, unstaged `B`, untracked `C`, rename, delete, symlink, bit eseguibile, binario nul-byte, commit locale, thread review + child. Verificare che il bundle includa refs **sia index sia working** e che `git read-tree --reset -u` gestisca file staged-deleted. Export/import su repo fresh clone e confronto di HEAD/index/worktree.
- Bundle prerequisito presente / assente; branch rewrite; detached HEAD; 2 repo e 3 child; ripresa chunk dopo errore.
- Test di non mutazione del source: hash index/refs/status prima/dopo export e dopo Join.
- Test di path traversal, symlink escapes, malevolent remote URL, LFS pointer non risolto, limit exceeded.
