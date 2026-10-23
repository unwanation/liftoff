//! Compose variable interpolation, as far as images need it.

use std::collections::BTreeMap;

/// Substitutes `$$`, `$NAME`, `${NAME}`, `${NAME:-default}` and
/// `${NAME-default}` the way `docker compose` does: an unset variable
/// becomes empty (or the default); `:-` also applies the default to an
/// empty value.
///
/// Fails closed: anything else compose might expand (`${NAME:?err}`,
/// `${NAME:+alt}`, nested defaults, a lone `$`, an unclosed `${`) is an
/// error, as is a substituted value or default that itself contains `$`
/// (compose's dotenv may expand it, this function does not).
pub(super) fn interpolate(text: &str, vars: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, tail)) = rest.split_once('$') {
        out.push_str(before);
        let (expanded, used) = expand(tail, vars)?;
        out.push_str(&expanded);
        rest = tail.get(used..).unwrap_or_default();
    }
    out.push_str(rest);
    Ok(out)
}

fn unsupported(text: &str) -> String {
    format!("unsupported variable syntax near \"${text}\"")
}

/// A substituted value must be final text.
fn plain(value: &str) -> Result<String, String> {
    if value.contains('$') {
        Err("a variable value containing \"$\" cannot be resolved".to_owned())
    } else {
        Ok(value.to_owned())
    }
}

/// Expands what follows a `$`; returns the text and how many bytes of
/// `tail` it consumed.
fn expand(tail: &str, vars: &BTreeMap<String, String>) -> Result<(String, usize), String> {
    if tail.starts_with('$') {
        return Ok(("$".to_owned(), 1));
    }
    if let Some(braced) = tail.strip_prefix('{') {
        let end = braced.find('}').ok_or_else(|| unsupported(tail))?;
        let inner = braced.get(..end).unwrap_or_default();
        return Ok((braced_value(inner, vars)?, end.saturating_add(2)));
    }
    let len = name_len(tail);
    if len == 0 {
        return Err(unsupported(tail));
    }
    let name = tail.get(..len).unwrap_or_default();
    Ok((plain(vars.get(name).map_or("", String::as_str))?, len))
}

fn name_len(text: &str) -> usize {
    let mut chars = text.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return 0,
    }
    chars
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map_or(text.len(), |(i, _)| i)
}

fn braced_value(inner: &str, vars: &BTreeMap<String, String>) -> Result<String, String> {
    let len = name_len(inner);
    let name = inner.get(..len).unwrap_or_default();
    let modifier = inner.get(len..).unwrap_or_default();
    let (default, empty_counts) = if modifier.is_empty() {
        (None, false)
    } else if let Some(default) = modifier.strip_prefix(":-") {
        (Some(default), true)
    } else if let Some(default) = modifier.strip_prefix('-') {
        (Some(default), false)
    } else {
        return Err(unsupported(&format!("{{{inner}}}")));
    };
    if len == 0 {
        return Err(unsupported(&format!("{{{inner}}}")));
    }
    let value = match (vars.get(name), default) {
        (Some(value), Some(default)) if value.is_empty() && empty_counts => default,
        (Some(value), _) => value,
        (None, Some(default)) => default,
        (None, None) => "",
    };
    plain(value)
}

#[cfg(test)]
#[expect(
    clippy::literal_string_with_formatting_args,
    reason = "compose variables look like format arguments"
)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn vars() -> BTreeMap<String, String> {
        [("A", "1"), ("EMPTY", "")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    #[rstest]
    #[case::braced("x:${A}", "x:1")]
    #[case::bare("x:$A.y", "x:1.y")]
    #[case::escaped("cost $$5", "cost $5")]
    #[case::unset("x:${NOPE}", "x:")]
    #[case::default_for_unset("x:${NOPE:-d}", "x:d")]
    #[case::default_for_empty_with_colon("x:${EMPTY:-d}", "x:d")]
    #[case::empty_kept_without_colon("x:${EMPTY-d}", "x:")]
    fn substitutes_like_compose_when_variable_syntax(#[case] text: &str, #[case] expected: &str) {
        assert_eq!(interpolate(text, &vars()).as_deref(), Ok(expected));
    }

    #[rstest]
    #[case::required("${A:?x}")]
    #[case::required_no_colon("${A?x}")]
    #[case::alternative("${A:+alt}")]
    #[case::alternative_no_colon("${A+alt}")]
    #[case::nested_default("${NOPE:-${A}}")]
    #[case::value_with_dollar("${REF}")]
    #[case::default_with_dollar("${NOPE:-$A}")]
    #[case::lone_dollar("a $ b")]
    #[case::unclosed("a ${A")]
    #[case::nameless("${}")]
    fn fails_when_variable_syntax_is_not_understood(#[case] text: &str) {
        let mut vars = vars();
        vars.insert("REF".to_owned(), "${A}".to_owned());
        assert!(interpolate(text, &vars).is_err());
    }
}
