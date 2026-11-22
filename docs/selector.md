# ORCA Music Selector a Analytika vysílání

Profesionální plánování hudby v ORCA RadioAIR. Vedle něj zůstává **jednoduché plánování** (Jednoduché clocky a
Jednoduchá pravidla výběru, jako v Jazleru). Jednoduché plánování je zároveň nouzové odbavení, když Selector nemá plán.

## 1. Audit výchozího stavu (před 0.5.0)

| Oblast | Stav před změnou | Kde |
|---|---|---|
| Výběr hudby | za běhu: clock (kroky typ + kategorie) → nejdéle nehraná skladba s odstupem skladby a interpreta, náhodnost, kompenzační kategorie | `rotation.rs::select`, `automation.rs::fill_queue` |
| Týdenní mřížka clocků | 7 × 24, výchozí clock | `config.rs::AutoConfig` |
| Plán vysílání (pořady, playlisty) | datované a týdenní playlisty, mají přednost před clocky | `sched.rs::plan_take` |
| As-played | `play_log`: start, konec, odehraná délka, zdroj, důvod výběru; všechny kanály | `db.rs::log_start/log_end` |
| Atributy | kategorie 1–3, Vocal, Properties (volné štítky), rok, žánr | `model.rs::Track` |
| Signály enginu | `TrackStarted {item, channel}` (main, overlay, bed), `TrackEnded {item_id, played_ms}` | `events.rs`, `engine.rs` |
| Oprávnění | role a oprávnění (studio, library, schedule, settings …) | `users.rs::required_perm` |

Chyběl: plán dopředu, editor, publikace, hudební atributy, cíle, přechody, projekce kapacity, simulace, analytika.

## 2. Architektura

```
Knihovna + music_attrs ──► SelTrack (snímek)                         play_log (as-played, idempotentní)
                               │                                        │      ▲
SelectorConfig (verze) ──► generator::Ctx ──► DayPlan ──► sel_logs/sel_items (revize, koncept → publikováno)
   kategorie, pravidla,        │ projekce, tvrdé filtry,                │
   cíle, přechody, clocky,     │ skóre s rozkladem, varianty            ▼
   mřížka, výjimky             └── capacity(), simulate()      automation.fill_queue → take_next() → engine
                                                                       (bez plánu → jednoduché clocky)
analytics: load_spins / plan_spins → station_report / detail / csv  (stejné výpočty pro UI, export, simulaci)
```

* `crates/orca-core/src/selector/mod.rs` – konfigurace formátu, verzované uložení (`sel_docs`, `sel_doc_revisions`), věty pravidel.
* `attrs.rs` – hudební atributy (`music_attrs`), identity interpretů (feat., &, x …), klíč díla (verze), návrhy, duplicity.
* `generator.rs` – hodiny dne (DST), evidence umístění, tvrdé filtry, skóre, varianty, diagnostika, kapacita.
* `logs.rs` – revize plánů, editor, publikace, přegenerování hodin, odbavení (`take_next`), plán vs. skutečnost.
* `analytics.rs` – metriky (verze `orca-metrics-1`), detail skladby/interpreta, CSV.
* `presets.rs` – 20 formátů a převod na konfiguraci (dvě hodiny A/B, metoda největšího zbytku).
* Server: `crates/orca-server/src/selector_api.rs` (`/api/selector/*`, `/api/analytics/*`).
* UI: `src/features/selector/*`, `src/features/analytics/AnalyticsSection.tsx`, `src/lib/api/selector.ts`.

### Migrace (DB v9)
`play_log` + `channel`, `item_uid` (unikátní – replay se nezapočte dvakrát), `plan_item_id`, `origin`, `attrs` (snímek atributů v čase hraní);
nové tabulky `music_attrs`, `music_aliases`, `sel_docs`, `sel_doc_revisions`, `sel_logs`, `sel_items`, `sel_consumed`.
Staré stanice: migrace jen přidává sloupce/tabulky, `mode` je výchozí `off` → chování se nemění.

### Generátor
1. Hodiny místního dne v UTC (`day_hours`): 23 / 25 hodin při přechodu času, dvojí 02:00 má štítky „(1.)“ a „(2.)“. Okna `[od, do)`.
2. Clock hodiny: výjimka (nejvyšší priorita, při shodě pozdější) → mřížka → výchozí.
3. Zafixují se pevné, zamčené, ruční, odvysílané a v ochranném okně ležící položky; historie = as-played (8 dní) + navazující plány.
4. Pořadí průchodů podle `pass` kategorie; imaging až po hudbě (sweeper přes intro zná následující skladbu).
5. Tvrdé filtry (počítadla vyřazení): neaktivní/pozastavená/chybí soubor, požadavky atributů pozice (neznámá ≠ nevyhovující),
   explicitnost v hodinách, odstup skladby, verze díla, interpreta (i host), interpreta v pozicích, alba, stejná hodina (tvrdá),
   max. skladby/interpreta za den, ochrana špiček, zakázané přechody, max. stejných hodnot za sebou, závazné cíle (strop).
6. Skóre 0–1 = vážený průměr: odpočinek (mezera / obrat kategorie), opomíjená (plán / očekávané spiny), cílové odstupy,
   stejná hodina (měkká), cíle zastoupení, nežádoucí přechody, plynulost energie, délka (dorovnání hodiny); × váha skladby;
   deterministický tie-break ze seedu. Rozklad se ukládá u položky.
7. Hodiny s neobsazenými pozicemi: až `variants` variant (jiné pořadí a seed), vyhrává nejméně neobsazených, pak nejvyšší skóre.
8. Náhradní kategorie jen podle konfigurace (`origin = fallback`). Neobsazené: důvody „pravidlo: vyřadil N z M“ + nápověda, co nejvíc pomůže.
9. Stejný vstup + seed = stejný plán (test `deterministic_and_artist_separation`).

### Publikace a provoz
* Koncept → publikováno: transakce zneplatní předchozí publikovanou revizi a aktivuje novou; opakování je bezpečné; `token` brání přepsání souběžné úpravy (HTTP 409).
* Úprava publikovaného plánu vytvoří koncept (kopii); publikace nemění už zařazené položky (`sel_consumed`) ani ochranné okno.
* Automatika (`mode = active`): další položka aktuální hodiny publikovaného plánu; při startu uprostřed hodiny se dávno prošlé položky vypustí; nedohrané položky minulé hodiny se vypustí (pevné body chrání hodina). Bez plánu, po vyčerpání hodiny nebo při chybějícím souboru → jednoduché clocky (nouzové odbavení, upozornění).
* `shadow`: plán se generuje a v Analytice porovnává, vysílání řídí clocky. Přepnutí `off/shadow/active` je vratné (verze konfigurace).

### Analytika – definice
* start = položka na hlavním výstupu (`channel = main`); instanty (overlay), podkresy a předposlech se nepočítají;
* kvalifikovaný spin = hrál ≥ `qualify.min_ms` (30 s) nebo ≥ `min_pct` (50 %) délky; stav ended / cut / skipped / unconfirmed;
* nepotvrzený konec = odhad z dalšího startu (max. délka); minuty se dělí přes půlnoc podle místního času;
* podíly vždy s jmenovatelem (starty / kvalifikované hudební spiny), neznámý atribut je skupina „?“;
* Jaccard sousedních dnů = |A ∩ B| / |A ∪ B| množin titulů; koncentrace = spiny top 10 / hudební spiny;
* skóre formátu: v toleranci 100, mimo 100 − (přesah / tolerance) × 50 (min. 0); pokrytí atributu < 50 % = nevyhodnotitelné; vážený průměr.

## 3. Matice požadavků

Stav: **hotovo** = funguje v UI i enginu a je otestováno; **částečně** = funguje s uvedeným omezením; **chybí**; **nelze ověřit**.
Reference (GSelector, MusicMaster, Powergold, StationPlaylist Creator, Jazler, CartMaster, RadioDJ, PlayoutONE) – funkce převzaty z veřejných popisů v zadání;
proprietární algoritmy a neveřejné edice nejsou ověřené – ORCA má vlastní implementaci.

| # | Požadavek | Stav | Důkaz (soubor / symbol) | Scénář | Riziko | Akceptace |
|---|---|---|---|---|---|---|
| 1 | Jednoduché plánování (Jazler) zachováno | hotovo | `rotation.rs`, režim `off` | stanice bez Selectoru hraje beze změny | – | `mode=off` → `fill_queue` beze změny |
| 2 | Stínový provoz na stejných datech | hotovo | `mode=shadow`, `an_station` basis plan / pva | týden plánu vs. skutečnost | – | plán se generuje, vysílání z clocků |
| 3 | Vratné zapnutí, přežití výpadku generátoru | hotovo | `take_next` → fallback clocky; verze `sel_docs` | smazaný plán → hraje se dál | – | bez publikovaného plánu se fronta plní z clocků |
| 4 | Souběžné úpravy | hotovo | `save_config(base_version)`, `bump(token)` | dva dramaturgové | – | druhý dostane 409 (test API) |
| 5 | As-played idempotentní, replay 1×, cue 0× | hotovo | `log_start_ex(uid)`, test `as_played_idempotent_and_cue` | restart položky | – | test |
| 6 | Opožděná / mimo pořadí doručená událost | částečně | `log_end` bere MAX odehrané délky | pozdní konec kratšího hraní | engine posílá v pořadí | test |
| 7 | Odlišení replikovaných výstupů / více stanic | chybí | 1 instalace = 1 stanice | – | víc stanic = víc instalací | – |
| 8 | Ruční zásah se propíše do historie i generování | hotovo | `history()` čte `play_log` | moderátor pustí jinou skladbu | – | další generování ji respektuje |
| 9 | Dílo / nahrávka / soubor, ochrana verzí | částečně | `work_key`, `protect_versions` | radio edit + remix | dílo = klíč, ne samostatná entita | test `artists_and_works` |
| 10 | Identity interpretů, feat., bez slučování | hotovo | `artist_ids`, `music_aliases` (tabulka) | „A feat. B“ hlídá oba | aliasy zatím bez UI | test |
| 11 | Jazyk, země, lokální, dekáda, žánr, zvuk, BPM, energie, nálada, známost, explicitnost, vlastní | hotovo | `MusicAttrs`, `SelTrack::attr` | filtr „Chybí: jazyk“ | – | UI Atributy |
| 12 | Neznámá hodnota ≠ nula | hotovo | `Option<…>`, UI „?“ | – | – | `matches()` vrací None |
| 13 | Platnost, sezónní nasazení, pozastavení, váha | hotovo | `active_from/to`, `paused`, `weight` | vánoční písně | – | `active_on` |
| 14 | Alternativní kategorie podle pásma, podmnožiny, balíčky | chybí | – | – | – | – |
| 15 | Hromadná editace s náhledem, import CSV s chybami, návrhy s původem | hotovo | `bulk_set(dry_run)`, `/selector/attrs/import`, `suggest` | – | – | test API |
| 16 | Audioanalýza BPM / energie dávkou | chybí | (LUFS a cue body analýza existuje) | – | BPM jen ruční / import | – |
| 17 | Duplicity k potvrzení | hotovo | `duplicates()` | – | – | UI |
| 18 | Clocky: verze, kopie, klávesnice, typy pozic, pevný čas, volitelná pozice | hotovo | `SelClock`, `Slot`, UI Schémata | – | přetažení myší: šipky / Alt+↑↓ | UI |
| 19 | Týdenní mřížka, pásma, datované výjimky s prioritou | hotovo | `clock_for`, test `exceptions_priority` | Vánoce, koncert | – | test |
| 20 | Atributy na pozici (energický hit po reklamě, CZ skladba) | hotovo | `Slot.attrs`, `R_ATTR/R_UNKNOWN` | – | – | test API |
| 21 | Průběh hodiny, přetok / podtečení, pevný bod | částečně | `retime`, `over_ms`, `est_s` | – | automatické vypuštění volitelných pozic řeší odbavení po hodinách | UI hodiny |
| 22 | UTC + pásmo, `[od, do)`, DST, 2× stejná hodina | hotovo | `day_hours`, test `dst_hours` | 25. 10. / 29. 3. | – | test |
| 23 | Tvrdé zákazy neoslabovat, výjimka s důvodem a auditem | hotovo | `candidates`, `Edit::Replace{reason}` → `audit_log` | ruční výjimka | – | UI vyžaduje důvod |
| 24 | Odstupy skladby / verze / interpreta / alba časem i pozicemi | hotovo | `Rules`, `hard()` | – | – | testy |
| 25 | Max. za hodinu/den, za sebou, stejná hodina sousedních dnů, ochrana špiček | částečně | `max_*`, `same_hour_*`, `protect_drive_times` | – | max za hodinu jen přes clock | – |
| 26 | Cíle zastoupení s jednotkou a jmenovatelem, závazné s proveditelností | částečně | `Goal`, `soft()`, `hard()` | CZ 30 % ± 5 | rozsah „týden / pásmo“ jen jako den / hodina + pásmo | simulace |
| 27 | Přechody povoleno / nežádoucí / zakázáno | hotovo | `Transition` | – | souběh více podmínek jako více řádků | – |
| 28 | Rozsah pravidla (stanice/pásmo/šablona/kategorie/pozice) | částečně | pravidla stanice, cíle s pásmem, požadavky pozice | – | pravidla na kategorii zatím ne | – |
| 29 | Twofers, tematické bloky, sady se zachováním pořadí | chybí | (pevné tituly v clocku) | – | – | – |
| 30 | Projekce: použitelné tituly, spiny, obrat, konflikty, přetížení interpretů | hotovo | `capacity()`, test kalibrace | 12 titulů × 3/h = 4 h | – | test |
| 31 | Pořadí průchodů, skóre s rozkladem, opomíjené skladby | hotovo | `pass`, `soft()`, breakdown | – | – | UI alternativy |
| 32 | Varianty se seedem, opakovatelnost, zrušení, průběh | hotovo | `variants`, `GenStatus`, `cancel_gen` | – | – | test determinismu |
| 33 | Neobsazené pozice s důvody a návrhem | hotovo | `place()`, `hints()` | úzká kategorie | – | test kalibrace |
| 34 | Reverzní / posunutá rotace | chybí | – | – | detekce stejné hodiny je | – |
| 35 | Simulace změny počtu pozic bez živých dat | hotovo | `/selector/simulate` | 3 → 2 Power | – | UI „Odhad účinku“ |
| 36 | Imaging rotace, sweeper přes intro (kontrola délky intra) | částečně | `Slot.kind=imaging`, `R_INTRO` | – | cílené vložení podle atributu skladby chybí | – |
| 37 | Editor: výměna, přesun, vložení, zámek, alternativy se skóre, přegenerování úseku | hotovo | `Edit`, `regenerate_hours` | dramaturg vyměnil hit | vložení jen přes API | UI |
| 38 | Revize, porovnání, koncept → publikováno, atomičnost, 409 | hotovo | `publish`, `/logs/compare` | – | – | test API |
| 39 | Ochranné okno, nezměnit hranou položku | hotovo | `protect_min`, `sel_consumed` | – | – | – |
| 40 | Audio náhled alternativ | hotovo | předposlech (PFL) | – | – | UI |
| 41 | Validace exportů pro cílový systém | chybí | – | – | ORCA odbavuje sama | – |
| 42 | Analytika: skutečnost / plán / srovnání, filtry | hotovo | `station_report`, `Filter` | – | zdroj odbavení filtr jen API | UI |
| 43 | Detail skladby: období, první/poslední/příští, odstupy, percentily, heatmapa, sled | hotovo | `detail()` | programový ředitel | – | UI |
| 44 | Interpret: sólo + spolupráce, podíl | hotovo | `artist_keys` | – | – | UI |
| 45 | Stav rotace (0 spinů, méně, nad, nové, sezónní) | hotovo | `/analytics/rotation` | – | – | UI |
| 46 | Stanice: spiny, unikátní, minuty, podíly s jmenovatelem, pokrytí, mezery | hotovo | `StationReport` | – | – | test |
| 47 | Opakování: top 10, koncentrace, Jaccard, stejná hodina, dvojice, série | hotovo | `station_report` | – | trojice zatím ne | test Jaccard |
| 48 | „Jak stanice působí“, „Odpovídá formátu?“ se skóre a metodou | hotovo | `format_checks` | – | aircheck = existující archiv nahrávek | UI |
| 49 | Export CSV s verzí metrik, pásmem, jednotkami | hotovo | `analytics::csv` | – | – | API |
| 50 | OSA / INTERGRAM | nelze ověřit | existující reporty `reports.rs` | – | aktuální specifikace neověřena | – |
| 51 | Presety formátů (20), průvodce 5 kroků, verze, kopie, neprepisovat stanice | hotovo | `presets::catalog/apply`, UI průvodce | nový dramaturg | hodnoty = návrhové výchozí body | test `largest_remainder` |
| 52 | Rychlý / běžný / volný stupeň rotace | hotovo | `ApplyPreset.rotation` | – | – | – |
| 53 | Oprávnění čtení / dramaturgie / publikace / admin | částečně | `/selector` → oprávnění „schedule“ | – | publikace = stejné oprávnění jako úpravy | – |
| 54 | Audit ručních výjimek | hotovo | `audit_log` selector_override / publish | – | – | – |
| 55 | Demo data označená, jen pro test | hotovo | kategorie „DEMO …“, `origin = demo` (jen testovací instalace) | – | v produkci se neinstalují | – |

## 4. Výsledky testů a výkon

* `cargo test -p orca-core --lib selector` – 10 testů: kalibrace (72 pozic, 12 titulů, 6h odstup → neobsazené pozice; 4h → rovnoměrně 6× každá),
  determinismus a odstup interpreta s featuringem, DST (25 a 23 hodin), priorita výjimek, identity a díla, kvalifikace spinu a Jaccard,
  minuty přes půlnoc, idempotence as-played, rozložení presetů, výkon.
* Výkon generátoru (test `perf_week_3000_tracks`): 3 000 skladeb, 4 kategorie, 12 pozic/h, 7 dní (2 016 pozic) –
  **1,6 s** v release buildu (9,5 s v debug), 0 neobsazených.
* Testovací stanice (169 skladeb DEMO, 6 ID, 8 sweeperů): simulace 7 dní (2 688 pozic) 1,0 s (debug), týden plánů 0,9 s,
  měsíční přehled analytiky 70 ms (1 500 přehrání), detail skladby 90 ms.
* End-to-end na testovací stanici: průvodce → uložení (stínový) → simulace → generování týdne → výměna z alternativ → 409 při souběhu →
  publikace (i opakovaná) → úprava publikovaného = nový koncept → přegenerování hodiny (16 změn) → aktivní režim: fronta plněná z plánu
  („Selector … #2 (plán)“) → plán vs. skutečnost.

## 5. Zapnutí pro stanici

1. Doplňte atributy (Atributy skladeb → Doplnit návrhy, import CSV, hromadně) – hlavně jazyk / lokální tvorbu, energii.
2. Music Selector → Průvodce formátem → projděte 5 kroků → Použít (stínový provoz).
3. Plán → vygenerujte týden, projděte hodiny, publikujte.
4. Několik dní stínového provozu, v Analytice porovnejte plán se skutečností a „Odpovídá formátu?“.
5. Music Selector → Aktivní Selector. Návrat: Jednoduché plánování (okamžitě, vratné).

## 6. Zbývající mezery

Alternativní kategorie podle pásma a balíčky variant (14), BPM/energie z audioanalýzy (16), Twofers a tematické sady (29),
reverzní rotace (34), cílené automatické vložení imagingu podle atributů skladby (36), exporty do cizích systémů (41),
více stanic v jedné instalaci (7), samostatné oprávnění pro publikaci (53), ověření OSA/INTERGRAM exportu (50).
