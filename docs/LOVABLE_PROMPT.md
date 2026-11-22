Build the complete user interface for **ORCA RadioAIR**, a professional radio playout/automation desktop application (macOS + Windows). The workflow should feel familiar to operators of classic radio automation systems (Studio screen with decks, libraries, clocks, commercial breaks), but the visual design must be original and modern. Do not use any other product's name, logo or branding.

## 1. Technical requirements (important)
- React + TypeScript + Vite + Tailwind + shadcn/ui, lucide-react icons, TanStack Query, Zustand.
- **No Supabase, no auth, no backend of your own.** All data comes from a local engine over HTTP + WebSocket (contract in section 7).
- `vite.config.ts`: `base: './'` (the build is served from a local folder by the engine and inside a desktop webview).
- API base URL: `localStorage.orcaApi` → if the page is served from port 8750, use same origin (`''`) → otherwise `http://127.0.0.1:8750`. Editable in Settings → Connection.
- **Mock mode:** if the API is unreachable (e.g. in the Lovable preview), switch automatically to realistic demo data with a simulated clock, progress, VU meters and queue, so the whole UI is usable and testable. Show a small "DEMO" badge. Keep a clean `api/` layer with `real` and `mock` implementations of the same interface.
- WebSocket `ws(s)://<base>/api/ws`: auto-reconnect with backoff, connection indicator in the top bar.
- **i18n:** react-i18next with complete `cs` (default) and `en` dictionaries. Every visible string comes from the dictionary. Language switch CZ/EN in the top bar; persist it in localStorage and send `PUT /api/settings/ui {language}`. Format dates and times with the active locale (24 h for cs).
- **Theme:** dark (default), light and system. Toggle in the top bar; persist it and send `PUT /api/settings/ui {theme}`. Implement via CSS variables / Tailwind `dark` class. Both themes must have broadcast-grade contrast.
- Desktop-first: optimal at 1440×900, minimum 1100×700, no mobile layout needed. No page scroll on the Studio screen; panels scroll internally.
- Performance: virtualize long lists (library can have 50,000+ tracks), throttle VU rendering with requestAnimationFrame, no layout shift when values change.

## 2. Visual language
- Professional broadcast studio look: calm dark graphite surfaces, one accent color (cyan/teal), red reserved **only** for ON AIR, live and critical states, amber for warnings.
- All times, durations and countdowns use `font-variant-numeric: tabular-nums` and a monospace display font for big numbers.
- Color code per item type, used consistently as a left border or chip on decks, tables and buttons: song = blue, jingle = violet, spot (commercial) = amber, sweeper = teal, voice track = pink, asset = slate, instant = lime.
- Large and obvious hit targets for transport controls (operators click under stress). Minimal, fast animations (≤150 ms).
- Clear empty states and error states, toasts for alerts, a confirmation dialog for destructive actions (Stop, Clear decks, Delete).

## 3. App shell
**Top control bar** (always visible):
- Logo and wordmark "ORCA RadioAIR", station name
- Mode segmented control **AUTO / MANUAL** (AUTO highlighted when active)
- Engine state pill (playing / paused / stopped)
- Big digital clock HH:MM:SS (server time from status)
- Connection indicator (API/WS), stream status dots per encoder (green live / amber connecting / red error, tooltip with message)
- Layout switcher, language CZ/EN, theme toggle, **Control Panel** button (opens settings and management)

Two main areas: **Studio** (broadcast screen) and **Control Panel** (management), switchable without losing state.

## 4. Studio screen
**A) On-air player (top strip):**
- Cover art (`GET /api/tracks/{id}/cover`, fallback type icon), big title, artist, category chip, type chip
- Progress bar showing intro, mix (segue) and end markers; elapsed / **remaining** (huge countdown; turns amber under 20 s and red under 10 s)
- "Next" preview (next deck item + ETA)
- Transport buttons: **Play, Next, Pause, Stop, Stop after current** (toggle), all with keyboard shortcuts (F1 Play, F2 Next, F3 Pause, F4 Stop, F5 Stop after current), shown as hints
- Stereo VU/peak meters L/R (values 0–1 linear, display in dBFS, -60…0 dB scale, green/amber/red zones, peak hold)

**B) Decks (left column)** = upcoming queue:
- Rows: ETA time, type color bar, artist – title, duration (cue_out − cue_in), source badge (auto/manual), tooltip with **reason** (why it was selected)
- Drag & drop reorder (`POST /api/queue/move`), remove (×), context menu: play now, move to top, remove, track info
- Drop target: dragging a track from the library inserts it at that position
- Compact and detailed view toggle; total time until end of decks

**C) Center panels** (tabbed or split, user can show/hide):
1. **History & next events:** last played items from `/api/log` (time, title, played duration) and upcoming items
2. **Libraries browser:** type tabs (Songs, Jingles, Spots, Sweepers, Voice tracks, Assets), category filter, search (debounced), virtualized table (artist, title, category, duration, last played, play count), drag into decks, double click = add to end, buttons: Add, Insert next, Play now
3. **Instant jingles (cartwall):** grid of pads (e.g. 4×6) with pages; each pad has track, label and color; click = `POST /api/instant/{id}`, a playing pad shows progress; "Stop all instants" button. Pad layout stored in localStorage for now.
4. **Alerts / log:** live stream of `alert` events (level, time, translated message by `code`, raw message as detail)
5. **Clock:** analog hour clock showing the current hour's clock pattern as colored segments and the current position
6. **Track info:** details of the selected item (all metadata, cue points, LUFS, reason)

**Layouts:** 4 presets (Classic: decks left + panels; Wide panels; Simple: player + decks only; Max info: 3 panels). Remember per user in localStorage.

## 5. Control Panel (management)
Left navigation with sections:
1. **Library:** full track table with filters (type, category, search, include disabled/missing), bulk select and bulk edit (category, enabled), track edit dialog: metadata, ISRC, composer, publisher, playlister code, categories 1–3, gain dB, enabled. **Cue editor:** waveform area (placeholder bar for now) with draggable markers cue in / intro / mix / cue out / hook in / hook out, numeric ms inputs, "Re-analyze" button (`POST /api/tracks/{id}/reanalyze`). Badges for missing / not analyzed / disabled. Library stats (`/api/library/stats`) and "Rescan folders" button.
2. **Watch folders:** list and edit `config.watch_folders` (path text input, type select, fixed category, "category from subfolder" switch, enabled). Short explanation: "Files dropped into these folders are added automatically – no import needed."
3. **Clocks:** list of clocks; clock editor with ordered steps (type + category from `/api/categories`), drag reorder, color, backup category; preview as an hour-circle. **Weekly schedule:** grid 7 days × 24 hours; select cells by click-drag and assign a clock (keys `"<day>-<hour>"`, day 1 = Monday). Default clock selector.
4. **Song selection rules:** artist separation (min), song separation (min), selection randomness (0 = strict rotation), decks to keep filled (min_queue), start in Auto, "Reshuffle all media" button.
5. **Audio:** output device select (`/api/audio/devices` + "None – stream only"), buffer ms, fade on segue, manual fade ms, stop fade ms, loudness normalization switch + target LUFS, ducking dB. Autocue thresholds per type (start dB, end dB, mix dB relative, "play to end" for no mix).
6. **Streaming encoders:** cards with live status; form: name, host, port, mount, username, password, format (MP3/AAC/Ogg/Opus), bitrate, sample rate, channels, stream name, genre, description, URL, public, send metadata, autostart, reconnect seconds; Start/Stop buttons; listen URL with copy button.
7. **Now playing / Internet:** enable exports, export folder, per-type rule (empty = metadata, `NO-UPDATE`, or custom text), history size, next size, export cover, webhook URLs.
8. **Log viewer & reports:** date range picker, type filter, table of the broadcast log with the **reason** column; "Download CSV report" (`/api/reports/broadcast.csv?from&to&kind&lang`).
9. **General:** station name, language, theme, time offset (s), API connection URL and token, about (version, platform, data folder).

Config editing pattern: load `GET /api/config`, edit with a typed form, save with `PUT /api/config` (whole object), show a dirty state and a Save/Revert bar.

## 6. Translations
Provide the complete cs and en dictionaries, including alert codes: `watch_folder_unavailable, watch_failed, analysis_failed, output_unavailable, output_failed, output_rate_mismatch, playback_failed, encoder_disconnected, auto_no_tracks`. Use correct broadcast terminology in Czech: Ve vysílání, Další, Stop po skladbě, Decky, Knihovna, Reklamy, Jingly, Sweepery, Voice tracky, Hodiny (clocky), Týdenní rozvrh, Rozestup interpreta, Rozestup skladby, Náhodnost výběru, Streamy, Sledované složky, Důvod výběru, Odvysíláno.

## 7. API contract (engine at http://127.0.0.1:8750)
```ts
type TrackKind = 'song'|'jingle'|'spot'|'sweeper'|'voicetrack'|'asset';
interface Track { id:number; kind:TrackKind; path:string; filename:string; artist:string; title:string; album:string; year:number|null; genre:string;
  category:string; category2:string; category3:string; duration_ms:number; cue_in_ms:number; intro_ms:number; mix_ms:number; cue_out_ms:number;
  hook_in_ms:number|null; hook_out_ms:number|null; gain_db:number; lufs:number|null; analyzed:boolean; manual_cues:boolean; enabled:boolean; missing:boolean;
  playlister_code:string|null; isrc:string; composer:string; publisher:string; added_at:string; updated_at:string; last_played_at:string|null; play_count:number }
interface QueueItem { id:string; track:Track; source:'auto'|'manual'|'instant'|'playlist'; reason:string; eta:string|null }
interface VoiceStatus { item:QueueItem; channel:'main'|'overlay'; position_ms:number; length_ms:number; remaining_ms:number; mix_at_ms:number; fading:boolean }
interface EngineStatus { state:'playing'|'paused'|'stopped'; stop_after_current:boolean; current:VoiceStatus|null; voices:VoiceStatus[];
  output:{ device:string; ok:boolean; sample_rate:number; underruns:number; message:string } }
interface EncoderStatus { id:string; name:string; state:'stopped'|'connecting'|'live'|'error'; message:string; since:string|null; bytes_in:number; url:string }
interface Status { type:'status'; time:string; mode:'auto'|'manual'; engine:EngineStatus; meters:{l:number;r:number}; encoders:EncoderStatus[]; queue_length:number }
interface LogEntry { id:number; track_id:number|null; kind:string; artist:string; title:string; album:string; path:string; category:string; isrc:string;
  composer:string; publisher:string; duration_ms:number; started_at:string; ended_at:string|null; played_ms:number|null; source:string; reason:string }
// Note: track positions (position_ms, remaining_ms) are relative to cue_in. Timestamps in the log are UTC ISO strings.
```
Endpoints:
- `GET /api/info` → `{app, version, station, language, theme, platform, arch, data_dir, sample_rate}`
- `GET /api/status` → `Status`
- `POST /api/transport/{play|next|pause|stop|stop-after-current|cancel-stop-after-current}`
- `POST /api/mode` body `{mode:'auto'|'manual'}`
- `GET /api/queue` → `QueueItem[]` (with eta) · `POST /api/queue` `{track_id, position?}` · `DELETE /api/queue/{id}` · `DELETE /api/queue` (clear) · `POST /api/queue/move` `{id, to}`
- `GET /api/tracks?kind&category&q&limit&offset&sort(artist|title|added|played|duration)&include_disabled&include_missing` → `{items: Track[], total}`
- `GET /api/tracks/{id}` · `PATCH /api/tracks/{id}` (any subset of editable fields; setting cue fields marks manual cues) · `GET /api/tracks/{id}/cover` (image) · `POST /api/tracks/{id}/reanalyze` `{reset_manual?}` · `POST /api/tracks/{id}/play-now`
- `POST /api/instant/{trackId}` · `POST /api/instant/stop`
- `GET /api/categories` → `{kind, category, count}[]`
- `POST /api/library/rescan` · `GET /api/library/stats` → `{tracks, missing, analysis_pending}` · `POST /api/auto/reshuffle`
- `GET /api/log?from&to&kind&limit` → `LogEntry[]` (newest first) · `GET /api/reports/broadcast.csv?from&to&kind&lang`
- `GET /api/config` / `PUT /api/config` (full Config object; sections: station, watch_folders[], audio, autocue.per_kind, auto{start_in_auto,min_queue,artist_separation_min,song_separation_min,randomness,clocks[{name,color,steps[{kind,category}],backup_category}],clock_grid{"d-h":clockName},default_clock}, encoders[], nowplaying, api, ffmpeg_path)
- `PUT /api/settings/ui` `{language?, theme?}`
- `GET /api/audio/devices` → `{name, is_default, channels, default_rate}[]`
- `GET /api/encoders` → `EncoderStatus[]` · `POST /api/encoders/{id}/start|stop`
- Errors: HTTP 4xx/5xx with `{error:string}` → show a toast.
- Optional auth: if a token is configured, send `Authorization: Bearer <token>` (and `?token=` for the WebSocket).

WebSocket messages (JSON, field `type`):
`hello {info}` · `status` (5×/s, same as GET /api/status) · `track_started {item, channel}` · `track_ended {item_id, played_ms}` · `queue_changed` (refetch queue) · `mode_changed {mode}` · `library_changed {added, updated, missing}` (invalidate library queries) · `analysis_progress {pending}` · `encoder_status {id, state, message}` · `alert {level:'info'|'warning'|'error', code, message}` · `config_changed` (refetch config).

## 8. Deliverable quality
Organize the code by feature (`features/studio`, `features/library`, `features/clocks`, `features/encoders`, …), keep strict TypeScript types matching the contract, and make the mock data realistic: Czech and international artists, categories such as Hot Current, Pop, Gold 80s, Rock, jingles "Station ID", spots "Reklama – Autoservis Novák", and two encoders (MP3 128k live, AAC 64k connecting).
