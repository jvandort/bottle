//! Words for a POSIX shell: what folio writes into the scripts it runs, in bottles and on runners.

/// `s` as one word for a shell: as it is, if nothing in it is special to one; else in single
/// quotes, each of its own closed, escaped and reopened.
pub fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn a_word_is_quoted_only_if_it_must_be() {
        assert_eq!(
            quote("plain/path-1.0:a=b,c@d%e+f"),
            "plain/path-1.0:a=b,c@d%e+f"
        );
        assert_eq!(quote(""), "''");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("$HOME;rm"), "'$HOME;rm'");
    }
}
