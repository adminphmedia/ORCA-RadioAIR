# ORCA RadioAIR – vysílací zvukový processing

Stav: implementováno v enginu (crates/orca-core/src/dsp, processing.rs), API (crates/orca-server/src/proc_api.rs)
a UI (Ovládací panel → Vysílání a metadata → Zvukový processing). Tovární presety: `assets/processing/factory_presets_v1.json` (schéma 1, verze 1).

## 1. Napojení na skutečnou audio cestu

```
soubory / rebroadcast / instanty
   │  zisk zdroje: normalizace LUFS souboru (±12 dB, soubor se nemění) + trim třídy zdroje (audio.kind_trim_db)
   ▼
mixer enginu (48 kHz, float, bloky 10 ms)  ── kontinuální PCM tok drží vlákno orca-mixer (běží i při změně skladeb,
   │                                           reconnectu streamu a změně metadat)
   ▼
MASTER (jednou pro všechny výstupy): vstupní trim → HPF/DC → AGC → EQ → 4pásmová dynamika (LR4 + allpass) → de-esser → šířka
   │   přepnutí nastavení: nová instance, převzetí stavu AGC/kompresorů, 50ms prolnutí, potvrzení generace z mixeru
   ├──► větev „Zvuková karta“ (v mixeru): trim → true-peak limiter → výstup zvukové karty
   └──► tap každého enkodéru (vlákno streamu): převzorkování na frekvenci enkodéru (rubato FFT) → trim → true-peak limiter
             → ffmpeg (stejná frekvence, žádné další převzorkování) → Icecast
```

Proč tady: celý program (hudba, reklamy, jingly, voice tracky, rebroadcast) se míchá v enginu ORCA a odtud jde
jak do zvukové karty, tak do enkodérů – je to jediné místo, kde existuje finální programový mix před kódováním.
Mikrofon do ORCA nevstupuje (je na externím pultu); talk-over pouze ztlumí hudbu.

Výchozí stav po migraci: processing **vypnutý** (revize „Transparent / vypnuto“) – signál jde do streamů beze změny
(původní soft-clip). Zapnutí je výslovná akce operátora („Nasadit do vysílání“).

Latence: master 0 ms (IIR), větev 48 kHz 5,65 ms, větev 44,1 kHz 9,02 ms (převzorkování + lookahead). Latence je pevná,
nemění se při změně presetu.

## 2. Tovární presety (schéma 1)

Poměry a prahy jsou výchozí kalibrace na syntetickém referenčním materiálu (viz testy), ne matematický předpis.
Prahy pásem jsou pro RMS detektor pásma (dBFS, +3 dB korekce na sinus). Intenzita 50 = hodnoty v tabulce.

| Preset | Zásah | HPF | AGC | EQ | Pásma (poměr @ práh dBFS) | De-esser | Inspirace formátem |
|---|---|---|---|---|---|---|---|
| **ENERGIC CHR** | silná | HPF 30 Hz | cíl -17 dBFS, +8/−10 dB, 4/1.5 dB/s, gate -45 | LS +1.0 dB @100 Hz, HS +0.5 dB | 2.0:1 @-30 / 2.3:1 @-31 / 2.3:1 @-33 / 2.0:1 @-37 | ano | Z100 New York (formát Top 40) |
| **ENERGIC Dance** | silná | HPF 30 Hz | cíl -17 dBFS, +8/−10 dB, 4/1.5 dB/s, gate -45 | LS +1.5 dB @75 Hz, HS +0.5 dB | 1.8:1 @-29 / 2.0:1 @-31 / 2.0:1 @-33 / 2.0:1 @-37 | ano | Taneční a klubové stanice |
| **Hot AC** | střední | HPF 30 Hz | cíl -18 dBFS, +6/−8 dB, 3/1 dB/s, gate -45 | LS +0.5 dB @100 Hz, HS +0 dB | 1.7:1 @-29 / 1.8:1 @-30 / 1.8:1 @-32 / 1.7:1 @-36 | ano | Hot AC formát |
| **Soft AC** | mírná | HPF 30 Hz | cíl -19 dBFS, +5/−8 dB, 2/0.8 dB/s, gate -45 | LS +0 dB @100 Hz, HS -0.5 dB | 1.5:1 @-28 / 1.5:1 @-29 / 1.5:1 @-31 / 1.5:1 @-35 | ano | 106.7 Lite FM (Soft AC) |
| **Classic Rock** | střední | HPF 30 Hz | cíl -18 dBFS, +6/−8 dB, 3/1 dB/s, gate -45 | LS +0.5 dB @100 Hz, HS +0.3 dB | 1.8:1 @-29 / 2.0:1 @-30 / 2.1:1 @-32 / 1.8:1 @-36 | ne | Q104.3 (Classic Rock) |
| **Hip Hop & R&B** | střední | HPF 30 Hz | cíl -17 dBFS, +7/−9 dB, 3/1 dB/s, gate -45 | LS +1.0 dB @80 Hz, HS +0 dB | 1.6:1 @-28 / 2.0:1 @-30 / 2.0:1 @-32 / 1.8:1 @-36 | ano | Power 105.1 (Hip Hop & R&B) |
| **Gold & Oldies** | mírná | HPF 40 Hz | cíl -19 dBFS, +4/−8 dB, 2/0.6 dB/s, gate -40 | LS +0 dB @100 Hz, HS +0 dB | 1.6:1 @-28 / 1.6:1 @-29 / 1.6:1 @-31 / 1.5:1 @-35 | ne | Stanice s hudbou z různých dekád |
| **News & Talk** | silná | HPF 70 Hz | cíl -17 dBFS, +8/−10 dB, 6/2 dB/s, gate -40 | LS +0 dB @100 Hz, HS +0 dB | 2.0:1 @-30 / 2.5:1 @-32 / 2.5:1 @-34 / 2.0:1 @-38 | ano | 710 WOR, BBC Radio 4 |
| **Live Sport** | střední | HPF 50 Hz | cíl -18 dBFS, +4/−10 dB, 4/0.5 dB/s, gate -38 | LS +0 dB @100 Hz, HS +0 dB | 1.8:1 @-29 / 2.0:1 @-31 / 2.0:1 @-33 / 1.8:1 @-37 | ano | Sportovní přenosy |
| **Jazz & Acoustic** | mírná | HPF 30 Hz | cíl -19 dBFS, +4/−6 dB, 1.5/0.5 dB/s, gate -45 | LS +0 dB @100 Hz, HS +0 dB | 1.4:1 @-27 / 1.4:1 @-28 / 1.4:1 @-30 / 1.3:1 @-34 | ne | Jazzové a akustické stanice |
| **Classical & Culture** | mírná | HPF 20 Hz | vypnuto | LS +0 dB @100 Hz, HS +0 dB | 1.2:1 @-22 / 1.25:1 @-22 / 1.25:1 @-24 / 1.2:1 @-28 | ne | BBC Radio 3 (klasický program) |
| **Transparent** | ochrana | HPF 15 Hz | vypnuto | LS +0 dB @100 Hz, HS +0 dB | vypnuto | ne | Čistá distribuce |

Mapování **Intenzity** (0–100, deterministické, `dsp/params.rs::effective`): k = (I − 50)/50;
AGC max. zesílení × (1 + 0,5k); poměr pásma 1 + (r − 1)(1 + 0,5k) (max 6:1); práh − 4k dB; max. redukce × (1 + 0,3k).
Nemění true-peak strop ani výstupní cíle. U Transparent je skrytá. Basy = low shelf 100 Hz ±6 dB, Jas = high shelf 8 kHz ±6 dB, Vstupní zisk ±12 dB.

**Transparent** ≠ servisní bypass: Transparent nechává aktivní ochranu výstupních větví (true-peak limiter),
servisní hard bypass vypíná i ji (vyžaduje potvrzení, je výrazně zobrazený a zapisuje se do auditu).

## 3. Výstupní šablony

| Šablona | True-peak strop | Ref. hlasitost (jen měření) | Převzorkování |
|---|---|---|---|
| Icecast / internet | −2 dBTP | −18 LUFS | na frekvenci enkodéru |
| DAB+ | −2 dBTP | −18 LUFS | 48 kHz |
| PCM distribuce / DVB-T2 | −1 dBTP | −18 LUFS | beze změny |
| PCM −23 LUFS (odběratel) | −1 dBTP | −23 LUFS | beze změny |

Šablony se nabízejí jen pro existující výstupy (zvuková karta, každý nakonfigurovaný enkodér). DAB+ v ORCA vzniká
přes stream do ODR-AudioEnc – větev DAB+ se proto přiřadí enkodéru, který DAB napájí. Žádná FM preemfáze ani MPX.
Žádný rychlý regulátor k cílové hlasitosti – referenční LUFS slouží jen ke kalibraci (trim větve).

## 4. Výsledky testů (provedeno 22. 11. 2026, Linux x86_64, release build)

Syntetický materiál: `cargo test --release -p orca-core dsp:: -- --nocapture`

* ENERGIC CHR – hlasitost segmentů (LUFS, po 2 s náběhu): CHR skladba −10,9 → −19,6; tichá starší −27,0 → −22,7;
  hlasitý jingle −5,0 → −15,3; mluvené slovo −20,7 → −22,8; ticho zůstává ticho (gate); taneční −12,0 → −20,1;
  velmi nízká úroveň −45,1 → −44,2 (gate nezesiluje); přebuzený vstup −1,1 → −16,0.
  **Rozptyl mezi zdroji 22,0 LU → 7,5 LU.** Jingle zůstává o ~4 LU nad hudbou – hrubé vyrovnání dělá normalizace souborů.
* Změna presetu mění PCM: součet rozdílů vůči Transparent ENERGIC CHR 49,4 LU, ENERGIC Dance 48,1 LU; Transparent zachová hlasitost (±0,3 LU).
* True-peak limiter, syntetický ISP test (sinus fs/4, fáze 45°, +3,11 dBTP): strop −2 → −2,03 dBTP (48 kHz), −2,14 dBTP (44,1 kHz);
  strop −1 → −1,03 dBTP; referenční 8× převzorkování −2,03 dBTP. Program přes ENERGIC CHR → 44,1 kHz: −2,11 dBTP.
* Zátěž: master + 2 větve, 30 s audia za 0,63 s = 47× rychleji než reálný čas (≈ 2,1 % jednoho jádra).
* Rozdělení pásem při neutrálním nastavení: odchylka < 0,05 dB (50 Hz – 10 kHz).

Živá cesta (engine + Icecast 2.4 + ffmpeg, MP3 128 kb/s, dekódováno ffmpeg ebur128):

| Nastavení | PCM před enkodérem TP max | Dekódovaný stream TP | Dekódovaný I |
|---|---|---|---|
| bez processingu (normalizace vypnutá, test ISP + hlasitý šum) | — | **+2,7 dBTP** | 0,5 LUFS |
| ENERGIC CHR | −2,00 dBTP | −3,3 dBTP | −13,8 LUFS |
| Transparent (jen ochrana) | −2,04 dBTP | −2,1 dBTP | −4,0 LUFS |

* Nasazení: 12–52 ms do potvrzení z mixeru, 0 underrunů, 0 chyb DSP, CPU bloku typicky 150–220 µs z 10 ms.
* Rollback, souběžná editace (HTTP 409 při staré základní revizi), chybná konfigurace (schéma 99 → odmítnuto, běží dál poslední funkční) – ověřeno.

**Neprovedeno (vyžaduje zařízení / poslech):** měření skutečného DAB+ enkodéru (ODR-AudioEnc → dekódování z multiplexu),
AAC/HE-AAC streamu, dlouhodobý (24h) zátěžový běh na Mac mini, poslechové doladění prahů na reálném programu stanice.

## 5. Stručný návod – odbavení ENERGIC

1. Ovládací panel → Zvukový processing. Zkontrolujte upozornění na externí procesor.
2. Vyberte **ENERGIC CHR** (nebo Dance pro taneční bloky), Intenzita 50, Basy/Jas 0.
3. **Porovnat A/B** na posledních 20 s vysílání nebo na skladbě z knihovny (hlasitost vyrovnaná, poslech na tomto počítači).
4. Ve **Výstupních větvích** nechte Icecast −2 dBTP; po nasazení sledujte „Naměřeno (PCM)“ – integrovaná hlasitost by měla být kolem −18 LUFS
   na reprezentativním bloku. Odchylku dorovnejte **trimem větve**, ne intenzitou.
5. **Nasadit do vysílání** (prolnutí 50 ms). Kdykoli **Vrátit předchozí nastavení**.
6. Pro taneční blok v Programové mřížce nastavte u pořadu „Zvukový preset bloku = ENERGIC Dance“; po bloku se vrací hlavní preset.
