use std::time::Duration;

use scraper::{Html, Selector};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("non-success status: {0}")]
    StatusCode(reqwest::StatusCode),
    #[error("requires authentication (X/Twitter, Instagram, etc.)")]
    RequiresAuth,
}

/// Fetches a URL and extracts readable content as plain text.
/// Tries to find main content (article, main) first, falls back to body.
pub async fn fetch_url_content(url: &str, timeout: Duration) -> Result<String, FetchError> {
    let client = reqwest::Client::builder()
        .user_agent("kamaji/0.1 (note-taking bot)")
        .timeout(timeout)
        .build()?;

    let response = client.get(url).send().await?;

    if !response.status().is_success() {
        return Err(FetchError::StatusCode(response.status()));
    }

    let html = response.text().await?;
    let content = extract_readable_text(url, &html);

    // If we got nothing useful and this is an auth-required site, return error
    if content.trim().is_empty() && requires_auth(url) {
        return Err(FetchError::RequiresAuth);
    }

    Ok(content)
}

/// Returns true if the URL is from a domain that requires authentication.
fn requires_auth(url: &str) -> bool {
    let auth_domains = [
        "x.com",
        "twitter.com",
        "instagram.com",
        "facebook.com",
        "linkedin.com",
    ];

    auth_domains.iter().any(|domain| {
        url.contains(&format!("://{}", domain)) || url.contains(&format!("://www.{}", domain))
    })
}

/// Extracts the main readable content from HTML.
/// Tries selectors in order: article, main, body.
/// Converts the best match to plain text.
fn extract_readable_text(url: &str, html: &str) -> String {
    let document = Html::parse_document(html);

    // Known JS-rendered/SPA sites (X/Twitter, Instagram, ...) serve almost
    // no real content in their initial HTML -- the OpenGraph description is
    // the only thing worth extracting there, so check it first. For every
    // other site, prefer the actual page body: nearly every page sets an
    // og:description for SEO, and preferring it unconditionally was
    // truncating ordinary articles down to a one-line summary.
    if requires_auth(url) {
        if let Some(meta_content) = extract_og_description(&document) {
            if !meta_content.trim().is_empty() {
                return meta_content;
            }
        }
    }

    // Try to find the main content container in order of preference
    let selectors = ["article", "main", "[role=main]", "body"];

    for selector_str in &selectors {
        if let Ok(selector) = Selector::parse(selector_str) {
            if let Some(element) = document.select(&selector).next() {
                let inner_html = element.html();
                // Convert HTML to text, wrapping at 80 chars
                let text = html2text::from_read(inner_html.as_bytes(), 80);
                if !text.trim().is_empty() {
                    return text;
                }
            }
        }
    }

    // Fallback for sites not in the auth-domain list that still turned out
    // to have no real body content (another JS shell, an empty page, etc.).
    if let Some(meta_content) = extract_og_description(&document) {
        if !meta_content.trim().is_empty() {
            return meta_content;
        }
    }

    // Last resort: convert entire document
    html2text::from_read(html.as_bytes(), 80)
}

/// Extracts OpenGraph description meta tag content.
/// Used for X/Twitter and other social sites that embed content in meta tags.
fn extract_og_description(document: &Html) -> Option<String> {
    let meta_selectors = [
        r#"meta[property="og:description"]"#,
        r#"meta[name="twitter:description"]"#,
        r#"meta[name="description"]"#,
    ];

    for selector_str in &meta_selectors {
        if let Ok(selector) = Selector::parse(selector_str) {
            if let Some(meta) = document.select(&selector).next() {
                if let Some(content) = meta.value().attr("content") {
                    return Some(content.to_string());
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_readable_text_prefers_article() {
        let html = r#"
            <html>
            <body>
                <nav>Navigation junk</nav>
                <article>
                    <h1>Article Title</h1>
                    <p>Main content here.</p>
                </article>
                <footer>Footer stuff</footer>
            </body>
            </html>
        "#;

        let text = extract_readable_text("https://example.com/article", html);
        assert!(text.contains("Article Title"));
        assert!(text.contains("Main content"));
        // Should NOT contain nav/footer if article extraction worked
        assert!(!text.contains("Navigation junk"));
    }

    #[test]
    fn extract_readable_text_falls_back_to_body() {
        let html = r#"
            <html>
            <body>
                <h1>Simple Page</h1>
                <p>Just some content.</p>
            </body>
            </html>
        "#;

        let text = extract_readable_text("https://example.com/page", html);
        assert!(text.contains("Simple Page"));
        assert!(text.contains("Just some content"));
    }

    #[test]
    fn ordinary_sites_prefer_full_article_over_short_og_description() {
        // Regression test: a normal blog post that also sets an
        // og:description (nearly every site does, for SEO) must not be
        // truncated down to that one-line summary.
        let html = r#"
            <html>
            <head>
                <meta property="og:description" content="Why & how we rewrote Bun from Zig to Rust">
            </head>
            <body>
                <article>
                    <h1>Why & how we rewrote Bun from Zig to Rust</h1>
                    <p>Full article body with much more detail than the summary.</p>
                </article>
            </body>
            </html>
        "#;

        let text = extract_readable_text("https://bun.com/blog/bun-in-rust", html);
        assert!(text.contains("Full article body with much more detail"));
    }

    #[test]
    fn auth_required_sites_still_prefer_og_description() {
        // X/Twitter's initial HTML has no real content -- only the OG
        // description reflects the actual post.
        let html = r#"
            <html>
            <head>
                <meta property="og:description" content="the tweet text">
            </head>
            <body><div id="react-root"></div></body>
            </html>
        "#;

        let text = extract_readable_text("https://x.com/user/status/1", html);
        assert_eq!(text, "the tweet text");
    }
}
