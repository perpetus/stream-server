use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct VideoMetadata {
    pub name: Option<String>,
    pub year: Option<i32>,
    pub season: Option<i32>,
    pub episode: Option<Vec<i32>>,
    pub disk_number: Option<i32>,
    pub type_: String,
    pub imdb_id: Option<String>,
    pub tags: Vec<String>,
}

fn get_extensions() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\.(mkv|avi|mp4|wmv|vp8|mov|mpg|mp3|flac)$").unwrap())
}

fn get_movie_keywords() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(1080p|720p|480p|blurayrip|brrip|divx|dvdrip|hdrip|hdtv|tvrip|xvid|camrip)",
        )
        .unwrap()
    })
}

fn get_season_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)S(\d{1,2})").unwrap())
}

fn get_episode_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)E(\d{2})").unwrap())
}

fn get_year_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(19|20)\d{2}\b").unwrap())
}

fn get_sample_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(sample|etrg)").unwrap())
}

fn has_episode_prefix(text: &str, episode_start: usize) -> bool {
    text[..episode_start]
        .chars()
        .next_back()
        .is_some_and(|ch| !ch.is_alphanumeric() || ch.is_numeric())
}

fn has_episode_marker(text: &str) -> bool {
    get_episode_regex()
        .captures_iter(text)
        .any(|caps| caps.get(0).is_some_and(|m| has_episode_prefix(text, m.start())))
}

pub fn parse_filename(path: &Path) -> Option<VideoMetadata> {
    let filename = path.file_name()?.to_str()?;
    if !get_extensions().is_match(filename) {
        return None;
    }

    let mut meta = VideoMetadata::default();
    meta.type_ = "other".to_string();

    let clean_name = filename
        .replace('.', " ")
        .replace('_', " ")
        .replace('-', " ");

    if let Some(caps) = get_year_regex().find(&clean_name) {
        if let Ok(year) = caps.as_str().parse::<i32>() {
            meta.year = Some(year);
        }
    }

    if let Some(caps) = get_season_regex().captures(&clean_name) {
        if let Some(s) = caps.get(1) {
            meta.season = s.as_str().parse::<i32>().ok();
        }
    }

    let mut episodes = Vec::new();
    for caps in get_episode_regex().captures_iter(&clean_name) {
        let Some(episode_match) = caps.get(0) else {
            continue;
        };
        if !has_episode_prefix(&clean_name, episode_match.start()) {
            continue;
        }
        if let Some(e) = caps.get(1) {
            if let Ok(ep) = e.as_str().parse::<i32>() {
                episodes.push(ep);
            }
        }
    }
    if !episodes.is_empty() {
        meta.episode = Some(episodes);
    }

    // Determine Type
    if meta.season.is_some() && meta.episode.is_some() {
        meta.type_ = "series".to_string();
    } else if meta.year.is_some() || get_movie_keywords().is_match(&clean_name) {
        meta.type_ = "movie".to_string();
    }

    let parts: Vec<&str> = clean_name.split_whitespace().collect();
    let mut name_parts = Vec::new();
    for part in parts {
        if get_year_regex().is_match(part)
            || get_season_regex().is_match(part)
            || has_episode_marker(part)
            || get_movie_keywords().is_match(part)
        {
            break;
        }
        name_parts.push(part);
    }

    if !name_parts.is_empty() {
        meta.name = Some(name_parts.join(" "));
    } else {
        meta.name = Some(
            path.file_stem()?
                .to_str()?
                .to_string()
                .replace('.', " ")
                .replace('_', " "),
        );
    }

    if clean_name.contains("1080p") {
        meta.tags.push("1080p".to_string());
        meta.tags.push("hd".to_string());
    }
    if clean_name.contains("720p") {
        meta.tags.push("720p".to_string());
    }
    if clean_name.contains("480p") {
        meta.tags.push("480p".to_string());
    }
    if get_sample_regex().is_match(&clean_name) {
        meta.tags.push("sample".to_string());
    }

    Some(meta)
}

#[cfg(test)]
mod tests {
    use super::parse_filename;
    use std::path::Path;

    #[test]
    fn parses_compact_season_episode() {
        let meta = parse_filename(Path::new("Some.Show.S01E02.mkv")).unwrap();

        assert_eq!(meta.name.as_deref(), Some("Some Show"));
        assert_eq!(meta.season, Some(1));
        assert_eq!(meta.episode, Some(vec![2]));
        assert_eq!(meta.type_, "series");
    }

    #[test]
    fn preserves_leading_episode_marker_behavior() {
        let meta = parse_filename(Path::new("E02.Some.Show.mkv")).unwrap();

        assert_eq!(meta.episode, None);
    }
}
