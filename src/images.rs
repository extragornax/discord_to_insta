use once_cell::sync::Lazy;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

static DISTANCE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)distance\s*[:=]\s*(\d+(?:[.,]\d+)?)\s*km").unwrap());

/// Parse a distance-in-kilometers value out of a Discord message body.
/// Matches e.g. `Distance : 20km`, `distance: 19.7 km`, `distance=18,4km`.
/// The value is rounded to the nearest integer.
pub fn parse_distance_km(raw: &str) -> Option<u32> {
    let caps = DISTANCE_RE.captures(raw)?;
    let num = caps.get(1)?.as_str().replace(',', ".");
    let km: f64 = num.parse().ok()?;
    if km.is_finite() && km >= 0.0 {
        Some(km.round() as u32)
    } else {
        None
    }
}

/// Find the post template image for a given integer kilometer distance.
/// Convention: any file in `images_dir` whose stem ends in `_{km}` wins.
/// Returns the first match (deterministic thanks to sorted order).
pub fn image_for_distance(images_dir: &Path, km: u32) -> Option<PathBuf> {
    let suffix_png = format!("_{km}.png");
    let suffix_jpg = format!("_{km}.jpg");
    let suffix_jpeg = format!("_{km}.jpeg");
    let mut candidates: Vec<PathBuf> = fs::read_dir(images_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    let stem = Path::new(n).file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    stem.ends_with(&format!("_{km}"))
                        && (n.ends_with(&suffix_png)
                            || n.ends_with(&suffix_jpg)
                            || n.ends_with(&suffix_jpeg))
                })
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_basic() {
        assert_eq!(parse_distance_km("📏 Distance : 20km ;"), Some(20));
    }

    #[test]
    fn distance_with_space() {
        assert_eq!(parse_distance_km("Distance : 20 km"), Some(20));
    }

    #[test]
    fn distance_decimal_rounds_up() {
        assert_eq!(parse_distance_km("distance: 19.7 km"), Some(20));
    }

    #[test]
    fn distance_comma_decimal() {
        assert_eq!(parse_distance_km("Distance=18,4km"), Some(18));
    }

    #[test]
    fn distance_half_rounds_away_from_zero() {
        // f64::round rounds half away from zero (19.5 -> 20), which matches
        // "round to the nearest int" as a human would read it.
        assert_eq!(parse_distance_km("Distance: 19.5 km"), Some(20));
    }

    #[test]
    fn distance_missing_returns_none() {
        assert_eq!(parse_distance_km("no distance here"), None);
    }

    #[test]
    fn image_lookup_matches_suffix() {
        let tmp = tempdir();
        std::fs::write(tmp.join("mayo-post-ok-v6-2025-feed_20.png"), b"fake").unwrap();
        std::fs::write(tmp.join("mayo-post-ok-v6-2025-feed_25.png"), b"fake").unwrap();
        let found = image_for_distance(&tmp, 20).expect("found");
        assert!(found.to_string_lossy().ends_with("feed_20.png"));
    }

    #[test]
    fn image_lookup_ignores_other_numbers() {
        let tmp = tempdir();
        std::fs::write(tmp.join("feed_200.png"), b"fake").unwrap();
        // suffix "_20" must not match "_200".
        assert!(image_for_distance(&tmp, 20).is_none());
    }

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "discord_to_insta_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
