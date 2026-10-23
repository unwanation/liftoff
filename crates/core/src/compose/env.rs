//! The `.env` of an environment (CMP-12, BND-07).

use std::collections::BTreeMap;

use crate::domain::{EnvName, Host, ImageTag, ProjectName};

/// The variables liftoff adds to every `.env`.
///
/// | Name              | Value                                          |
/// |-------------------|------------------------------------------------|
/// | `LIFTOFF_PROJECT` | project name                                   |
/// | `LIFTOFF_ENV`     | environment name                               |
/// | `LIFTOFF_TAG`     | the deployed image tag                         |
/// | `IMAGE_TAG`       | the same tag, the name compose files use       |
/// | `LIFTOFF_DOMAIN`  | the first bound host alphabetically, or empty  |
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiftoffVars {
    project: String,
    env: String,
    tag: String,
    domain: String,
}

impl LiftoffVars {
    /// Builds the variables. `bound_hosts` are the hosts of every domain
    /// bound to the environment, in any order (BND-07: the alphabetically first
    /// wins, so adding a binding can change `LIFTOFF_DOMAIN` only when the
    /// new host sorts before the old ones).
    #[must_use]
    pub fn new<'a>(
        project: &ProjectName,
        env: &EnvName,
        tag: &ImageTag,
        bound_hosts: impl IntoIterator<Item = &'a Host>,
    ) -> Self {
        Self {
            project: project.to_string(),
            env: env.to_string(),
            tag: tag.to_string(),
            domain: bound_hosts
                .into_iter()
                .min()
                .map(ToString::to_string)
                .unwrap_or_default(),
        }
    }

    /// `(name, value)` in the order they are written.
    #[must_use]
    pub fn pairs(&self) -> [(&'static str, &str); 5] {
        [
            ("LIFTOFF_PROJECT", &self.project),
            ("LIFTOFF_ENV", &self.env),
            ("LIFTOFF_TAG", &self.tag),
            ("IMAGE_TAG", &self.tag),
            ("LIFTOFF_DOMAIN", &self.domain),
        ]
    }
}

/// One `.env` line, read strictly.
enum Line<'a> {
    Skip,
    Assign(&'a str, &'a str),
}

/// Reads a line as blank, `#` comment or exactly `[export ]NAME=value`.
///
/// Anything else is an error: compose's dotenv accepts more forms (`KEY:
/// value`, `export<tab>`, inline comments, escapes) than liftoff models, and
/// a line liftoff misreads would let an image skip the permission check.
/// The value is unquoted without `"`, `'`, `#`, `\` and `$`, or sits whole
/// inside one pair of matching quotes with none of `\` and `$`.
fn read_line(line: &str) -> Result<Line<'_>, String> {
    if line.trim().is_empty() || line.starts_with('#') {
        return Ok(Line::Skip);
    }
    // Never echo the line: its value may be a secret.
    let bad = |why: &str| Err(why.to_owned());
    if line.chars().any(char::is_control) {
        return bad("control character");
    }
    let body = line.strip_prefix("export ").unwrap_or(line);
    let Some((name, value)) = body.split_once('=') else {
        return bad("expected NAME=value");
    };
    let mut chars = name.chars();
    let name_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !name_ok {
        return bad("invalid name");
    }
    let value = value.trim_end();
    let quoted = ['"', '\'']
        .into_iter()
        .find_map(|q| value.strip_prefix(q).map(|rest| (q, rest)));
    let inner = if let Some((q, rest)) = quoted {
        match rest.strip_suffix(q) {
            Some(inner) if !inner.contains(q) => inner,
            _ => return bad("unbalanced quote or text after the closing quote"),
        }
    } else if value.contains(['"', '\'', '#']) {
        return bad("quote or # in an unquoted value");
    } else {
        value
    };
    if inner.contains(['\\', '$']) {
        return bad("backslash or $ in a value");
    }
    Ok(Line::Assign(name, inner))
}

/// The variable name a `.env` line defines, if it is a valid assignment.
fn assigned_name(line: &str) -> Option<&str> {
    match read_line(line) {
        Ok(Line::Assign(name, _)) => Some(name),
        _ => None,
    }
}

/// Appends liftoff's variables to the user's `.env`.
///
/// The user's lines keep their order and text (comments and blank lines
/// too), except assignments to a variable liftoff sets, which are dropped
/// so the liftoff value wins. liftoff's lines follow, in the order of
/// [`LiftoffVars::pairs`]. Empty user input yields only liftoff's lines.
/// Every line ends with `\n`.
#[must_use]
pub fn merge_env(user_env: &str, vars: &LiftoffVars) -> String {
    let ours = vars.pairs();
    let mut out = String::new();
    for line in user_env.lines() {
        if assigned_name(line).is_some_and(|name| ours.iter().any(|(own, _)| *own == name)) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    for (name, value) in ours {
        out.push_str(name);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }
    out
}

/// Reads `NAME=value` lines into a map for interpolation.
///
/// The result feeds
/// [`ComposeFile::images_resolved`](super::ComposeFile::images_resolved).
/// Blank lines and `#` comments are skipped; a quoted value loses its
/// quotes; a later assignment wins. Fails closed: any other line is an
/// error (see the rules on [`merge_env`]'s input below), and the deploy must
/// refuse such an env. Call this on the user's `.env` before [`merge_env`].
///
/// A line is accepted when it is exactly `[export ]NAME=value` with NAME
/// matching `[A-Za-z_][A-Za-z0-9_]*`, and the value is either unquoted
/// without `"`, `'`, `#`, `\` and `$`, or wholly inside one matching pair of
/// quotes with no `\` or `$`.
///
/// # Errors
///
/// A message naming the number of the first line that does not fit and why.
/// It never contains the line itself: values may be secrets.
pub fn parse_env(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for (number, line) in text.lines().zip(1_usize..).map(|(line, n)| (n, line)) {
        let read =
            read_line(line).map_err(|why| format!("unsupported .env line {number} ({why})"))?;
        if let Line::Assign(name, value) = read {
            map.insert(name.to_owned(), value.to_owned());
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(hosts: &[&str]) -> LiftoffVars {
        let hosts: Vec<Host> = hosts.iter().map(|h| Host::new(h).unwrap()).collect();
        LiftoffVars::new(
            &ProjectName::new("acme").unwrap(),
            &EnvName::new("qa").unwrap(),
            &ImageTag::new("sha-1").unwrap(),
            &hosts,
        )
    }

    const OURS: &str = "LIFTOFF_PROJECT=acme
LIFTOFF_ENV=qa
LIFTOFF_TAG=sha-1
IMAGE_TAG=sha-1
LIFTOFF_DOMAIN=a.example.com
";

    mod merge_env {
        use super::*;

        // TC: v1 CMP-12
        #[test]
        fn liftoff_value_wins_when_user_sets_image_tag() {
            let out = merge_env("IMAGE_TAG=old\nFOO=1\n", &vars(&["a.example.com"]));
            assert_eq!(out, format!("FOO=1\n{OURS}"));
        }

        // TC: v1 CMP-12
        #[test]
        fn keeps_user_lines_in_order_when_they_do_not_clash() {
            let out = merge_env(
                "# comment\nZ=1\n\nA=2\nexport LIFTOFF_TAG=x\nLIFTOFF_DOMAIN=mine\nB=3",
                &vars(&["a.example.com"]),
            );
            assert_eq!(out, format!("# comment\nZ=1\n\nA=2\nB=3\n{OURS}"));
        }

        // TC: v1 CMP-12
        #[test]
        fn has_only_liftoff_variables_when_user_env_is_empty() {
            assert_eq!(merge_env("", &vars(&["a.example.com"])), OURS);
        }

        // TC: v1 CMP-12
        #[test]
        fn keeps_a_user_value_that_merely_contains_a_liftoff_name() {
            let out = merge_env(
                "NOT_IMAGE_TAG=1\nURL=x?IMAGE_TAG=2\n",
                &vars(&["a.example.com"]),
            );
            assert!(
                out.starts_with("NOT_IMAGE_TAG=1\nURL=x?IMAGE_TAG=2\n"),
                "{out}"
            );
        }
    }

    mod liftoff_vars {
        use super::*;

        // TC: v1 BND-07
        #[test]
        fn domain_is_the_alphabetically_first_host_when_several_are_bound() {
            let v = vars(&["z.example.com", "b.example.com", "m.example.com"]);
            assert_eq!(v.pairs()[4], ("LIFTOFF_DOMAIN", "b.example.com"));
        }

        // TC: v1 BND-07
        #[test]
        fn domain_is_empty_when_nothing_is_bound() {
            let out = merge_env("", &vars(&[]));
            assert!(out.ends_with("IMAGE_TAG=sha-1\nLIFTOFF_DOMAIN=\n"), "{out}");
        }
    }

    mod parse_env {
        use super::*;

        #[test]
        fn reads_assignments_when_text_has_comments_quotes_and_export() {
            let map = parse_env("# c\nA=1\nB=\"two words\"\nexport C='3'\n\nA=4\n").unwrap();
            let pairs: Vec<(&str, &str)> =
                map.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            assert_eq!(pairs, [("A", "4"), ("B", "two words"), ("C", "3")]);
        }

        // TC: v1 CMP-10
        #[rstest::rstest]
        #[case::colon("IMG: git.example.com/a/b:1")]
        #[case::export_tab("export\tIMG=git.example.com/a/b:1")]
        #[case::trailing_comment("IMG=\"git.example.com/a/b:1\" # c")]
        #[case::single_quote_comment("IMG='git.example.com/a/b:1'#x")]
        #[case::inline_comment("IMG=git.example.com/a/b:1 #c")]
        #[case::escape("IMG=\"a\\nb\"")]
        #[case::dollar("IMG=${A}")]
        #[case::unbalanced("IMG=\"abc")]
        #[case::no_equals("bad line")]
        #[case::spaced_name("IMG =x")]
        #[case::leading_space(" IMG=x")]
        fn rejects_when_line_is_not_plain_assignment(#[case] line: &str) {
            assert!(parse_env(line).is_err(), "{line}");
        }
    }
}
