# Složka stanice, automatická složka a import z jiných programů

Tento dokument popisuje, jak ORCA RadioAIR ukládá soubory stanice, jak do knihovny dostat nové soubory
a jak převést knihovnu z jiného odbavovacího programu (Jazler SOHO, RadioDJ, CartMaster, mAirList a další).
Návod pro uživatele je také v aplikaci: **Nápověda → Knihovna** a pod každou sekcí ovládacího panelu.

## 1. Složka stanice

Všechny soubory rádia jsou v jedné **složce stanice** (`library.root`; prázdné = `<data aplikace>/Library`).

```text
<Složka stanice>/
  Songs/<kategorie>/…            hudba – sledovaná složka, podsložka = kategorie
  Jingles/ Spots/ Sweepers/ Voice Tracks/ Assets/ Instant Jingles/ Beds/
  Import/                        automatická složka (inbox)
    Songs/<kategorie>/ Jingles/ … stejná struktura – určuje typ a kategorii
    _Chyby/                      co nešlo zařadit + .txt s důvodem
  Data/
    Images/Covers  Presenters  Shows  Spots  Station  Slides
    Recordings/  Documents/  Exports/
  _Trash/                        smazané soubory knihovny
  ČTI MĚ.txt
```

* **Ovládací panel → Složka stanice a import → Vytvořit strukturu složek** založí všechny složky včetně
  podsložek podle kategorií (z knihovny i číselníku kategorií). Existující složky se nemění.
* **Obrázky a data ve složce stanice** (`library.data_in_station`, výchozí zapnuto): nově nahrané fotky moderátorů,
  obrázky pořadů, logo, obaly skladeb a obrázky ke spotům se ukládají do `Data/Images/…`.
  Starší obrázky zůstávají, kde byly (cesty v databázi jsou absolutní).

### Databáze (SQL)

Databáze je SQLite soubor `orca.db` v datové složce aplikace
(`~/Library/Application Support/ORCA RadioAIR` na macOS, `%APPDATA%\ORCA RadioAIR` na Windows).
Obsahuje údaje o skladbách, značky, kategorie, plán, reklamu, historii vysílání a **absolutní cestu ke každému souboru**.
Zvukové soubory v databázi nejsou – jsou ve složce stanice. Proto:

* **Změnit / přesunout složku stanice** (`POST /api/station/relocate`) – soubory se přesunou (na jiný disk
  kopie + smazání), ve **všech textových sloupcích všech tabulek** se přepíše začátek cesty (i v JSON dokumentech
  plánu, i ve formě s escapovanými zpětnými lomítky), přemapují se sledované složky a automatická složka
  a sledování se restartuje. Před přesunem se automaticky vytvoří záloha databáze.
* Pokud jste soubory zkopírovali ručně, vypněte „Přesunout soubory“ a nechte „Přepsat cesty v databázi“.
* Pro jednorázové přemapování (např. jiné písmeno disku) slouží i **Zálohy a údržba → Přepsat cesty**
  a **Obecné → mapování složek** (cesta se přepisuje za běhu bez změny databáze).

## 2. Tři způsoby, jak dostat soubory do knihovny

| Způsob | Kde | Co se stane se souborem |
|---|---|---|
| Ruční nahrání | Knihovna → Nahrát / přetažení | zkopíruje se do `Typ/Kategorie` ve složce stanice |
| Automatická složka | `Import/` (lze sdílet po síti) | po dokopírování se **přesune** do `Typ/Kategorie`, převezme značky |
| Sledované složky | Ovládací panel → Sledované složky | zůstane na místě (NAS, archiv), ORCA ho jen indexuje |

### Automatická složka (inbox)

Nastavení `library.inbox`: `enabled`, `dir` (prázdné = `<stanice>/Import`), `default_kind`, `settle_s`, `read_markers`.

* Typ podle první podsložky: názvy složek knihovny (`Songs`, `Jingles`, `Spots`, `Sweepers`, `Voice Tracks`,
  `Assets`, `Instant Jingles`, `Beds`), interní názvy (`song`, `jingle` …) i česky (`Hudba`, `Znělky`, `Reklamy`,
  `Podkresy`, `Instanty`, `Ostatní`). Jinak výchozí typ.
* Kategorie podle druhé podsložky (`Import/Songs/Pop 90s/x.mp3`), resp. první, pokud první není typ.
* Soubor se zpracuje, když se jeho velikost a čas změny nezmění mezi dvěma kontrolami (každé 3 s)
  a je starší než `settle_s` sekund – kopírování velkých souborů po síti se tak nepřeruší.
* Ignorují se skryté a dočasné soubory (`.part`, `.crdownload`, `.tmp`, `~$…`), `.mmd` sidecary se přesouvají
  spolu se zvukovým souborem.
* Značky se převezmou z cart chunku (WAV), mAirList `.mmd` a RadioDJ tagu (viz níže);
  u WAV se doplní i interpret, titul a číslo cartu z cart chunku, pokud soubor nemá tagy.
* Chyby: soubor se přesune do `Import/_Chyby/` a vedle vznikne `<soubor>.txt` s důvodem.
* Stav a protokol: `GET /api/library/inbox`, okamžitá kontrola `POST /api/library/inbox/scan`.

## 3. Import z jiných programů

Ovládací panel → **Import z jiných programů**. Postup: předvolba → zdroj → nastavení → **náhled** (nic se nezapisuje)
→ úprava přiřazení sloupců → **Importovat** (na pozadí, s průběhem). Před importem se zazálohuje databáze,
na konci vznikne protokol `Reports/import_<datum>.txt` se seznamem chybějících souborů a chyb.
Import lze opakovat – soubory, které už v knihovně jsou, se jen aktualizují.

### Předvolby

| Předvolba | Zdroj | Co se načte |
|---|---|---|
| **Jazler SOHO** | CSV export knihovny | sloupce podle hlavičky (cesta / název souboru, interpret, titul, kategorie, rok, intro, mix …) |
| **RadioDJ** | SQL dump MySQL nebo CSV tabulky `songs` | `path`, `artist`, `title`, `album`, `year`, `song_type` → typ, `id_subcat` → název podkategorie, `id_genre` → žánr, `cue_times` → značky |
| **CartMaster / cart chunk** | složka WAV souborů | cart chunk (AES46): titul, interpret, číslo cartu, kategorie, časovače; LIST/INFO |
| **mAirList** | složka se soubory a `.mmd` | CueIn, Ramp1, Outro, StartNext / FadeOut, CueOut, HookIn/Out, titul, interpret |
| **Složka se soubory** | libovolná složka | tagy + všechny podporované značky, kategorie podle podsložky |
| **Playlist M3U / PLS** | `.m3u`, `.m3u8`, `.pls` | cesty, interpret a titul z `#EXTINF` / `TitleN` |
| **Obecný CSV / TSV** | libovolná tabulka | ruční přiřazení sloupců |

#### Jazler SOHO
Control Panel → Libraries → Songs (Jingles, Spots …) → Export. Při prvním exportu je potřeba v **Customize Export**
vybrat, které údaje se exportují – zaškrtněte všechny. Každou knihovnu importujte zvlášť a nastavte výchozí typ.
Sloupce se rozpoznají podle názvu hlavičky (česky i anglicky, bez ohledu na velikost písmen a diakritiku);
časy v `mm:ss`, sekundách i milisekundách rozpozná automatická jednotka. Kódování Windows-1250 se pozná samo.

#### RadioDJ
RadioDJ ukládá knihovnu do MySQL. Export: HeidiSQL (Export databáze jako SQL), phpMyAdmin (Export) nebo
`mysqldump radiodj songs subcategory genre > radiodj.sql`. Parser čte `CREATE TABLE` (pořadí sloupců)
i `INSERT INTO … VALUES` s vícenásobnými řádky, escapováním `\'` a `''`, `NULL`.

Pole `cue_times` (sekundy): `sta` start, `int` intro, `out` outro, `xta` start další položky (mix),
`end` konec, `fin`/`fou` délka náběhu/zeslabení, `hin`/`hou` háček, `pin`/`pou` smyčka.
Bez `xta` se mix spočítá jako `end − fou`. Platí první výskyt klíče (stejně jako v RadioDJ).

Převod `song_type` (lze upravit v náhledu): 0 hudba, 1 jingle, 2 sweeper, 3 voice track, 4 reklama,
14 jingle (teaser), ostatní → asset. Zkontrolujte rozdělení typů v náhledu.

#### Cart chunk (CartMaster, ENCO, Dalet, Rivendell …)
Čte se `fmt ` (vzorkovací frekvence), `cart` (Title, Artist, CutID, Category, 8 časovačů `dwUsage`/`dwValue`
ve vzorcích) a `LIST/INFO` (IART, INAM, IGNR, ICMT). Časovače: `AUDs` → start, `AUDe` → konec,
`INT*` (kromě `INTs`) → konec intra, `SEGs`/`SEG1` → mix, `OUT*` → outro, `EOD*` → konec, pokud chybí `AUDe`,
háčky `HOOs`/`HOOe`. Pokud program ukládá značky jinak (vlastní databáze), použijte jeho CSV export a obecný import.

#### mAirList
Metadata `.mmd` (XML) vedle souboru (`soubor.mp3.mmd` nebo `soubor.mmd`). Čtení je tolerantní: element
`<CueIn>1.5</CueIn>` i `<CuePoint Type="CueIn" Position="1.5"/>`, desetinná čárka i tečka, sekundy.

### Nastavení importu (`ImportOptions`)

| Pole | Význam |
|---|---|
| `preset` | `jazler_soho`, `radiodj`, `cartchunk`, `mairlist`, `folder`, `playlist`, `generic` |
| `source` | cesta k souboru nebo složce (nahraný soubor: `POST /api/import/upload?name=…` vrátí cestu) |
| `delimiter`, `has_header` | oddělovač (prázdné = automaticky `;` `,` TAB `|`), hlavička |
| `mapping` | pole ORCA → název sloupce (nebo `#3` = 3. sloupec). Doplňuje automatické rozpoznání |
| `time_unit`, `sample_rate` | `auto`, `ms`, `s`, `hms` (mm:ss / hh:mm:ss.f), `samples` |
| `path_from`, `path_to` | přemapování začátku cesty (`C:\Jazler\Songs` → `/Volumes/Radio/Songs`), lomítka se převedou |
| `files_root` | kde hledat soubory podle názvu, když cesta neexistuje (při shodě názvů rozhodne nadřazená složka) |
| `default_kind`, `kind_map` | výchozí typ, převod hodnot sloupce „typ“ |
| `category_from_folder`, `default_category` | kategorie podle složky souboru / výchozí |
| `file_mode` | `copy` (do složky stanice, `Typ/Kategorie`), `move`, `data_only` (jen doplnit údaje k souborům v knihovně – párování podle cesty, jinak podle názvu souboru) |
| `update_existing` | přepsat údaje u skladeb, které už v knihovně jsou |
| `read_file_markers` | doplnit chybějící značky ze souborů (cart chunk, `.mmd`, RadioDJ tag) |

Pole ORCA: `path`, `filename`, `artist`, `title`, `album`, `year`, `genre`, `category`, `category2`, `kind`, `duration`,
`cue_in`, `intro`, `outro`, `mix`, `cue_out`, `fade_out`, `hook_in`, `hook_out`, `cue_string` (RadioDJ),
`gain`, `isrc`, `composer`, `publisher`, `comment`, `code` (playlister kód / číslo cartu), `enabled`.

Importované značky se uloží jako ruční (`manual_cues`), takže je automatická analýza nepřepíše;
hlasitost (LUFS) a délka se změří analýzou po importu.

### API

| Metoda | Cesta | |
|---|---|---|
| GET | `/api/station` | složka stanice, databáze, přehled složek s počty skladeb |
| POST | `/api/station/structure` | vytvořit strukturu složek |
| POST | `/api/station/relocate` | `{ path, move_files, rewrite_db }` |
| GET / POST | `/api/library/inbox`, `/api/library/inbox/scan` | stav a protokol automatické složky / kontrola hned |
| GET | `/api/import/presets` | předvolby a pole |
| POST | `/api/import/upload?name=` | nahrát exportní soubor (tělo = obsah) |
| POST | `/api/import/preview` | náhled (`ImportOptions`) |
| POST | `/api/import/run` | spustit import na pozadí |
| GET | `/api/import/status` | průběh a výsledek |
| GET | `/api/fs/list?path=&files=` | procházení disku pro výběr složky / souboru |

## Zdroje formátů

* Jazler SOHO – export knihovny do CSV a „Customize Export“ (dokumentace Jazler).
* RadioDJ – databáze MySQL, tabulka `songs`, pole `cue_times` (fórum RadioDJ).
* AES46 – cart chunk v broadcast WAV; PlayIt Live a další čtou intro a segue z cart chunku.
* mAirList – metadata `.mmd` vedle souboru (wiki mAirList).
