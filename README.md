# ORCA RadioAIR

Odbavovací systém pro rádio pro **macOS a Windows**: automatické vysílání, knihovna ze sledovaných složek, clocky, crossfade, streamování do Icecastu, exporty now-playing a reporty vysílání.

Rozsah funkcí vychází z mapy `ORCA-funkce-vs-JazlerSOHO.md` (projekt ORCA RadioAIR).

## Co umí verze 0.1 (etapa 1)

| Oblast | Stav |
|---|---|
| **Knihovna bez importování.** Sledované složky se přidávají a aktualizují automaticky, kategorie se berou z podsložek, smazané soubory se označí jako chybějící. Každých 10 min proběhne kontrolní sken (kvůli NAS a síťovým diskům). | ✅ |
| Formáty MP3, AAC/M4A, FLAC, WAV, AIFF, OGG Vorbis, ALAC; tagy včetně ISRC, autora, vydavatele a obalu alba | ✅ |
| **Autocue:** začátek, bod mixu (relativní k hlasitosti skladby) a konec; prahy zvlášť pro každý typ (reklamy hrají celé) | ✅ |
| **Normalizace hlasitosti** podle LUFS (EBU R128), soubor se nemění | ✅ |
| **Playout engine:** přednačítání, crossfade od bodu mixu (kosinová křivka), převzorkování, soft-clip | ✅ |
| Výstup na zvukovku (CoreAudio/WASAPI); po výpadku karty stream pokračuje a karta se automaticky znovu připojí | ✅ |
| Režimy Auto / Manual, Play, Další, Pauza, Stop, Stop po skladbě, instant jingly přes hlavní stopu, ducking | ✅ |
| **Main Auto:** clocky, týdenní mřížka 7×24, separace interpreta a skladby, randomness, kompenzace, reshuffle | ✅ |
| **Důvod výběru** u každé položky (Jazler: „proč se to hrálo“) | ✅ |
| **Icecast encodery** (MP3, AAC, Ogg Vorbis, Opus) přes ffmpeg, automatické znovupřipojení, metadata skladby | ✅ |
| Exporty NowOnAir.txt/.xml/.json, AirPlayHistory, AirPlayNext, CurrentAlbumImage, webhooky; pravidla pro každý typ (NO-UPDATE / vlastní text) | ✅ |
| Log vysílání v SQLite, CSV report (UTF-8, středník, pro český Excel) | ✅ |
| Lokální REST + WebSocket API pro UI z Lovable a vzdálené panely | ✅ |
| Desktopová aplikace (Tauri 2): potvrzení před ukončením, vypnutý App Nap a uspávání na Macu | ✅ |
| Vestavěný ovládací panel CZ/EN, tmavý a světlý režim (dokud není UI z Lovable) | ✅ |

**Další etapy:** reklamní kampaně a bloky s prioritami, playlisty a import Prodigy/TXT, force clocks, Final Log, sweepery a voice tracky s pozicemi, Stitcher, RDS, rebroadcast, uživatelé a oprávnění, vizuální editor, šablony reportů pro OSA a Intergram.

## Jak to funguje se složkami

```
RADIO/
├── Songs/            ← sledovaná složka typu "song", kategorie z podsložek
│   ├── Hot Current/
│   ├── Pop/
│   └── Gold 80s/
├── Jingles/          ← typ "jingle", pevná kategorie nebo podsložky
├── Spots/            ← typ "spot" (hraje celé, bez mixu)
└── Shows/            ← typ "asset"
```

Soubor stačí do složky nakopírovat (ručně, skriptem nebo synchronizací z NAS). Během 1–2 s se objeví v knihovně, proběhne analýza a od té chvíle je k dispozici v rotaci. Přepsáním souboru se stejným názvem se aktualizuje i záznam v knihovně. Ruční úpravy cue bodů zůstanou zachované.

## Spuštění

### Desktopová aplikace (Mac mini / Windows)
Instalátory sestavuje GitHub Actions (`.github/workflows/release.yml`):
1. Nahraj repozitář na GitHub.
2. Spusť Actions → **Build installers**.
3. Stáhni `.dmg` (macOS Universal: Apple Silicon i Intel) nebo `.exe` / `.msi` (Windows).

Na macOS je potřeba ffmpeg: `brew install ffmpeg`. Ve Windows je ffmpeg součástí instalátoru.

### Bez okna (headless, server, vývoj UI)
```bash
cargo run --release -p orca-server -- --data-dir ~/ORCA-data
# UI a API: http://127.0.0.1:8750
```

### Lokální sestavení desktopové aplikace
```bash
cargo install tauri-cli --version "^2" --locked
cd apps/orca-desktop && cargo tauri build
```

## Konfigurace

Soubor `orca.toml` v datové složce:
- macOS: `~/Library/Application Support/ORCA RadioAIR/`
- Windows: `%APPDATA%\ORCA RadioAIR\`

Vše se dá měnit i přes API (`PUT /api/config`), tedy z UI.

```toml
[station]
name = "Moje Rádio"
language = "cs"      # cs | en
theme = "dark"       # dark | light | system

[[watch_folders]]
path = "/Users/pavel/RADIO/Songs"
kind = "song"
category_from_subfolder = true

[[watch_folders]]
path = "/Users/pavel/RADIO/Jingles"
kind = "jingle"
category = "ID"
category_from_subfolder = false

[audio]
output_device = ""   # "" = výchozí, "none" = bez zvukovky (jen stream)
normalize = true
target_lufs = -16.0

[auto]
min_queue = 6
artist_separation_min = 45
song_separation_min = 180
default_clock = "Hodina"

[[auto.clocks]]
name = "Hodina"
backup_category = "Pop"
steps = [
  { kind = "jingle", category = "ID" },
  { kind = "song", category = "Hot Current" },
  { kind = "song", category = "Gold 80s" },
  { kind = "song", category = "Pop" },
]

[auto.clock_grid]    # den-hodina (1 = Po … 7 = Ne) → název clocku
"6-20" = "Sobotní noc"

[[encoders]]
name = "Icecast MP3"
host = "127.0.0.1"
port = 8000
mount = "/live"
password = "…"
format = "mp3"       # mp3 | aac | ogg | opus
bitrate_kbps = 128
```

## API (výběr)

| Metoda | Cesta | Popis |
|---|---|---|
| GET | `/api/status` | stav enginu, aktuální skladba, metry, encodery |
| WS | `/api/ws` | události v reálném čase + stav 5× za sekundu |
| POST | `/api/transport/{play\|next\|pause\|stop\|stop-after-current}` | ovládání |
| POST | `/api/mode` `{"mode":"auto"\|"manual"}` | režim |
| GET/POST/DELETE | `/api/queue`, `/api/queue/{id}`, `/api/queue/move` | decky |
| GET/PATCH | `/api/tracks`, `/api/tracks/{id}` | knihovna, úpravy, cue body |
| POST | `/api/instant/{id}` | instant jingle |
| GET | `/api/log`, `/api/reports/broadcast.csv` | log a report |
| GET/PUT | `/api/config`, `/api/settings/ui` | nastavení, jazyk, motiv |

## Struktura

```
crates/orca-core     engine, knihovna, DB, automatika, encodery (Rust)
crates/orca-server   HTTP/WS API + vestavěný panel, headless spouštění
apps/orca-desktop    desktopová aplikace (Tauri 2) pro macOS a Windows
ui-dist/             sem patří build UI z Lovable
```

## Testy
```bash
cargo test
```
Testy ověřují autocue a crossfade (obě skladby znějí současně, bez děr v signálu, včetně převzorkování). End-to-end test proběhl proti Icecastu 2.4: MP3 a AAC streamy, metadata, automatický import ze složek a exporty.
