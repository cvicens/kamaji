use once_cell::sync::Lazy;
use regex::Regex;

// Good enough for v1: match http(s) URLs up to the next whitespace, trimming
// common trailing punctuation that isn't part of the link (e.g. a period
// ending a sentence, or a closing paren from surrounding prose).
static URL_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"https?://[^\s]+").expect("static regex is valid"));

pub fn extract_urls(text: &str) -> Vec<String> {
    URL_RE
        .find_iter(text)
        .map(|m| trim_trailing_punctuation(m.as_str()).to_string())
        .collect()
}

fn trim_trailing_punctuation(url: &str) -> &str {
    url.trim_end_matches(['.', ',', ')', ']', '>', '\'', '"', '!', '?'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_single_url() {
        let urls = extract_urls("check this out https://example.com/foo it's great");
        assert_eq!(urls, vec!["https://example.com/foo"]);
    }

    #[test]
    fn extracts_multiple_urls() {
        let urls = extract_urls("see https://a.com and http://b.com/x?y=1 too");
        assert_eq!(urls, vec!["https://a.com", "http://b.com/x?y=1"]);
    }

    #[test]
    fn no_urls_returns_empty() {
        assert!(extract_urls("just some plain text").is_empty());
    }

    #[test]
    fn trims_trailing_sentence_punctuation() {
        let urls = extract_urls("worth reading (https://example.com/page).");
        assert_eq!(urls, vec!["https://example.com/page"]);
    }
}
