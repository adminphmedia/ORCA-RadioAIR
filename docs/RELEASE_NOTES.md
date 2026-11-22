# ORCA RadioAIR 0.2.0 – testovací verze

## Instalace

**macOS (Apple Silicon i Intel, macOS 11+)** – soubor `ORCA RadioAIR_0.2.0_universal.dmg`
1. Otevřete DMG a přetáhněte ORCA RadioAIR do složky Aplikace.
2. Aplikace zatím není podepsaná certifikátem Apple. Při prvním spuštění: pravým tlačítkem na aplikaci → Otevřít → Otevřít.
   Pokud macOS hlásí „je poškozená“, spusťte v Terminálu: `xattr -dr com.apple.quarantine "/Applications/ORCA RadioAIR.app"`
3. Při prvním přehrávání / nahrávání povolte přístup k mikrofonu a zvuku, pokud se systém zeptá.

**Windows 10/11 (64 bit)** – `ORCA RadioAIR_0.2.0_x64-setup.exe` (nebo `.msi`)
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
