# ORCA RadioAIR 0.9.0 – testovací verze

## Nové: Propojení počítačů – záložní a přípravný počítač
- Role počítače: samostatný, hlavní vysílací, záložní vysílací, přípravný. Spojení chráněné společným klíčem (port 8770).
- Záložní počítač v pohotovosti: každou minutu stahuje změny z hlavního (databáze, nastavení, nové zvukové soubory), nehraje ani nestreamuje. Nastavení vázaná na počítač (zvuková karta, mikrofon, GPI, efekty, mapování složek…) se nepřepisují.
- Hlídání každou sekundu: když hlavní neodpovídá (výchozí 5 s), dlouho hraje ticho, nebo (volitelně) zastavil vysílání, záloha okamžitě převezme vysílání – automatika, streamy, upozornění. Hlavní hlídá zálohu taky (upozornění, když není jištěno).
- Návrat: automaticky, když hlavní 15 s znovu vysílá, nebo ručně tlačítkem. Ruční převzetí. Odznak Záloha / ZÁLOHA VYSÍLÁ v horní liště.
- Vysílání bez mixu (jen stream): na Icecast se připojuje jen ten, kdo vysílá. Při převzetí hlavní na požádání zálohy uvolní mountpoint (i když jen hraje ticho), při návratu se hned připojí zpět. S nastaveným fallback-mount + fallback-override na Icecastu zůstanou posluchači připojení (ověřeno: přepnutí 1–5 s bez odpojení posluchače).
- Záloha po spuštění nepřevezme vysílání, dokud hlavní aspoň jednou neviděla (2 min ochrana proti převzetí při současném startu).
- Přípravný počítač: Stáhnout z vysílání → připravit plány, kampaně, playlisty, nové skladby → Odeslat do vysílání (nové soubory se nahrají samy, na hlavním se zachová statistika hraní, předem záloha). Ochrana proti přepsání změn provedených mezitím na vysílacím počítači.

## Nové: Upozornění do Telegramu
- Vlastní bot (BotFather), vyhledání chatu jedním tlačítkem, zkušební zpráva. Převzetí a návrat vysílání se posílají vždy (i e-mailem).

## Nové: Voice tracky z domova přes web
- Samostatná stránka pro moderátory (počítač i mobil): přihlášení jménem a PINem, výběr playlistu nebo dne plánu Music Selectoru, poslech začátku a konce skladeb, nahrávání mikrofonem s ukazatelem úrovně, poslech celého přechodu, odeslání. ORCA nahrávku ořízne od ticha, převede do MP3 a vloží na místo (do plánu jako koncept).
- Běží na samostatném portu (8760), kde je jen přihlášení a voice tracky. Nové oprávnění Voice tracky z domova. Návod pro https přes Tailscale / Cloudflare Tunnel.

## Nové: Reklama v síti stanic
- Centrála posílá při reklamním bloku signál začátku a konce (s délkou bloku) regionálním stanicím.
- Regionální stanice při převzetí programu centrály odvysílá místní reklamní blok podle svého plánu, jingly, výplň do délky bloku centrály (+ zpoždění streamu) a vrátí se na program centrály. Signál přijme i z jiného systému (UDP text nebo HTTP).

## Nové: Editor přechodů (vícestopý)
- Tři stopy (konec skladby, začátek další, hlas/jingle přes ně), posouvání myší, poslech mixu, uložit jen pro toto vysílání nebo do knihovny.

## Nové: Podcast a editor nahrávek
- Střih nahrávek (vyřezání, ořez), znělka na začátek a konec, sweeper přes úvod s ducknutím, prolnutí, sjednocení hlasitosti (−16 LUFS), poslech, export MP3, epizody s RSS feedem (iTunes) a nahrání na FTP/SFTP. Automatický podcast z nahraného pořadu.

## Nové: Externí efekty VST / VST3 / AU a hardwarový procesor
- Průvodce zapojením (plugin host + virtuální kabel BlackHole / VB-Cable, nebo procesor Optimod/Omnia), návrat zpracovaného zvuku pro streamy a nahrávání, pojistka při výpadku efektů.

## Nové: Now playing na FTP / SFTP / TuneIn / HTTP / TCP / UDP
- Služby se šablonou textu, zkouška jedním tlačítkem, stav posledního odeslání.

## Nové: Globální klávesové zkratky
- Ovládání studia klávesami i když je ORCA na pozadí (desktopová aplikace).

# ORCA RadioAIR 0.8.0 – testovací verze

## Nové: Monitoring – nahrávání vysílání
- Nepřetržitý záznam 24/7 do souborů po hodinách (15 min – celý den), automatické mazání po N dnech a nad limit místa, odhad místa na disku.
- Zdroje: program ORCA (co jde do vysílání), vstup zvukové karty (odposlech éteru z FM/DAB přijímače), internetový stream (URL). Více záznamníků najednou.
- Archiv: den na časové ose (záznam, odvysílané položky, mikrofon, ticho, výpadky), poslech od libovolného času (jen do předposlechu), výřez MP3/WAV/FLAC, důkaz odvysílání položky ±3 s, aircheck (jen mluvené vstupy), zamčení dne proti smazání.
- Kontrolní součty SHA-256 dokončených souborů a jejich ověření; hlídání ticha s upozorněním; upozornění při výpadku záznamu.
- Pravidelné záznamy pořadů (celé nebo aircheck) po skončení pořadu, volitelně rovnou do knihovny pro reprízu.
- V horní liště ● REC (při chybě bliká).

## Předposlech nikdy do vysílání
- ORCA nepustí předposlech na výstup, kam jde vysílání; poslech v editoru skladby, náhled časomíry i porovnání processingu se při riziku přesměruje do předposlechu nebo zablokuje.
- Výběr výstupu předposlechu v Ovládací panel → Audio s kontrolou (vysílání / předposlech / okno aplikace).

## Knihovna: přesun souborů mezi druhy a kategoriemi
- V detailu souboru Druh a Kategorie, hromadně „Přesunout do…“, nebo přetažení souboru na záložku druhu. Soubor se přesune i ve složce na disku.

## Časomíra ve frontě
- Naplánované časomíry (i z pravidelných událostí na 3 h dopředu) jsou vidět přímo ve frontě vysílání: čas dlouhého tónu, pípání, podkres, první pípnutí; zrušení jednoho výskytu.

# ORCA RadioAIR 0.7.9 – testovací verze

## Oprava: studio se nenačetlo („Obrazovku se nepodařilo načíst“, React #185)
- V rozložení studia Široké a Maximum se obrazovka zacyklila na panelu Jingly (stejná chyba hrozila u Streamů). Opraveno.
- Pojistka: chyba v jednom panelu studia už neshodí celou obrazovku – panel ukáže hlášku s tlačítkem Zkusit znovu.
- Chybová obrazovka má tlačítko Výchozí rozložení studia.

# ORCA RadioAIR 0.7.8 – testovací verze

## Časomíra (odpočet v přesný čas)
- Studio → Fronta vysílání → ikona stopek: pípání odpočtu s dlouhým tónem přesně v zadaný čas (např. celá hodina před zprávami); po posledním tónu hned hraje další položka.
- 10 vestavěných zvuků (klasické pípání, vysoké, jemný tón, digitální hodinky, tikání, sonar, stoupající, marimba, hluboký puls, zpravodajský blip) s poslechem ▶ a vlastní zvuk (jednotka max. 1 s, závěrečný tón max. 3 s).
- Volitelný podkres namíchaný pod odpočet (hlasitost, předehra, dozvuk).
- Kdy: v přesný čas (aktuální položka se plynule přeruší), jako další, hned; naplánované časomíry lze zrušit.
- Pravidelně přes Události (akce Časomíra) a jako položka playlistu / pořadu.

## Panely v samostatném okně
- Panel otevřený v novém okně se ve studiu skryje (zástupce s tlačítkem Vrátit do studia); vrátit jde i tlačítkem v samotném okně.

# ORCA RadioAIR 0.7.7 – testovací verze

## Novinky v 0.7.7
- **Zvukový processing – hlavní vypínač:** velký pruh se stavem (Processing běží / vypnutý / spouští se / problém / servisní bypass), přepínač Zapnuto–Vypnuto s potvrzením (zapne se hned, bez nasazování), **Diagnostika** s kontrolním seznamem a tlačítkem opravy u každého problému (vypnout bypass, zapnout ochranu streamů, restartovat streamy, otevřít Audio). Upozornění u presetu Transparent (jen ochrana výstupů).
- **Jednoduché clocky:** krok s **konkrétním souborem** (📌 např. znělka zpráv); barevná značka u každého kroku shodná s náhledem hodiny; u kroku odhad délky (průměrná stopáž kategorie / přesná délka souboru) a čas začátku v hodině; **Délka hodiny** – kolik zbývá do 60 minut a kolik skladeb se ještě vejde; náhled hodiny podle skutečných délek, **zvětšení** a **stažení PNG** s názvem hodiny.
- **Analytika – Obchod a provoz:** reklamní spoty a minuty, reklama za hodinu a hodiny nad 12 minut, odhad tržeb, **plnění kampaní** (objednáno / odvysíláno / kolik by mělo být dnes), klienti a obory inzerentů, skladba programu, reklamní zátěž po hodinách a týdnu, podíl živého vysílání, imaging za hodinu, kvalita provozu (nedohrané spoty, spoty bez kampaně, přeskočené skladby). **U všech údajů analytiky ikona ⓘ** s vysvětlením.
- **Poznámky ve frontě vysílání a playlistech:** memo nebo vlastní text jako odsazená položka bez času, nic nehraje; kliknutím se otevře velkým písmem; vložení přetažením z panelu Memos, ikonou + u memo, ikonou lístečku ve frontě nebo položkou Poznámka v playlistu; při dojetí vysílání sama zmizí.

# ORCA RadioAIR 0.7.6 – testovací verze

## Opravy v 0.7.6
- **Video → zvuk:** s novějšími verzemi ffmpeg (7/8 na macOS) mohla úprava zvuku skončit chybou „Invalid argument / Could not open encoder“. Převod teď při selhání úprav automaticky zkusí bez oříznutí ticha a pak jako čistý převod – zvuk z videa se tak převede vždy, když video zvukovou stopu má (ORCA upozorní, která úprava se vynechala).
- Video bez zvukové stopy hlásí srozumitelně „video nemá zvukovou stopu“; chybová hlášení jsou kratší (jen podstatné řádky).

# ORCA RadioAIR 0.7.5 – testovací verze

## Novinky v 0.7.5
- **Studio: okno Disk** vedle stavu streamů – kroužek se zaplněním disku (ORCA / ostatní data), volné místo v GB a %, barevně (zelená / oranžová pod 20 % / červená „Málo místa!“ pod 10 % nebo 5 GB), podrobnosti po najetí myší, kliknutím Úložiště.
- **Hlídání místa:** engine každých 10 minut kontroluje disky a při nedostatku místa pošle upozornění (nejvýš jednou za 6 hodin).

# ORCA RadioAIR 0.7.4 – testovací verze

## Opravy v 0.7.4
- **Chyba „This page didn't load“ při prvním otevření po instalaci nové verze:** okno aplikace drželo v mezipaměti stránku předchozí verze, která odkazovala na už neexistující soubory. Engine teď stránku vždy načítá čerstvou (soubory s hashem cachuje natrvalo, chybějící vrací 404 místo stránky) a aplikace se v takovém případě sama jednou obnoví.
- Chybová obrazovka je česky, ukazuje text chyby a tlačítko Načíst znovu; vysílání chyba obrazovky neovlivní.

# ORCA RadioAIR 0.7.3 – testovací verze

## Novinky v 0.7.3
- **Video → zvuk** (Ovládací panel → Knihovny): videa (MP4, MOV, MKV, WEBM, AVI…) se přetáhnou do ORCA nebo zkopírují do složky Video a převedou na zvuk pro vysílání – do knihovny (typ + kategorie) nebo do vlastní složky, FLAC / WAV / MP3 320. Převod ručně tlačítkem nebo automaticky; video po převodu ponechat, smazat hned nebo po N dnech; hromadné mazání převedených videí.
- **Příprava zvuku z videa:** oříznutí ticha, sjednocení hlasitosti na cílové LUFS (EBU R128, −16 LUFS výchozí), volitelně úprava řeči. Vysílací processing se do souboru nedělá – běží až na výstupu (jinak by se zvuk zpracoval dvakrát).
- **Úložiště** (Ovládací panel → Systém): kolik místa zabírá ORCA na disku – pruh disku (ORCA / ostatní / volno), graf a seznam částí (skladby, reklamy, jingly… s kategoriemi, videa, koš, zálohy, databáze, cache), upozornění při nedostatku místa, smazání převedených videí a vysypání koše.
- YouTube odkazy ORCA nepřehrává ani nestahuje (nespolehlivé pro vysílání, podmínky YouTube); vlastní videa stáhnete v YouTube Studiu a vložíte do Video → zvuk.

# ORCA RadioAIR 0.7.2 – testovací verze

## Novinky v 0.7.2
- **Aktualizace jedním tlačítkem** – ORCA sama zjistí novou verzi (po spuštění a pak každých 6 h), ukáže tlačítko v horní liště a v Obecné → Aktualizace. „Aktualizovat a restartovat“ stáhne podepsaný balíček, nainstaluje ho a restartuje aplikaci. Tuto verzi 0.7.2 je potřeba nainstalovat ještě ručně, další už půjdou tlačítkem.
- **Nastavení se při aktualizaci zachová** – data (nastavení, knihovna, plány, reklamy, uživatelé, logy) jsou ve složce dat mimo aplikaci; před prvním spuštěním nové verze se navíc automaticky zálohují do Backups/pred-aktualizaci_… (posledních 5).
- **Ovládací panel:** tlačítka Sbalit / Rozbalit všechny nabídky vedle „Zpět do studia“; pruh Uložit / Vrátit má tlačítka vlevo, aby je nepřekrývala upozornění vpravo dole.

# ORCA RadioAIR 0.7.1 – testovací verze

## Opravy v 0.7.1
- **Streamy:** mezera nebo „http://“ v poli Server (častá při vložení adresy) způsobovala chybu „Failed to resolve hostname“. ORCA teď adresu serveru, port vložený za dvojtečkou, mount a uživatele sama očistí – při psaní i před připojením.
- **Streamy:** chybové hlášení začíná srozumitelným vysvětlením (server nenalezen / špatné heslo / port nepřijímá spojení / mount obsazený) a **heslo se v hlášení ani v upozorněních už nezobrazuje**.

## Novinky v 0.7.0
- **Výběr mikrofonu ve studiu** – kolečko vedle MIC: „Přes mixážní pult“ (jen ztlumí hudbu) nebo „Přes počítač“ (USB mikrofon / zvuková karta, ORCA mikrofon přimíchá do vysílání); výběr vstupu, kanálu, zesílení, šumové brány a živý měřák. Odpojený vstup se po připojení sám napojí.
- **Fronta vždy s celou další hodinou** (jako Jazler) – v AUTO je ve frontě připravená i příští hodina, oddělená pruhem „23:00 další hodina · počet · délka“. V RUČNĚ je pod frontou náhled další hodiny (doplňování v ručním režimu volitelně v Pravidlech automatiky).
- **Informace na displeji rádia** – playlisty sdělení pro DAB+ (text i obrázek současně) a RDS: skladba, další skladba, pořad, další pořad, stanice, kontakt, datum a čas, počasí, počasí pro vybrané lokality, zprávy z RSS/Atom s fotkou, snímek webkamery s popisem a počasím, promo/reklama s logem a odkazem (ClickThroughURL), vlastní text. Rozvrh dny × hodiny, živý náhled displeje a přeskočení sdělení.
- **Informace pro moderátora ke skladbě** – text u skladby, ikona žárovky ve frontě a knihovně, automaticky ve studiu při hraní (zvětšení pro čtení).
- **AI texty pro moderátory** – Claude (s ověřováním fakt na webu), ChatGPT, Ollama v počítači nebo služba s OpenAI API; předvolby podle stylu stanice, vlastní pokyny, pravidla přesnosti „nic nevymýšlej“; generování u skladby, pro vybrané skladby nebo hromadně na pozadí. Klíč zůstává jen v tomto počítači.
- **Platnost souborů** (ne hudba) – od–do, dny v týdnu, časové okno; mimo platnost je automatika, Music Selector ani clocky nezařadí. Štítek stavu v knihovně, okno **Po platnosti** se smazáním vybraných/všech a automatický úklid po N dnech.
- **Sloupce knihovny** – pořadí přetažením záhlaví, šířka tažením okraje (dvojklik = výchozí), zobrazení/skrytí sloupců (Album, Rok, Přidáno, Platnost…); pamatuje se pro každou záložku.
- Manuál: nové články (mikrofon, další hodina, informace na displeji, informace pro moderátora, nastavení AI, platnost, sloupce) a nápověda sekcí.

### Test 0.7.0
1. Studio → kolečko u MIC → Přes počítač → vyberte vstup, mluvte (měřák), MIC zapne mikrofon i ztlumení hudby.
2. AUTO: ve frontě je pruh další hodiny; RUČNĚ: pod frontou náhled další hodiny.
3. Ovládací panel → Informace na displeji → zapnout, přidat informační sadu (počasí pro lokality, RSS, promo) → zařadit do playlistu → živý náhled; ve složce DAB je jen aktuální slide.
4. Informace o skladbách (AI) → služba, klíč, Načíst modely → Uložit → u skladby Vygenerovat pomocí AI → ověřte text → ve studiu se ukáže při hraní.
5. Jingle → Platnost: jen středa 6–9 → štítek „dnes ne“; platnost do včera → Po platnosti → Smazat vše.
6. Knihovna → přetáhněte záhlaví a okraj sloupce → po obnovení stránky zůstane.

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
- **Fronta vysílání** – dřívější „Decky“ přejmenovány (v úzkém panelu „Fronta“).
- **Pruh Výstupy** ve studiu: zvuková karta a všechny streamy – zelená běží (s dobou běhu), oranžová se připojuje, červená chyba, šedá vypnuto; detail po najetí myší.
- **Přehrát hned** už nehraje přes sebe: hrající položka se krátce stáhne (max. 0,6 s) a nová začne až po jejím doznění; u položky z decků se nezdvojí. Podkres a instanty pokračují.
- **Podkres**: značky ON i OFF jasně žluté, položky mezi nimi se štítkem „přes podkres“; stav „doznívá“ se u podkresu už nezobrazuje trvale.
- **Reklamy** sytě červeně s ikonou peněz v deckách i playlistech.
- **macOS: podepsaná a notarizovaná verze** – sestavení se podepíše certifikátem Apple Developer ID a notarizuje, jakmile jsou nastavené secrets (návod `docs/macos-podpis.md`); aplikace má oprávnění ke zvukovému vstupu a popis pro dotaz na mikrofon.
- Ovládací panel: výraznější a sbalitelné skupiny menu; nápověda každé části začíná laickým vysvětlením „Jednoduše řečeno“.

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
