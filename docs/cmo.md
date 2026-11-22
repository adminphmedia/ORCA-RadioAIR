# Hlášení OSA a INTERGRAM (`crates/orca-core/src/cmo.rs`)

## K čemu to je
Rozhlasová stanice každý měsíc hlásí kolektivním správcům, co odvysílala:
- **OSA** – autoři hudby a textů (licenční smlouva pro rozhlasové vysílání, čl. 6): hlásí se všechna užitá díla
  včetně znělek, reklam a podkresů – datum, název, všichni autoři (i textaři), stopáž, značka výrobce u nahrávek
  a způsob užití u hudby, která není v popředí; měsíčně do 15 pracovních dnů po skončení měsíce, po dnech a hodinách.
  Smlouva stanoví smluvní pokuty za pozdní a za chybné či neúplné údaje.
- **INTERGRAM** – výkonní umělci a výrobci zvukových záznamů: měsíční playlisty; přiřazení nahrávek stojí hlavně na ISRC.

## Jak to funguje
1. Zdroj je skutečný log vysílání (`play_log`, jen hlavní výstup `channel='main'`), ne plán.
2. Údaje se berou z knihovny (opravy po odvysílání se projeví), jinak ze snímku v logu (v10: `lyricist`, `label`).
3. Vlastní produkce (znělky, předěly…) bez údajů dostane značku a autora z nastavení.
4. Kontrola úplnosti po dílech: chybějící autor hudby, autor textu (jen zpívané skladby; `vocal` = instrumentální → nevyžaduje se),
   značka výrobce, ISRC (jen skladby, kontrola tvaru CC-XXX-RR-NNNNN), stopáž.
5. Export CSV podle šablony sloupců (pořadí, názvy), oddělovač, kódování UTF-8 s BOM nebo Windows-1250.
6. Termín = 15. pracovní den po měsíci (víkendy a státní svátky ČR včetně Velkého pátku a Velikonočního pondělí).

## API
| Metoda | Cesta | Popis |
|---|---|---|
| GET/PUT | `/reports/cmo/config` | nastavení a šablony (PUT vyžaduje oprávnění „reports“) |
| GET | `/reports/cmo/validate?month=RRRR-MM&profile=osa\|intergram` | kontrola úplnosti, termín, poznámky |
| GET | `/reports/cmo/osa.csv?month=…`, `/reports/cmo/intergram.csv?month=…` | soubor k odeslání |

## Omezení – ověřte u správce
Veřejně dostupná specifikace datových rozhraní INTERGRAMu (DR3/DR4/DR6/DR7) ani závazný vzor sloupců OSA nebyly k dispozici.
Výchozí šablony vycházejí z náležitostí podle licenční smlouvy OSA a z údajů, které INTERGRAM potřebuje k přiřazení nahrávek.
Pokud správce pošle vlastní vzor, upravte pořadí a názvy sloupců v ovládacím panelu (Hlášení OSA / INTERGRAM → Nastavení hlášení).

Zdroje: licenční smlouva OSA pro rozhlasové vysílání (osa.cz), INTERGRAM – rozhlasové vysílání (intergram.cz).
