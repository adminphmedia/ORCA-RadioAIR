//! Katalog formátů (presetů) a jejich převod na konfiguraci stanice.
//! Hodnoty jsou NÁVRHOVÉ VÝCHOZÍ BODY ORCA RadioAIR – ne normy ani interní nastavení konkrétních rádií.
//! Kalibrujte je proti své knihovně (kapacita, simulace týdne).

use super::{Daypart, Goal, Rules, SelCategory, SelClock, SelectorConfig, Slot};
use crate::model::TrackKind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetRole { pub id: String, pub name: String, pub share_pct: f64, pub rotation: String, pub pass: u32, pub hint: String }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub family: String,
    pub version: u32,
    pub description: String,
    pub music_per_hour: u32,
    pub roles: Vec<PresetRole>,
    pub rules: Rules,
    pub goals: Vec<Goal>,
    pub notes: Vec<String>,
    /// Podíly v minutách místo počtu (klasika).
    pub minutes: bool,
    /// Cíle, které musí doplnit stanice (např. podíl CZ) – průvodce se zeptá.
    pub ask: Vec<String>,
    /// Volitelné programové pozice (NEWS ZONE …)
    pub optional_slots: Vec<String>,
}

fn role(id: &str, name: &str, share: f64, rotation: &str, pass: u32, hint: &str) -> PresetRole {
    PresetRole { id: id.into(), name: name.into(), share_pct: share, rotation: rotation.into(), pass, hint: hint.into() }
}

fn rules(song_h: u32, artist_min: u32, same_hour_days: u32) -> Rules {
    Rules { song_sep_min: song_h * 60, song_target_min: song_h * 60 + 60, artist_sep_min: artist_min, artist_target_min: artist_min + 30, work_sep_min: song_h * 60, same_hour_days, ..Rules::default() }
}

fn goal(attr: &str, value: &str, target: f64, tol: f64, daypart: &str, sev: &str) -> Goal {
    Goal { id: format!("{attr}-{value}-{daypart}"), attr: attr.into(), value: value.into(), target_pct: target, tolerance_pct: tol, unit: "count".into(), scope: "day".into(), severity: sev.into(), daypart: daypart.into(), weight: 1.0 }
}

fn p(id: &str, name: &str, family: &str, desc: &str, mph: u32, roles: Vec<PresetRole>, rules: Rules, goals: Vec<Goal>, notes: &[&str]) -> Preset {
    Preset { id: id.into(), name: name.into(), family: family.into(), version: 1, description: desc.into(), music_per_hour: mph, roles, rules, goals, notes: notes.iter().map(|s| s.to_string()).collect(), minutes: false, ask: vec![], optional_slots: vec![] }
}

fn cur_rec_gold(a: f64, b: f64, c: f64) -> Vec<PresetRole> {
    vec![role("current", "Aktuální", a, "fast", 1, "nové a stoupající skladby"), role("recurrent", "Rekurenty", b, "normal", 2, "nedávné hity 1–3 roky"), role("gold", "Katalog (gold)", c, "slow", 3, "starší osvědčené hity")]
}

pub fn catalog() -> Vec<Preset> {
    let mut v = vec![];
    let mut chr = p("chr", "CHR / Top 40", "Hitová rádia", "Současné hity s rychlou rotací. Kontrolní model z 20 hudebních pozic: 3 power, 6 dalších aktuálních, 6 rekurentů, 5 katalogových – v reálné hodině se přepočte podle stopáží, reklamy a vstupů.", 12,
        vec![role("power", "Power current", 15.0, "fast", 1, "největší aktuální hity"), role("current", "Aktuální", 30.0, "fast", 2, "ostatní aktuální"), role("recurrent", "Rekurenty", 30.0, "normal", 3, "nedávné hity"), role("gold", "Katalog", 25.0, "slow", 4, "starší hity")],
        Rules { same_hour_hard: false, max_in_row: BTreeMap::from([("gender".into(), 3)]), ..rules(2, 60, 3) }, vec![], &["Přísně hlídá hlasy, opakování stejné hodiny a featuringy (host = interpret).", "Power current se vrací zhruba každé 2–3 hodiny – ověřte simulací."]);
    chr.rules.song_sep_min = 100; chr.rules.song_target_min = 150;
    v.push(chr);
    let mut en = p("chr_energic_cz", "CHR – ENERGIC CZ", "Hitová rádia", "Hitové rádio s českou a světovou rotací A–D, akustickými verzemi, Milenium a Energic. CZ podíl se nehádá – zadáte ho v průvodci.", 12,
        vec![role("world_a", "World A", 17.0, "fast", 1, "největší světové hity"), role("cz_a", "CZ A", 8.0, "fast", 1, "největší české hity"), role("world_b", "World B", 15.0, "normal", 2, ""), role("cz_b", "CZ B", 8.0, "normal", 2, ""),
             role("world_c", "World C", 12.0, "normal", 3, ""), role("cz_c", "CZ C", 6.0, "normal", 3, ""), role("world_d", "World D", 8.0, "slow", 4, ""), role("cz_d", "CZ D", 4.0, "slow", 4, ""),
             role("acoustic", "Acoustic", 5.0, "slow", 5, "akustické verze – sdílí ochranu s originálem"), role("milenium", "Milenium", 9.0, "slow", 5, "hity 2000–2015"), role("energic", "Energic", 8.0, "normal", 5, "energické tracky")],
        Rules { protect_versions: true, max_in_row: BTreeMap::from([("language".into(), 3)]), ..rules(2, 60, 3) }, vec![], &["Dvě až tři A pozice za hodinu podle části dne (víc ráno a odpoledne).", "Christmas je sezónní kategorie – nasazení přes datovanou výjimku.", "NEWS ZONE, Hack Your Life a TRAVEL MODE jsou volitelné programové pozice."]);
    en.ask = vec!["local".into()];
    en.optional_slots = vec!["NEWS ZONE".into(), "Hack Your Life".into(), "TRAVEL MODE".into()];
    en.rules.song_sep_min = 100;
    v.push(en);
    v.push(p("hot_ac", "Hot AC", "AC", "Současný pop pro dospělé: zhruba 45 % aktuální, 35 % rekurenty, 20 % katalog. Energie kolem ranních a odpoledních vstupů, večer klidněji.", 12,
        cur_rec_gold(45.0, 35.0, 20.0), Rules { energy_max_jump: 2, ..rules(3, 75, 3) },
        vec![goal("energy", "1", 0.0, 10.0, "Ráno", "soft")], &["Ráno a odpoledne vyšší energie (pozice s požadavkem energie ≥ 3 po reklamě)."]));
    v.push(p("mainstream_ac", "Mainstream AC", "AC", "20 % aktuální, 30 % rekurenty, 50 % katalog. Jemnější zvukové přechody a větší ochrana stejných hodin.", 12,
        cur_rec_gold(20.0, 30.0, 50.0), Rules { energy_max_jump: 2, same_hour_days: 5, ..rules(4, 90, 5) }, vec![], &[]));
    v.push(p("soft_ac", "Soft AC / Relax", "AC", "10 / 25 / 65 % aktuální / rekurenty / katalog. Strop energie, kontrola prudkých změn, večer nižší tempo.", 12,
        cur_rec_gold(10.0, 25.0, 65.0), Rules { energy_max_jump: 1, same_hour_days: 5, ..rules(6, 120, 5) }, vec![goal("energy", "5", 0.0, 5.0, "", "soft")], &["Večerní profil: pozice s požadavkem energie ≤ 3."]));
    v.push(p("adult_hits", "Adult Hits / Variety", "Katalogová", "60 % hlavní známý katalog, 30 % sekundární, 10 % překvapení. Co nejširší pokrytí dekád, omezení opakovaných dvojic a sledů.", 13,
        vec![role("core", "Hlavní katalog", 60.0, "normal", 1, ""), role("secondary", "Sekundární", 30.0, "slow", 2, ""), role("surprise", "Překvapení", 10.0, "slow", 3, "méně hrané skladby")],
        Rules { max_in_row: BTreeMap::from([("decade".into(), 2)]), ..rules(8, 120, 7) }, vec![], &[]));
    v.push(p("classic_hits", "Classic Hits / Oldies", "Katalogová", "45 % hlavní dekáda, 35 % vedlejší dekády, 20 % hlubší katalog. Varianty Oldies a Millenium upravíte sadou dekád v kategoriích.", 13,
        vec![role("main_decade", "Hlavní dekáda", 45.0, "normal", 1, ""), role("side_decades", "Vedlejší dekády", 35.0, "normal", 2, ""), role("deep", "Hlubší katalog", 20.0, "slow", 3, "")],
        rules(8, 120, 7), vec![], &[]));
    v.push(p("classic_rock", "Classic Rock / AOR", "Rock", "45 % podpisové skladby, 35 % katalog, 20 % hluboký katalog. Hlídá skupiny i členy se sólovou kariérou (vyplňte identity interpretů), délku a kontrast balad.", 11,
        vec![role("signature", "Podpisové", 45.0, "normal", 1, ""), role("catalog", "Katalog", 35.0, "slow", 2, ""), role("deep", "Hluboký katalog", 20.0, "slow", 3, "")],
        Rules { max_in_row: BTreeMap::from([("mood".into(), 2)]), ..rules(6, 90, 5) }, vec![], &["Balady: atribut nálada = ballad, nejvýše 2 za sebou."]));
    v.push(p("rock_alt", "Active Rock / Alternative / Indie / AAA", "Rock", "Výchozí 35 / 30 / 35 % aktuální / rekurenty / katalog. Active Rock vyšší energie, Indie nové objevy, AAA albumové a méně známé skladby.", 11,
        cur_rec_gold(35.0, 30.0, 35.0), rules(4, 90, 4), vec![], &["Varianty: Active Rock – pozice s energií ≥ 4; AAA – víc Katalogu a vyšší váha méně známých (známost ≤ 3)."]));
    v.push(p("dance", "Dance / EDM", "Rytmická", "45 % aktuální, 30 % rekurenty, 25 % katalog. BPM a energetické oblouky, klubové mixy ve večerních blocích.", 13,
        cur_rec_gold(45.0, 30.0, 25.0), Rules { energy_max_jump: 2, ..rules(3, 60, 3) }, vec![], &["Klubové mixy jako samostatná kategorie jen ve večerním schématu."]));
    v.push(p("rhythmic", "Hip-hop / Rhythmic CHR / R&B", "Rytmická", "45 / 35 / 20 % aktuální / rekurenty / katalog. Silná ochrana hlavních a hostujících hlasů, clean / explicit podle pásma.", 12,
        cur_rec_gold(45.0, 35.0, 20.0), Rules { explicit_forbidden: vec![(6, 22)], artist_sep_min: 75, ..rules(3, 75, 3) }, vec![], &["Explicitní verze jen 22–06; přes den clean edit (verze sdílí ochranu díla)."]));
    v.push(p("urban_ac", "Urban AC / Soul / Funk", "Rytmická", "20 / 30 / 50 %, současné R&B odděleně od staršího soulu a funku; vyvažování zvukových ér podle směny.", 12,
        cur_rec_gold(20.0, 30.0, 50.0), rules(5, 90, 5), vec![], &[]));
    v.push(p("country", "Contemporary Country", "Country", "Současná varianta 40 / 30 / 30 %. Hlavní hlasy, skupiny a odstup balad.", 12,
        cur_rec_gold(40.0, 30.0, 30.0), Rules { max_in_row: BTreeMap::from([("mood".into(), 2)]), ..rules(4, 90, 4) }, vec![], &[]));
    v.push(p("classic_country", "Classic Country", "Country", "Klasická varianta 5 / 15 / 80 %.", 12,
        cur_rec_gold(5.0, 15.0, 80.0), rules(8, 120, 7), vec![], &[]));
    v.push(p("jazz", "Jazz / Smooth Jazz / Blues", "Specializovaná", "60 % základní repertoár, 25 % průzkum katalogu, 15 % hostující styly. Sólista, vokál / instrumentál, doba a typ souboru; Smooth Jazz plynulost, Blues podžánry a éry.", 10,
        vec![role("core", "Základní repertoár", 60.0, "normal", 1, ""), role("explore", "Průzkum katalogu", 25.0, "slow", 2, ""), role("guest", "Hostující styly", 15.0, "slow", 3, "")],
        Rules { max_in_row: BTreeMap::from([("gender".into(), 2)]), ..rules(12, 180, 7) }, vec![], &["Vokál / instrumentál: atribut zpěv (gender) – nejvýše 2 vokální za sebou."]));
    let mut cl = p("classical", "Klasická hudba", "Specializovaná", "Zastoupení v minutách: 60 % hlavní epochy, 25 % vedlejší, 15 % objevná díla. Hlídá skladatele, dílo, sólistu a těleso; věty díla drží pohromadě.", 5,
        vec![role("main_era", "Hlavní epochy", 60.0, "normal", 1, ""), role("side_era", "Vedlejší epochy", 25.0, "slow", 2, ""), role("discovery", "Objevná díla", 15.0, "slow", 3, "")],
        Rules { protect_versions: true, ..rules(24, 240, 7) }, vec![], &["Skladatel = interpret (identita), dílo = klíč díla; celé dílo zařaďte jako pevný titul nebo sadu.", "Pořadí vět zamkněte v editoru (zámek)."]);
    cl.minutes = true;
    v.push(cl);
    let mut w = p("world_folk", "World / Folk / Regionální", "Specializovaná", "50 % domácí nebo hlavní region, 30 % příbuzné tradice, 20 % širší výběr. Definici lokální skladby a jazyka nastavuje stanice.", 12,
        vec![role("home", "Domácí / hlavní region", 50.0, "normal", 1, ""), role("related", "Příbuzné tradice", 30.0, "normal", 2, ""), role("wider", "Širší výběr", 20.0, "slow", 3, "")], rules(6, 120, 5), vec![], &[]);
    w.ask = vec!["local".into()];
    v.push(w);
    v.push(p("latin", "Latin / Reggaetón", "Rytmická", "45 / 30 / 25 % aktuální / rekurenty / katalog; jazyk a podžánr, rovnováha klubových a melodických titulů.", 12,
        cur_rec_gold(45.0, 30.0, 25.0), Rules { max_in_row: BTreeMap::from([("genre".into(), 3)]), ..rules(3, 75, 3) }, vec![], &[]));
    let mut nt = p("news_talk", "News/Talk s hudbou", "Mluvené slovo", "Clock řídí pevné body řeči a reklamy, hudba vyplňuje okna. Hudební cíle se počítají jen v hudebních úsecích.", 4,
        cur_rec_gold(20.0, 30.0, 50.0), rules(6, 120, 5), vec![], &["Zprávy v :00 a :30 jako pevné body, reklama jako zástupný blok."]);
    nt.optional_slots = vec!["Zprávy :00".into(), "Zprávy :30".into()];
    v.push(nt);
    v.push(p("christmas", "Vánoční sezóna", "Sezónní", "Datovaný profil s náběhem, hlavním obdobím a návratem k formátu. Ochrana stejné melodie přes různé interprety (klíč díla), měnitelný denní mix.", 12,
        vec![role("christmas_a", "Vánoční hity", 35.0, "normal", 1, "nejznámější vánoční"), role("christmas_b", "Vánoční ostatní", 15.0, "slow", 2, ""), role("base", "Základní formát", 50.0, "normal", 3, "běžná rotace stanice")],
        Rules { protect_versions: true, work_sep_min: 360, ..rules(4, 90, 3) }, vec![], &["Stejná melodie od různých interpretů: nastavte stejný klíč díla (Atributy skladeb).", "Nasaďte přes výjimky mřížky: náběh 1.–15. 12. (méně vánočních), hlavní 16.–26. 12., pak zpět."]));
    v
}

/// Rozdělí podíly rolí na pozice (metoda největšího zbytku) a rozprostře je v hodině.
pub fn positions(roles: &[(String, f64)], n: usize) -> Vec<String> {
    let total: f64 = roles.iter().map(|r| r.1).sum::<f64>().max(1e-9);
    let mut alloc: Vec<(String, usize, f64)> = roles.iter().map(|(id, s)| { let x = s / total * n as f64; (id.clone(), x.floor() as usize, x - x.floor()) }).collect();
    let mut left = n.saturating_sub(alloc.iter().map(|a| a.1).sum());
    let mut order: Vec<usize> = (0..alloc.len()).collect();
    order.sort_by(|a, b| alloc[*b].2.partial_cmp(&alloc[*a].2).unwrap());
    for i in order { if left == 0 { break; } alloc[i].1 += 1; left -= 1; }
    // rozprostřít: každá role na rovnoměrně rozložených místech (nejdřív role s nejvíc pozicemi)
    let mut slots: Vec<Option<String>> = vec![None; n];
    alloc.sort_by(|a, b| b.1.cmp(&a.1));
    for (id, k, _) in alloc {
        for j in 0..k {
            let ideal = ((j as f64 + 0.5) * n as f64 / k as f64) as usize;
            let pos = (0..n).map(|d| [ideal + d, ideal.wrapping_sub(d)]).flatten().find(|p| *p < n && slots[*p].is_none());
            if let Some(p) = pos { slots[p] = Some(id.clone()); }
        }
    }
    slots.into_iter().flatten().collect()
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ApplyPreset {
    pub preset_id: String,
    /// role → kategorie knihovny
    pub mapping: BTreeMap<String, String>,
    /// fast | normal | slow
    pub rotation: String,
    /// hudebních pozic za hodinu (0 = podle presetu)
    pub music_per_hour: u32,
    /// odpovědi na dotazy presetu, např. local = 30 (%)
    pub answers: BTreeMap<String, f64>,
    /// kategorie jinglů pro ID na začátku hodiny (prázdné = bez)
    pub id_category: String,
    pub sweeper_category: String,
    pub with_news: bool,
}

/// Sestaví konfiguraci selektoru z presetu. Stávající konfiguraci nepřepisuje – vrací návrh pro náhled / simulaci.
pub fn apply(p: &Preset, a: &ApplyPreset, base: &SelectorConfig) -> SelectorConfig {
    let mut c = SelectorConfig { mode: base.mode.clone(), dayparts: if base.dayparts.is_empty() { SelectorConfig::default().dayparts } else { base.dayparts.clone() }, seed: base.seed, qualify: base.qualify.clone(), ..Default::default() };
    c.format_name = p.name.clone();
    c.preset_id = p.id.clone();
    c.preset_version = p.version;
    c.rules = p.rules.clone();
    let k = match a.rotation.as_str() { "fast" => 0.75, "slow" => 1.3, _ => 1.0 };
    let sc = |m: u32| ((m as f64 * k) / 5.0).round() as u32 * 5;
    c.rules.song_sep_min = sc(c.rules.song_sep_min); c.rules.song_target_min = sc(c.rules.song_target_min); c.rules.work_sep_min = sc(c.rules.work_sep_min);
    c.goals = p.goals.clone();
    if let Some(local) = a.answers.get("local") { c.goals.push(Goal { id: "local".into(), attr: "local".into(), value: "1".into(), target_pct: *local, tolerance_pct: 5.0, unit: "count".into(), scope: "day".into(), severity: "soft".into(), daypart: String::new(), weight: 2.0 }); }
    if p.minutes { for g in c.goals.iter_mut() { g.unit = "minutes".into(); } }
    let mapped: Vec<(String, f64)> = p.roles.iter().filter_map(|r| a.mapping.get(&r.id).filter(|m| !m.is_empty()).map(|m| (m.clone(), r.share_pct))).collect();
    // více rolí na jednu kategorii → sečíst
    let mut merged: Vec<(String, f64)> = vec![];
    for (m, s) in mapped { if let Some(x) = merged.iter_mut().find(|x| x.0 == m) { x.1 += s; } else { merged.push((m, s)); } }
    for r in &p.roles {
        if let Some(cat) = a.mapping.get(&r.id).filter(|m| !m.is_empty()) {
            if c.categories.iter().any(|x| &x.name == cat) { continue; }
            c.categories.push(SelCategory { name: cat.clone(), kind: TrackKind::Song, role: r.id.clone(), pass: r.pass, rotation: r.rotation.clone(), ..Default::default() });
        }
    }
    let n = if a.music_per_hour > 0 { a.music_per_hour } else { p.music_per_hour } as usize;
    // dvě hodiny A/B – poměry, které jedna hodina neumí přesně, se vyrovnají ve dvou
    let two = positions(&merged, n * 2);
    for (ci, name) in ["A", "B"].iter().enumerate() {
        let mut slots = vec![];
        if !a.id_category.is_empty() { slots.push(Slot { kind: "imaging".into(), category: a.id_category.clone(), track_kind: Some(TrackKind::Jingle), label: "Station ID".into(), ..Default::default() }); }
        if a.with_news { slots.push(Slot { kind: "news".into(), stitch: Some("news".into()), label: "Zprávy".into(), at_min: Some(0), ..Default::default() }); }
        for (i, cat) in two.iter().skip(ci * n).take(n).enumerate() {
            if !a.sweeper_category.is_empty() && i > 0 && i % 3 == 0 {
                slots.push(Slot { kind: "imaging".into(), category: a.sweeper_category.clone(), track_kind: Some(TrackKind::Sweeper), label: "Sweeper".into(), position: Some("start".into()), ..Default::default() });
            }
            slots.push(Slot { kind: "music".into(), category: cat.clone(), ..Default::default() });
        }
        c.clocks.push(SelClock { name: format!("{} {name}", p.name), color: if ci == 0 { "#22c55e".into() } else { "#3b82f6".into() }, slots, note: format!("{} hudebních pozic, střídá se s druhou hodinou", n) });
    }
    let (ca, cb) = (c.clocks[0].name.clone(), c.clocks[1].name.clone());
    for d in 1..=7 { for h in 0..24u32 { c.grid.insert(format!("{d}-{h}"), if h % 2 == 0 { ca.clone() } else { cb.clone() }); } }
    c.default_clock = ca;
    if c.dayparts.is_empty() { c.dayparts = vec![Daypart { name: "Celý den".into(), from_hour: 0, to_hour: 24 }]; }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn largest_remainder_and_spread() {
        let v = positions(&[("P".into(), 15.0), ("C".into(), 30.0), ("R".into(), 30.0), ("G".into(), 25.0)], 20);
        assert_eq!(v.len(), 20);
        assert_eq!(v.iter().filter(|x| *x == "P").count(), 3);
        assert_eq!(v.iter().filter(|x| *x == "G").count(), 5);
        // power nejsou vedle sebe
        let p: Vec<usize> = v.iter().enumerate().filter(|(_, x)| *x == "P").map(|(i, _)| i).collect();
        assert!(p.windows(2).all(|w| w[1] - w[0] >= 4), "{v:?}");
        assert!(catalog().len() >= 19);
    }
}
