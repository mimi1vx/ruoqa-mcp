//! Query-string builder: drops `None` values and expands lists into repeated
//! keys, mirroring httpx's params encoding that `openqa-async` relied on.

use std::fmt::Display;

/// Prefix a REST endpoint with the openQA API root.
#[must_use]
pub fn api(path: &str) -> String {
    format!("/api/v1/{path}")
}

/// Accumulates query pairs, silently dropping `None` values.
#[derive(Default)]
pub struct Query {
    pairs: Vec<(&'static str, String)>,
}

impl Query {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `key=value` if `value` is `Some`; otherwise drop it.
    #[must_use]
    pub fn push(mut self, key: &'static str, value: Option<impl Display>) -> Self {
        if let Some(v) = value {
            self.pairs.push((key, v.to_string()));
        }
        self
    }

    /// Add one `key=id` pair per element, mirroring httpx's list expansion.
    #[must_use]
    pub fn push_all(mut self, key: &'static str, values: Option<&[i64]>) -> Self {
        if let Some(vs) = values {
            for v in vs {
                self.pairs.push((key, v.to_string()));
            }
        }
        self
    }

    /// Finish into `path`, or `path?a=b&...` if any pairs were pushed.
    #[must_use]
    pub fn finish(self, path: &str) -> String {
        if self.pairs.is_empty() {
            return path.to_string();
        }
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        for (k, v) in &self.pairs {
            serializer.append_pair(k, v);
        }
        format!("{path}?{}", serializer.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_none() {
        let q = Query::new()
            .push("state", Some("done"))
            .push("result", None::<&str>);
        assert_eq!(q.finish("/api/v1/jobs"), "/api/v1/jobs?state=done");
    }

    #[test]
    fn expands_list_into_repeated_keys() {
        let ids = [1, 2];
        let q = Query::new().push_all("ids", Some(&ids));
        assert_eq!(q.finish("/api/v1/jobs"), "/api/v1/jobs?ids=1&ids=2");
    }

    #[test]
    fn empty_query_has_no_question_mark() {
        let q = Query::new().push("state", None::<&str>);
        assert_eq!(q.finish("/api/v1/jobs"), "/api/v1/jobs");
    }

    #[test]
    fn percent_encodes_special_characters() {
        let q = Query::new().push("q", Some("a b+c&d"));
        assert_eq!(q.finish("/api/v1/search"), "/api/v1/search?q=a+b%2Bc%26d");
    }

    #[test]
    fn api_adds_leading_slash_prefix() {
        assert_eq!(api("jobs"), "/api/v1/jobs");
        assert_eq!(api("experimental/search"), "/api/v1/experimental/search");
    }
}
