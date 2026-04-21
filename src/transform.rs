use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

static EVERYONE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"@everyone\b|@here\b").unwrap());
static USER_MENTION_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<@!?(\d+)>").unwrap());
static CHANNEL_MENTION_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<#(\d+)>").unwrap());
static ROLE_MENTION_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<@&(\d+)>").unwrap());
// Trailing Discord relative-time suffix: "1d", "2h", "5m", "30s", "3w" on its own trailing line.
static TRAILING_TIMESTAMP_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)\n\s*\d+\s*[smhdw]\s*$").unwrap());

/// Pure transformation: Discord announcement text → Instagram caption.
///
/// `user_map` resolves Discord user IDs (as strings) to Instagram handles
/// (without the leading `@`).
pub fn discord_to_caption(raw: &str, user_map: &HashMap<String, String>) -> String {
    let mut text = raw.replace("\r\n", "\n");

    // 1. Drop the "Réactions :" trailer (from that line to end-of-message).
    if let Some(idx) = find_reactions_block(&text) {
        text.truncate(idx);
    }

    // 2. Drop Discord's trailing relative-time suffix ("1d", "2h", ...).
    text = TRAILING_TIMESTAMP_RE.replace(&text, "").into_owned();

    // 3. Strip @everyone / @here.
    text = EVERYONE_RE.replace_all(&text, "").into_owned();

    // 4. Replace user mentions with @handle (or a neutral fallback).
    text = USER_MENTION_RE
        .replace_all(&text, |caps: &regex::Captures| match user_map.get(&caps[1]) {
            Some(handle) => format!("@{handle}"),
            None => String::new(),
        })
        .into_owned();

    // 5. Replace channel & role mentions with a generic pointer to Discord.
    text = CHANNEL_MENTION_RE
        .replace_all(&text, "voir Discord (lien en bio)")
        .into_owned();
    text = ROLE_MENTION_RE
        .replace_all(&text, "voir Discord (lien en bio)")
        .into_owned();

    // 6. Normalize whitespace introduced by the removals without collapsing
    //    intentional blank lines in the body.
    collapse_inline_spaces(&text).trim().to_string()
}

fn find_reactions_block(text: &str) -> Option<usize> {
    // Match a line that starts with "Réactions" (accent-tolerant) followed by
    // optional whitespace and a colon. Anchored at line start.
    for (idx, line) in line_offsets(text) {
        let stripped = line.trim_start();
        if (stripped.starts_with("Réactions") || stripped.starts_with("Reactions"))
            && stripped.trim_end().ends_with(':')
        {
            // Include the preceding newline(s) in the cut so we don't leave a
            // dangling blank line.
            let cut = text[..idx].trim_end_matches(|c: char| c == '\n' || c == ' ').len();
            return Some(cut);
        }
    }
    None
}

fn line_offsets(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0usize;
    text.split_inclusive('\n').map(move |line| {
        let start = offset;
        offset += line.len();
        (start, line.trim_end_matches('\n'))
    })
}

/// Collapse runs of spaces/tabs (but not newlines) left over from removed
/// tokens. e.g. "Cartographe :  ;" → "Cartographe : ;" becomes "Cartographe :".
fn collapse_inline_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        if ch == ' ' || ch == '\t' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            prev_space = false;
            out.push(ch);
        }
    }
    // Tidy the common artefact " ;" → ";" only when a mention was stripped and
    // left a dangling separator. We avoid global cleanup to stay surgical.
    out.replace(" :\n", " :\n")
        .replace(" ;\n", " ;\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mayo_jaune_map() -> HashMap<String, String> {
        HashMap::from([(
            "699543821465419806".to_string(),
            "bertrandbernager".to_string(),
        )])
    }

    const MAYO_JAUNE_RAW: &str = "@everyone\n\n⏰ RDV : Lundi 20 avril à 19h45 (départ 20h) sur la place de la Bastille à Paris ;\n📏 Distance : 20km ; D+ : 170m ;\n📍 Arrivée : Les Blédards, 161 Quai de Valmy, 75010 Paris ;\n⏩ Allure sur plat : 25+km/h ;\n🗺️ Cartographe : <@699543821465419806> ;\n🏁 Guides : On verra sur place ;\n🌍 Tout le monde est le bienvenu.\n\nThème : Haute Joaillerie 💎\nCette carte passe entre-autres par : Collection de Minéraux de Sorbonne Université, rue des Cinq Diamants, Musée de Minéralogie de l’École des Mines, Pyramide du Louvre, Place Vendôme, École des Arts Joailliers et le restaurant Les Diamantaires (ancien quartier du diamant) 💍\n\nRappel :\n- Vérifiez l'état de votre vélo avant la ride ⚙️ ;\n- Casque 🪖 et lampes avant et arrière 🔦 ;\n- Règles à respecter lorsque l'on roule à Mayo Jaune : <#1126221340056223816> ;\n- Retrouvez la trace sur notre compte komoot le jour suivant de la balade ;\n- Suivant la météo ou par manque de staff, la balade peut être annulée. Une annonce sera faite dans ce cas.\n\nRéactions :\n- Je viens : ✅ ;\n- Pas disponible : 🚫 ;\n- Pas encore sûr d'être disponible/j'attends de voir qui vient : 🤔\n1d";

    #[test]
    fn mayo_jaune_golden() {
        let got = discord_to_caption(MAYO_JAUNE_RAW, &mayo_jaune_map());
        assert!(!got.contains("@everyone"));
        assert!(!got.contains("<@"));
        assert!(!got.contains("<#"));
        assert!(!got.contains("Réactions"));
        assert!(!got.trim_end().ends_with("1d"));
        assert!(got.contains("@bertrandbernager"));
        assert!(got.contains("voir Discord (lien en bio)"));
        // Body preserved.
        assert!(got.contains("Thème : Haute Joaillerie 💎"));
        assert!(got.contains("Place Vendôme"));
    }

    #[test]
    fn unknown_user_mention_is_dropped() {
        let raw = "Hello <@111> world";
        let got = discord_to_caption(raw, &HashMap::new());
        assert_eq!(got, "Hello world");
    }

    #[test]
    fn channel_mention_becomes_bio_pointer() {
        let raw = "Rules: <#42>";
        let got = discord_to_caption(raw, &HashMap::new());
        assert_eq!(got, "Rules: voir Discord (lien en bio)");
    }

    #[test]
    fn trailing_timestamp_removed() {
        let raw = "hello world\n2h";
        let got = discord_to_caption(raw, &HashMap::new());
        assert_eq!(got, "hello world");
    }

    #[test]
    fn reactions_block_and_everything_after_removed() {
        let raw = "keep me\n\nRéactions :\n- A\n- B\ntrailing";
        let got = discord_to_caption(raw, &HashMap::new());
        assert_eq!(got, "keep me");
    }
}
