//! Owning form builder for `Client::request_form`, which takes `&[(&str, &str)]`.
//! Numeric/owned values need somewhere to live across the call.

use std::fmt::Display;

#[derive(Default)]
pub struct Form(Vec<(String, String)>);

impl Form {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn push(mut self, key: &str, value: impl Display) -> Self {
        self.0.push((key.to_string(), value.to_string()));
        self
    }

    #[must_use]
    pub fn push_opt(self, key: &str, value: Option<impl Display>) -> Self {
        match value {
            Some(v) => self.push(key, v),
            None => self,
        }
    }

    #[must_use]
    pub fn push_all(mut self, key: &str, values: &[i64]) -> Self {
        for v in values {
            self.0.push((key.to_string(), v.to_string()));
        }
        self
    }

    #[must_use]
    pub fn pairs(&self) -> Vec<(&str, &str)> {
        self.0
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_preserve_insertion_order() {
        let form = Form::new()
            .push("text", "hi")
            .push_opt("prio", Some(40))
            .push_opt("dup_type_auto", None::<i64>);
        assert_eq!(form.pairs(), vec![("text", "hi"), ("prio", "40")]);
    }

    #[test]
    fn push_all_repeats_key() {
        let form = Form::new().push_all("jobs", &[1, 2]);
        assert_eq!(form.pairs(), vec![("jobs", "1"), ("jobs", "2")]);
    }
}
