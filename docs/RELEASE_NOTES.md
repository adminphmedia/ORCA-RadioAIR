# ORCA RadioAIR 0.6.0 – testovací verze

## Novinky v 0.6.0
- **Automatická analýza BPM a energie** – Atributy skladeb → Analyzovat BPM a energii: tempo a energie 1–5 z poslechu souboru, na pozadí, ruční a importované hodnoty se nepřepíší.
- **Twofer, tematické bloky a sady skladeb** – dvě skladby stejného interpreta za sebou, bloky se společným tématem, pevně vybrané sady (výročí, speciály) jako pozice ve schématu.
- **Obrácená a posunutá rotace** – skladby se nevracejí do stejných hodin; odhad kapacity upozorní, když obrat „sedí“ na 24 h.
- **Automatické jingly podle skladby** – např. jingle před každou českou skladbou nebo sweeper přes intro energického hitu; limit za hodinu a odstup; po výměně skladby v editoru se přepočítají; nerozdělí twofer ani sadu.
- **Kategorie podle denní doby** – v daném pásmu (noc) se místo kategorie bere jiná.
- **Samostatné oprávnění na publikaci** – role Hudební ředitel; editor plán upraví, ale nepublikuje (stávajícím editorům se oprávnění zachovalo).
- **Hlášení OSA / INTERGRAM** – kontrola úplnosti za měsíc s termínem odevzdání, doplnění údajů přímo u skladby, export CSV podle upravitelné šablony (UTF-8 / Windows-1250). Nová pole skladby Autor textu a Značka výrobce (načítají se i z tagů a importu).
- **Moderátorská časová osa** ve studiu – zvuková křivka, zóny intro / zpěv / outro, značky a velký pruh „co teď“ s odpočtem; intro další položky.
- **Barvy kategorií** – řádky v deckách i playlistech mají podklad v barvě kategorie a ikonu v plné barvě; kategorie bez barvy dostanou automatickou.
- **Logo stanice** vlevo v horní liště studia.
- Ovládací panel: výraznější nadpisy skupin menu; nápověda každé části začíná laickým vysvětlením „Jednoduše řečeno“.

### Test 0.6.0
1. Atributy skladeb → Analyzovat BPM a energii → po dokončení zkontrolujte sloupce Energie a BPM.
2. Nastavení formátu → Automatické jingly podle skladby → pravidlo Jazyk = cs, jingle před skladbou → Plán (editor) → Vygenerovat → v hodinách je „jingle podle skladby“.
3. Schémata → hudební pozice → Twofer; tlačítko Tematický blok.
4. Uživatelé → role Editor / dramaturg nemůže publikovat, Hudební ředitel ano.
5. Hlášení OSA / INTERGRAM → minulý měsíc → Doplnit chybějící údaje → Stáhnout CSV.

## Dříve (0.5.0)
- **ORCA Music Selector** – profesionální plánování hudby vedle jednoduchých clocků (Jazler): režimy Jednoduché / Stínový provoz / Aktivní (vratné, bez plánu nouzově z clocků).
- **Průvodce formátem** – 20 formátů (CHR, CHR – ENERGIC CZ, Hot / Mainstream / Soft AC, Adult Hits, Classic Hits, Classic Rock, Alternative/AAA, Dance, Hip-hop/R&B, Urban AC, Country, Jazz, Klasika, World/Folk, Latin, News/Talk, Vánoce): mapování na kategorie, kontrola kapacity (obrat, proveditelnost odstupů), simulace týdne bez zápisu, použití jako verze.
- **Pravidla formátu** jako věty: odstup skladby, interpreta (i hostujícího), verzí téže písně, alba, stejné hodiny, maxima za den, explicitní obsah podle hodin, max. stejných hodnot za sebou, energie; cíle zastoupení (např. CZ 30 % ± 5) a přechody; pokročilé váhy; verze a obnova.
- **Schémata a mřížka** – pozice hudby s požadavky atributů, pevné tituly, imaging (i přes intro), reklama, voice track, zprávy, poznámky; pevné časy; týdenní mřížka, denní pásma, datované výjimky s prioritou; přechody času.
- **Plán (editor)** – generování týdne na pozadí, hodiny s odhadem délky, alternativy se skóre a rozkladem, poslech, zámky, přesun, přegenerování hodiny, revize a porovnání, publikace s ochranou proti souběžné úpravě, ruční výjimky s důvodem v auditu.
- **Atributy skladeb** – jazyk, lokální tvorba, energie, BPM, zpěv, nálada, známost, explicitnost, platnost, váha; návrhy s jistotou, hromadné změny s náhledem, import CSV, duplicity a verze.
- **Analytika vysílání** – skutečnost / plán / srovnání, den–týden–měsíc, spiny, unikátní skladby a interpreti, podíly s jmenovatelem, top 10, Jaccard, opakování ve stejné hodině, energie během dne, soulad s formátem se skóre, detail skladby a interpreta s přesným sledem přehrání, stav rotace, CSV export.
- As-played je idempotentní (replay se počítá jednou), odlišuje hlavní výstup od instantů a pamatuje si atributy v čase hraní.
- Opravy: obsah dialogů a karet už nepřetéká (dlouhé názvy souborů), responzivní horní lišta.
- Dokumentace `docs/selector.md` (architektura, matice požadavků, testy, výkon, zapnutí pro stanici).

### Test 0.5.0
1. Atributy skladeb → Doplnit návrhy; u pár skladeb nastavte jazyk a energii.
2. Music Selector → Průvodce formátem → např. Hot AC → zmapujte kategorie → Zkontrolovat kapacitu → Simulovat týden → Použít.
3. Plán (editor) → Vygenerovat týden → rozbalte hodinu → Nahradit skladbu z alternativ → Publikovat.
4. Music Selector → Aktivní Selector → ve studiu má fronta důvod „Selector … (plán)“.
5. Analytika vysílání → 7 dní; klikněte na skladbu v top 10 → detail se sledem přehrání.

## Dříve (0.4.0)
- **Složka stanice** – jedna složka pro celé rádio: Songs, Jingles, Spots … s podsložkami podle kategorií, automatická složka Import a Data (obrázky moderátorů, pořadů, obaly, obrázky ke spotům, nahrávky). Tlačítko Vytvořit strukturu složek, přehled složek s počty skladeb.
- **Přesun složky stanice** na jiný disk / NAS – soubory se přesunou a cesty v databázi (skladby, obrázky, plán, historie) se přepíšou; předem se zazálohuje databáze.
- **Automatická složka (Import)** – soubor nakopírovaný do Import/Typ/Kategorie se po dokopírování sám zařadí do knihovny i se značkami; chyby do Import/_Chyby s vysvětlením; protokol v nastavení.
- **Import z jiných programů** – předvolby Jazler SOHO (CSV), RadioDJ (SQL dump / CSV, značky cue_times), CartMaster a broadcast WAV (cart chunk), mAirList (.mmd), složka se soubory, playlist M3U/PLS a obecný CSV s vlastním přiřazením sloupců a jednotkou času. Náhled před importem, hledání souborů podle názvu, přemapování cest, protokol importu.
- Výběr složky / souboru na disku přímo v nastavení (Procházet…).
- Manuál: nové články Složka stanice, Automatická složka, Import z jiných programů, Převod z Jazleru a z RadioDJ; dokumentace `docs/import.md`.

### Test 0.4.0
1. Ovládací panel → Složka stanice a import → Změnit / přesunout složku stanice → vyberte např. složku na externím disku → Přesunout. Ověřte, že knihovna hraje a v přehledu jsou počty skladeb.
2. Vytvořit strukturu složek → ve Finderu / Průzkumníku zkontrolujte Import/Songs/<kategorie> a Data/Images.
3. Nakopírujte MP3 do Import/Songs/<kategorie> a WAV do Import/Znělky → za pár sekund je v knihovně a v protokolu automatické složky.
4. Jazler: vyexportujte knihovnu Songs do CSV (Customize Export – všechna pole), zkopírujte hudbu staré stanice na Mac, Import z jiných programů → Jazler SOHO → nahrajte CSV, vyplňte „Kde jsou soubory staré stanice“ → Načíst náhled → zkontrolujte intro a mix → Importovat.

## Dříve (0.3.0)
- **Hudební podkres** – značky Podkres ON / OFF v playlistu i deckách, knihovna Podkresy (intro, outro, hlasitost, náběh/doznění), plynulá smyčka.
- **Živý vstup** – playlist se zastaví, ve studiu svítí ŽIVĚ s časem; pokračuje tlačítkem Pokračovat, klávesou (F12 / ⌥Enter) nebo stažením šavle na pultu.
- **Pult a fader start (GPI)** – MIDI (CC / nota), sériový port / USB GPIO (CTS/DSR/DCD/RI), TCP příkazy, HTTP pro Stream Deck / Companion.
- **Editor skladby** – skutečná zvuková křivka, přehrávání, zoom, značky Cue In / Intro / Outro / Mix / Cue Out / Hook (myší, klávesami, „sem“), poslech do sluchátek.
- **Studio** – zóny intro/outro „MLUVTE“, odpočet 30 s oranžově / 15 s červeně / 5 s bliká, plynulé měřiče (10 ms) s grafem hlasitosti v čase, panel Měření, nastavitelná velikost panelů, decky s barvou kategorie a ikonami, výraznější instanty (numerická klávesnice 1–0).
- **Vyrovnání hlasitosti (LUFS)** – celý program se srovná na cílovou hlasitost (soubory, převzaté vysílání, živé vstupy).
- **Reklama** – klienti, objednávka a denní limit, pozice v bloku, cena, hlídání konkurence, obrázek a text spotu v DAB+ (i RDS), rozplánování do bloků, obsazenost bloků, protokol o odvysílání pro klienta (tisk / PDF).
- **Klávesy** – fungují podle fyzické klávesy na Macu i Windows; každá akce má i alternativu ⌥/Alt + číslo (na Macu jsou F1–F12 systémové – fn+F1 nebo nastavení klávesnice).
- **Obrázky** – nahrávání libovolného formátu včetně WebP, GIF, BMP, TIFF a HEIC z iPhonu.
- **Nápověda** – rozbalovací popis pod každou částí ovládacího panelu a rozšířený manuál.

## Dříve (0.2.0)

## Instalace

**macOS (Apple Silicon i Intel, macOS 11+)** – soubor `ORCA RadioAIR_0.5.0_universal.dmg`
1. Otevřete DMG a přetáhněte ORCA RadioAIR do složky Aplikace.
2. Aplikace zatím není podepsaná certifikátem Apple. Při prvním spuštění: pravým tlačítkem na aplikaci → Otevřít → Otevřít.
   Pokud macOS hlásí „je poškozená“, spusťte v Terminálu: `xattr -dr com.apple.quarantine "/Applications/ORCA RadioAIR.app"`
3. Při prvním přehrávání / nahrávání povolte přístup k mikrofonu a zvuku, pokud se systém zeptá.

**Windows 10/11 (64 bit)** – `ORCA RadioAIR_0.5.0_x64-setup.exe` (nebo `.msi`)
1. Spusťte instalátor. SmartScreen: Další informace → Přesto spustit.
2. Instaluje se pro všechny uživatele (vyžaduje oprávnění správce).

ffmpeg (streamy, rebroadcast) je v obou instalátorech přibalený. Data a nastavení jsou v
`~/Library/Application Support/ORCA RadioAIR` (macOS) nebo `%APPDATA%\ORCA RadioAIR` (Windows).
Kompletní popis funkcí je přímo v aplikaci: Ovládací panel → Uživatelský manuál (nebo ikona ? v horní liště).

## Co zkusit (testovací scénář)

1. **Knihovna** – Ovládací panel → Knihovna → Přidat soubory: nahrajte 10–20 skladeb a pár znělek. Počkejte na analýzu, otevřete skladbu (tužka) a přetáhněte značku Intro. Vyzkoušejte předposlech (sluchátka).
2. **Studio** – přepněte AUTO a F1. Sledujte červeně zvýrazněnou hrající položku, odpočet (posledních 10 s červeně, 5 s bliká) a šedý blok „Další“. Přetáhněte skladbu z knihovny do decků, spusťte znělku ze zásobníku, otevřete panel v samostatném okně na druhém monitoru.
3. **Audio** – Ovládací panel → Audio: vyberte zvukovou kartu.
4. **Stream** – Ovládací panel → Streamy: zadejte svůj Icecast a spusťte. ON AIR a pole upozornění ve studiu musí ukazovat stav.
5. **Plánování** – vytvořte playlist s položkou „Poslední nahraný (n-tý)“ a naplánujte ho za pár minut; vytvořte pořad v programové mřížce.
6. **Rebroadcast** – Ovládací panel → Rebroadcast: zadejte URL jiného streamu, Otestovat, Převzít teď.
7. **DAB+ / RDS** – Stanice a texty, DAB+, RDS: zkontrolujte živý náhled DLS, slidů a RadioTextu.
8. **Zvukový processing** – vyberte preset, A/B náhled, nasaďte, vraťte revizi.
9. **Obecná nastavení** – vytvořte administrátora a moderátora s PINem, zapněte povinné přihlášení a ověřte, že moderátor nemůže do nastavení. Zapněte spouštění se systémem a restartujte počítač. Zálohujte teď. Nastavte hlídání ticha a e-mail (Poslat zkušební e-mail).
10. **Manuál** – zeptejte se asistenta např. „jak přidat skladbu a vyznačit intro“.

Na reálném hardwaru zatím neověřeno: spouštění se systémem, priorita procesu a zákaz uspání, předposlech na fyzické
zvukové kartě, DAB+ enkodér (ODR), dlouhodobý 24h test. Hlášení chyb prosím i se souborem z Reports/ nebo snímkem obrazovky.
