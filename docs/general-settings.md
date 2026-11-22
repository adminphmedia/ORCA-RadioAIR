# Obecná nastavení, uživatelé a priorita vysílání

Odpovídá „General Settings“ v Jazler SOHO. Kód: `crates/orca-core/src/general.rs`, `users.rs`, `monitor.rs`,
API `crates/orca-server/src/general_api.rs`, UI `src/features/control/GeneralSettings.tsx`.

| Funkce | Kde | Poznámka |
|---|---|---|
| Spouštění se systémem | Spuštění a zkratky | tauri-plugin-autostart (LaunchAgent / registry), synchronizuje se při uložení konfigurace |
| Priorita vysílání | Spuštění a zkratky | vlákna mixéru a zvukového výstupu s nejvyšší prioritou; Windows `HIGH_PRIORITY_CLASS` + `SetThreadExecutionState`; macOS `caffeinate -i -s -m` + vypnutý App Nap |
| Klávesové zkratky | Spuštění a zkratky | akce → klávesa (`general.hotkeys`) |
| Předposlech (PFL) | knihovna (ikona sluchátek) | samostatné zařízení `audio.monitor_device`, `/api/monitor/*` |
| Externí editor | `general.editor_path` | `/api/general/open-editor/{id}` |
| Uživatelé a role | Uživatelé a oprávnění | admin / editor / presenter / viewer + doplňková oprávnění; PIN (SHA-256 se solí jména); relace v paměti, automatické odhlášení |
| Kontrola oprávnění | server middleware | zápisové požadavky podle cesty → oprávnění (`users::required_perm`); s `require_login` bez relace → 401 `{login:true}`; čtení povoleno |
| Hlídání ticha | Hlídání a upozornění | práh / doba / akce: upozornit, další, záchranný playlist, záložní zdroj (rebroadcast) |
| E-mailová upozornění | Hlídání a upozornění | SMTP (lettre, rustls), STARTTLS / TLS / bez šifrování, deduplikace stejného hlášení |
| Vlastní logy | Vlastní logy | šablona řádku s klíčovými slovy, denní soubor, výběr typů knihovny |
| Zálohy | Zálohy a údržba | `VACUUM INTO` + kopie `orca.toml`, denně v nastavenou hodinu, ponechat N |
| Integrita | Zálohy a údržba | kontrola existence souborů → označit / vypnout / odebrat, TXT report v `Reports/` |
| Údržba DB | Zálohy a údržba | mazání starého logu, VACUUM, reset vybraných knihoven (po automatické záloze) |
| Mapování složek | Zálohy a údržba | překlad cest při přehrávání (prefix, bez ohledu na velikost písmen a lomítka) + trvalé přepsání cest v DB |

Uživatelský manuál s vyhledáváním a offline asistentem je v UI (Ovládací panel → Uživatelský manuál,
`src/features/manual/`). Při každé změně funkcí se aktualizuje (pravidlo v AGENTS.md UI repozitáře).

## Ověřeno (Linux, release build)
- přihlášení PINem, chybný PIN → 401, moderátor nesmí do nastavení (403), smí ovládat vysílání; poslední admin nejde degradovat;
- `require_login`: zápis bez relace → 401, čtení 200, admin nastavení vrátí;
- záloha (rotace na N), kontrola integrity (99 souborů), VACUUM;
- vlastní log OSA (hlavička + řádek při každé skladbě);
- hlídání ticha (umělý práh 0 dB → upozornění po 4 s);
- zkušební e-mail přes lokální SMTP server (UTF-8 předmět i tělo);
- vlákno mixéru dostalo nejvyšší prioritu.

Neověřeno na reálném HW: autostart a priorita procesu na macOS/Windows (běží v CI buildu, vyzkoušet po instalaci), předposlech na fyzické zvukové kartě.
