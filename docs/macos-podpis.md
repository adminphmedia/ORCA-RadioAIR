# Podepsaná verze pro macOS (Apple Developer ID + notarizace)

## K čemu to je
Bez podpisu macOS při prvním spuštění hlásí, že aplikaci „nelze ověřit“, a musí se ručně povolit
(pravé tlačítko → Otevřít, nebo Soukromí a zabezpečení → Přesto otevřít).
S podpisem certifikátem **Developer ID Application** a **notarizací** u Apple se ORCA RadioAIR nainstaluje
a spustí jako každá běžná aplikace – bez varování, na libovolném Macu.

Sestavení (GitHub Actions → „Build installers“) podepisuje automaticky, jakmile jsou v repozitáři nastavené
níže uvedené secrets. Bez nich vznikne jako dosud ad-hoc podepsaná verze.

## Co je potřeba (jednorázově)

### 1. Apple Developer Program
- Registrace na <https://developer.apple.com/programs/> (99 USD ročně), jako fyzická osoba nebo firma
  (pro firmu je potřeba číslo D-U-N-S). Jméno / firma se pak zobrazuje jako vývojář aplikace.

### 2. Certifikát „Developer ID Application“
Na Macu:
1. Klíčenka (Keychain Access) → menu Klíčenka → Asistent certifikátů → **Vyžádat certifikát od certifikační autority…**
   – e-mail, jméno, „Uloženo na disk“ → vznikne soubor `.certSigningRequest`.
2. <https://developer.apple.com/account/resources/certificates> → **+** → **Developer ID Application** → nahrát soubor z bodu 1 → stáhnout `.cer` a dvojklikem ho přidat do Klíčenky.
3. V Klíčence (Moje certifikáty) pravé tlačítko na „Developer ID Application: …“ → **Exportovat** → formát `.p12`, zvolit heslo.
4. Převést na text pro GitHub: `base64 -i certifikat.p12 | pbcopy` (zkopíruje do schránky).

> Pozor: typ musí být **Developer ID Application** (ne „Apple Development“ ani „Mac App Distribution“).

### 3. Přístup pro notarizaci – doporučeno: API klíč App Store Connect
1. <https://appstoreconnect.apple.com/access/integrations/api> → Team Keys → **+** → role **Developer** → stáhnout `AuthKey_XXXXXXXXXX.p8` (jde stáhnout jen jednou).
2. Poznamenat **Key ID** (10 znaků) a **Issuer ID** (UUID nahoře na stránce).

Alternativa: Apple ID + **heslo pro aplikaci** (<https://account.apple.com> → Přihlášení a zabezpečení → Hesla pro aplikace) + **Team ID**
(<https://developer.apple.com/account> → Membership details).

### 4. Uložit do GitHubu
Repozitář **adminphmedia/ORCA-RadioAIR** → Settings → Secrets and variables → Actions → **New repository secret**:

| Secret | Hodnota |
|---|---|
| `APPLE_CERTIFICATE` | base64 text souboru `.p12` (krok 2.4) |
| `APPLE_CERTIFICATE_PASSWORD` | heslo k `.p12` |
| `APPLE_SIGNING_IDENTITY` | *(nepovinné)* např. `Developer ID Application: Pavel Novák (ABCDE12345)` – jinak se najde sám |
| `APPLE_API_KEY` | Key ID (krok 3) |
| `APPLE_API_ISSUER` | Issuer ID (krok 3) |
| `APPLE_API_KEY_P8` | celý obsah souboru `AuthKey_….p8` (včetně řádků BEGIN/END) |

nebo místo posledních tří: `APPLE_ID`, `APPLE_PASSWORD` (heslo pro aplikaci), `APPLE_TEAM_ID`.

Hodnoty secrets nikdo (ani sestavení) nevidí v čitelné podobě; do repozitáře se nic neukládá.

## Sestavení
GitHub → Actions → **Build installers** → Run workflow. V souhrnu běhu je u macOS uvedeno
„podepsáno (Developer ID …)“ a „Notarizace: api“. Notarizace u Apple obvykle trvá 2–15 minut.

Co sestavení dělá:
1. importuje certifikát do dočasné klíčenky,
2. podepíše přibalený `ffmpeg` (hardened runtime + časové razítko),
3. sestaví a podepíše aplikaci s oprávněním k zvukovému vstupu (`Entitlements.plist`) a popisem pro dotaz na mikrofon (`Info.plist`),
4. Tauri aplikaci notarizuje a „sešije“ (staple),
5. podepíše, notarizuje a sešije i instalační DMG a ověří ho (`spctl`, `stapler validate`).

## Ověření na Macu
```
spctl --assess --type open --context context:primary-signature -v "ORCA RadioAIR_0.6.0_universal.dmg"
# → accepted, source=Notarized Developer ID
```

## Časté chyby
- *„V certifikátu chybí identita Developer ID Application“* – exportován jiný typ certifikátu nebo `.p12` bez soukromého klíče (exportujte z „Moje certifikáty“, ne z „Certifikáty“).
- *Notarizace „Invalid“* – v protokolu (`xcrun notarytool log <id>`) je soubor, který není podepsaný; dejte vědět, doplní se do podpisu.
- Platnost certifikátu je 5 let; po obnovení stačí vyměnit `APPLE_CERTIFICATE` a heslo.
