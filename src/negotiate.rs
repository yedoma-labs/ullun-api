//! Content negotiation (`Accept`, `Accept-Language`)
//!
//! ```
//! use ullun::negotiate::{best, best_language};
//!
//! let accept = "text/html;q=0.5, application/json, */*;q=0.1";
//! assert_eq!(best(accept, &["text/html", "application/json"]), Some("application/json"));
//! assert_eq!(best("text/*", &["application/json", "text/csv"]), Some("text/csv"));
//! assert_eq!(best("image/png", &["application/json"]), None);
//!
//! assert_eq!(best_language("de-CH, de;q=0.8, en;q=0.5", &["en", "de"]), Some("de"));
//! ```
//!
//! In handlers use [`Request::accepts`](crate::request::Request::accepts) and
//! [`Request::accepts_language`](crate::request::Request::accepts_language).
//! A missing or empty header means "anything": the first offer wins. Answer `406`
//! (or fall back to a default) when nothing matches.

struct Range<'a> {
    kind: &'a str,
    sub: &'a str,
    q: u16, // thousandths
}

fn parse_q(params: &str) -> u16 {
    params
        .split(';')
        .filter_map(|p| {
            p.trim()
                .strip_prefix("q=")
                .or_else(|| p.trim().strip_prefix("Q="))
        })
        .filter_map(|v| v.trim().parse::<f32>().ok())
        .next()
        .map(|q| (q.clamp(0.0, 1.0) * 1000.0).round() as u16)
        .unwrap_or(1000)
}

fn ranges(header: &str) -> Vec<Range<'_>> {
    header
        .split(',')
        .filter_map(|item| {
            let (mime, params) = item.split_once(';').unwrap_or((item, ""));
            let (kind, sub) = mime.trim().split_once('/')?;
            Some(Range {
                kind,
                sub,
                q: parse_q(params),
            })
        })
        .collect()
}

/// The best of `offered` for an `Accept` header value, `None` if none is acceptable
pub fn best<'a>(accept: &str, offered: &[&'a str]) -> Option<&'a str> {
    if accept.trim().is_empty() {
        return offered.first().copied();
    }
    let ranges = ranges(accept);
    let mut winner: Option<(&'a str, u16)> = None;
    for &offer in offered {
        let Some((kind, sub)) = offer.split_once('/') else {
            continue;
        };
        // the most specific matching range decides this offer's quality
        let q = ranges
            .iter()
            .filter(|r| {
                (r.kind == "*" && r.sub == "*")
                    || (r.kind.eq_ignore_ascii_case(kind)
                        && (r.sub == "*" || r.sub.eq_ignore_ascii_case(sub)))
            })
            .max_by_key(|r| usize::from(r.kind != "*") + usize::from(r.sub != "*"))
            .map(|r| r.q);
        match q {
            Some(q) if q > 0 && winner.is_none_or(|(_, best_q)| q > best_q) => {
                winner = Some((offer, q));
            }
            _ => {}
        }
    }
    winner.map(|(offer, _)| offer)
}

/// The best of `offered` for an `Accept-Language` header value. A range
/// matches an offer when it equals it or is a prefix up to a `-` (`en` matches
/// `en-GB`, and `en-GB` matches an offer of `en`).
pub fn best_language<'a>(accept_language: &str, offered: &[&'a str]) -> Option<&'a str> {
    if accept_language.trim().is_empty() {
        return offered.first().copied();
    }
    let ranges: Vec<(String, u16)> = accept_language
        .split(',')
        .filter_map(|item| {
            let (tag, params) = item.split_once(';').unwrap_or((item, ""));
            let tag = tag.trim();
            (!tag.is_empty()).then(|| (tag.to_ascii_lowercase(), parse_q(params)))
        })
        .collect();
    let mut winner: Option<(&'a str, u16)> = None;
    for &offer in offered {
        let o = offer.to_ascii_lowercase();
        // exact match beats prefix match beats wildcard; that range sets the quality
        let q = ranges
            .iter()
            .filter_map(|(tag, q)| {
                let rank = if *tag == o {
                    3
                } else if tag
                    .strip_prefix(o.as_str())
                    .is_some_and(|r| r.starts_with('-'))
                    || o.strip_prefix(tag.as_str())
                        .is_some_and(|r| r.starts_with('-'))
                {
                    2
                } else if tag == "*" {
                    1
                } else {
                    return None;
                };
                Some((rank, *q))
            })
            .max_by_key(|(rank, _)| *rank)
            .map(|(_, q)| q);
        match q {
            Some(q) if q > 0 && winner.is_none_or(|(_, best_q)| q > best_q) => {
                winner = Some((offer, q));
            }
            _ => {}
        }
    }
    winner.map(|(offer, _)| offer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_and_specificity() {
        let offers = ["text/html", "application/json"];
        assert_eq!(
            best("text/html, application/json;q=0.9", &offers),
            Some("text/html")
        );
        assert_eq!(
            best("text/html;q=0.1, application/json;q=0.9", &offers),
            Some("application/json")
        );
        // a specific range overrides the wildcard that would also match
        assert_eq!(
            best("application/json;q=0, */*", &offers),
            Some("text/html")
        );
        assert_eq!(best("text/html;q=0, application/json;q=0", &offers), None);
        assert_eq!(best("text/*;q=0.5, */*;q=0.1", &offers), Some("text/html"));
    }

    #[test]
    fn missing_header_takes_the_first_offer() {
        assert_eq!(best("", &["a/b", "c/d"]), Some("a/b"));
        assert_eq!(best("  ", &[]), None);
        assert_eq!(best_language("", &["en", "de"]), Some("en"));
    }

    #[test]
    fn ties_keep_server_preference() {
        assert_eq!(
            best("*/*", &["application/json", "text/html"]),
            Some("application/json")
        );
    }

    #[test]
    fn junk_is_ignored() {
        assert_eq!(
            best("garbage, ;;;, text/plain;q=abc", &["text/plain"]),
            Some("text/plain")
        );
        assert_eq!(best("text/html;q=7", &["text/html"]), Some("text/html"));
    }

    #[test]
    fn languages() {
        assert_eq!(best_language("en-US,en;q=0.9", &["de", "en"]), Some("en"));
        assert_eq!(
            best_language("fr;q=0.2, de;q=0.9", &["fr", "de"]),
            Some("de")
        );
        assert_eq!(best_language("de-CH", &["de"]), Some("de"));
        assert_eq!(best_language("de", &["de-AT"]), Some("de-AT"));
        assert_eq!(best_language("*;q=0.1", &["pt"]), Some("pt"));
        assert_eq!(best_language("ja", &["de", "en"]), None);
        assert_eq!(best_language("de;q=0, *;q=0.5", &["de"]), None); // explicit refusal wins
        assert_eq!(best_language("de;q=0, *;q=0.5", &["de", "fr"]), Some("fr"));
    }
}
