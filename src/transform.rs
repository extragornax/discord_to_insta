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

// Custom Discord emojis: `<:name:12345>` or `<a:name:12345>` (animated).
// Replace with `:name:` so the intent remains readable in the caption.
static CUSTOM_EMOJI_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<a?:([A-Za-z0-9_]+):\d+>").unwrap());

// Markdown delimiters. Rust's `regex` crate has no native dotall toggle per
// pattern; we use (?s) for the fenced-code block, and default mode (. does
// NOT cross newlines) for everything else so delimiters only match within
// a single paragraph.
static CODE_FENCE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?s)```(?:[A-Za-z0-9_+-]*\n)?(.*?)\n?```").unwrap());
static CODE_INLINE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"`([^`\n]+)`").unwrap());
static BOLD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*\*([^\n]+?)\*\*").unwrap());
static UNDERLINE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"__([^\n]+?)__").unwrap());
static ITALIC_STAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\*([^\n\*]+?)\*").unwrap());
static STRIKE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"~~([^\n]+?)~~").unwrap());
static SPOILER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\|\|([^\n]+?)\|\|").unwrap());
// Line-start markers (headings, subtext, blockquotes). Multiline mode so ^
// matches the start of each line.
static LINE_PREFIX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\s*(?:#{1,3}\s+|-#\s+|>>>\s?|>\s?)").unwrap());
// Bracketed URLs — Discord wraps links in `<...>` to suppress the embed
// preview. The brackets are noise in the caption.
static BRACKET_URL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<(https?://[^\s>]+)>").unwrap());

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

    // 6. Custom emojis (`<:name:id>`) → `:name:` so intent is preserved.
    text = replace_custom_emojis(&text);

    // 7. Strip Discord markdown delimiters so the caption doesn't show
    //    literal `**` or triple-backticks on Instagram (which doesn't render
    //    them).
    text = strip_markdown(&text);

    // 8. Unwrap `<https://…>` bracketed URLs.
    text = BRACKET_URL_RE.replace_all(&text, "$1").into_owned();

    // 9. Normalize whitespace introduced by the removals without collapsing
    //    intentional blank lines in the body.
    collapse_inline_spaces(&text).trim().to_string()
}

/// Replace `<:name:id>` and `<a:name:id>` with `:name:`.
pub(crate) fn replace_custom_emojis(text: &str) -> String {
    CUSTOM_EMOJI_RE.replace_all(text, ":$1:").into_owned()
}

/// Strip Discord markdown delimiters while preserving the content they
/// wrap. Intentionally conservative — does NOT strip single-underscore
/// italic (`_foo_`) to avoid mangling `snake_case` / file names.
pub(crate) fn strip_markdown(text: &str) -> String {
    let mut out = CODE_FENCE_RE.replace_all(text, "$1").into_owned();
    out = CODE_INLINE_RE.replace_all(&out, "$1").into_owned();
    out = BOLD_RE.replace_all(&out, "$1").into_owned();
    out = UNDERLINE_RE.replace_all(&out, "$1").into_owned();
    out = ITALIC_STAR_RE.replace_all(&out, "$1").into_owned();
    out = STRIKE_RE.replace_all(&out, "$1").into_owned();
    out = SPOILER_RE.replace_all(&out, "$1").into_owned();
    out = LINE_PREFIX_RE.replace_all(&out, "").into_owned();
    out
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

    #[test]
    fn custom_emoji_becomes_colon_name_colon() {
        assert_eq!(replace_custom_emojis("Hello <:sparkles:12345>!"), "Hello :sparkles:!");
        // Animated.
        assert_eq!(replace_custom_emojis("<a:dance:987654>"), ":dance:");
        // Multiple in one string.
        assert_eq!(
            replace_custom_emojis("<:one:111> and <:two:222>"),
            ":one: and :two:"
        );
    }

    #[test]
    fn unicode_shortcodes_left_alone() {
        // Plain `:sparkles:` is a Unicode emoji shortcode — no angle brackets,
        // no numeric ID — so it must pass through untouched.
        assert_eq!(replace_custom_emojis("keep :this: alone"), "keep :this: alone");
    }

    #[test]
    fn markdown_bold_italic_strike() {
        assert_eq!(strip_markdown("**bold** text"), "bold text");
        assert_eq!(strip_markdown("*italic* here"), "italic here");
        assert_eq!(strip_markdown("~~nope~~ yep"), "nope yep");
    }

    #[test]
    fn markdown_underline_not_confused_with_snake_case() {
        assert_eq!(strip_markdown("__underline__"), "underline");
        // Crucially, single underscores stay — don't wreck file_name_here.
        assert_eq!(strip_markdown("the file_name_here.txt"), "the file_name_here.txt");
    }

    #[test]
    fn markdown_inline_code_and_fence() {
        assert_eq!(strip_markdown("use `cargo test` please"), "use cargo test please");
        assert_eq!(
            strip_markdown("before\n```rust\nfn main() {}\n```\nafter"),
            "before\nfn main() {}\nafter"
        );
    }

    #[test]
    fn markdown_headings_blockquotes_subtext_spoiler() {
        assert_eq!(strip_markdown("# Big\n## Medium\n### Small"), "Big\nMedium\nSmall");
        assert_eq!(strip_markdown("> quoted line\n> another"), "quoted line\nanother");
        assert_eq!(strip_markdown(">>> big quote block"), "big quote block");
        assert_eq!(strip_markdown("-# fine print"), "fine print");
        assert_eq!(strip_markdown("||spoiler text||"), "spoiler text");
    }

    #[test]
    fn bracketed_urls_unwrapped() {
        let raw = "see <https://example.com/path?a=1> for details";
        let got = discord_to_caption(raw, &HashMap::new());
        assert_eq!(got, "see https://example.com/path?a=1 for details");
    }

    #[test]
    fn combined_transform_with_markdown_and_custom_emoji() {
        // An announcement-shaped message that exercises the new rules
        // alongside the existing ones.
        let raw = "@everyone\n**⏰ RDV**: 20h *sur place*\n📏 Distance : 25km ;\n<:mayo:9999> let's ride";
        let got = discord_to_caption(raw, &HashMap::new());
        assert!(!got.contains("@everyone"));
        assert!(!got.contains("**"));
        assert!(!got.contains("<:mayo:"));
        assert!(got.contains(":mayo:"));
        assert!(got.contains("⏰ RDV"));
        assert!(got.contains("sur place"));
    }
}
