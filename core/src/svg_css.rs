//! Direct2D ignores `<style>` sheets but honors the `style` attribute, and editors (Illustrator,
//! Figma, ...) put all colors into classes. So the sheet's simple rules are copied into the
//! `style` attribute of every element they match before the document goes to Direct2D.
//!
//! Selectors understood: `*`, `tag`, `.class`, `#id`, `tag.class`, `tag#id` and lists of them;
//! rules with other selectors and `@` blocks are skipped.

use std::ops::Range;

struct Rule {
    selectors: Vec<Selector>,
    declarations: String,
}

#[derive(Default)]
struct Selector {
    tag: Option<String>,
    class: Option<String>,
    id: Option<String>,
}

impl Selector {
    fn parse(s: &str) -> Option<Selector> {
        let s = s.trim();
        if s.is_empty() || s.contains(|c: char| c.is_whitespace() || ">+~:[".contains(c)) {
            return None;
        }
        let mut sel = Selector::default();
        let split = s.find(['.', '#']).unwrap_or(s.len());
        let (tag, rest) = s.split_at(split);
        if !tag.is_empty() && tag != "*" {
            sel.tag = Some(tag.to_string());
        }
        if let Some(class) = rest.strip_prefix('.') {
            (!class.contains(['.', '#'])).then_some(())?;
            sel.class = Some(class.to_string());
        } else if let Some(id) = rest.strip_prefix('#') {
            (!id.contains(['.', '#'])).then_some(())?;
            sel.id = Some(id.to_string());
        }
        Some(sel)
    }

    fn specificity(&self) -> u32 {
        100 * self.id.is_some() as u32
            + 10 * self.class.is_some() as u32
            + self.tag.is_some() as u32
    }

    fn matches(&self, tag: &str, classes: &[&str], id: Option<&str>) -> bool {
        self.tag.as_deref().is_none_or(|t| t == tag)
            && self.class.as_deref().is_none_or(|c| classes.contains(&c))
            && self.id.as_deref().is_none_or(|i| id == Some(i))
    }
}

fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        rest = rest[start + 2..]
            .find("*/")
            .map_or("", |end| &rest[start + 2 + end + 2..]);
    }
    out.push_str(rest);
    out
}

fn parse_rules(css: &str) -> Vec<Rule> {
    let css = strip_comments(css);
    let mut rules = Vec::new();
    let mut rest = css.as_str();
    while let Some(open) = rest.find('{') {
        let head = rest[..open].trim();
        let body_start = open + 1;
        if head.starts_with('@') {
            // Skip the whole block, nested braces included.
            let mut depth = 1;
            let mut end = rest.len();
            for (i, c) in rest[body_start..].char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => continue,
                }
                if depth == 0 {
                    end = body_start + i + 1;
                    break;
                }
            }
            rest = &rest[end..];
            continue;
        }
        let close = rest[body_start..]
            .find('}')
            .map_or(rest.len(), |i| body_start + i);
        let declarations = rest[body_start..close].trim().replace('"', "'");
        let selectors: Option<Vec<Selector>> = head.split(',').map(Selector::parse).collect();
        if let Some(selectors) = selectors.filter(|_| !declarations.is_empty()) {
            rules.push(Rule {
                selectors,
                declarations,
            });
        }
        rest = &rest[(close + 1).min(rest.len())..];
    }
    rules
}

/// An attribute of a start tag: its name, its value and where the whole `name="value"` is.
struct Attr<'a> {
    name: &'a str,
    value: &'a str,
    span: Range<usize>,
}

/// The attributes of the start tag text `tag` (without `<` and `>`), after its name.
fn attributes(tag: &str) -> Vec<Attr<'_>> {
    let b = tag.as_bytes();
    let mut i = tag
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(tag.len());
    let mut attrs = Vec::new();
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        if i == start {
            return attrs;
        }
        let name = &tag[start..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let Some(&quote) = b.get(i).filter(|&&q| q == b'"' || q == b'\'') else {
            return attrs;
        };
        let Some(len) = tag[i + 1..].find(quote as char) else {
            return attrs;
        };
        let value = &tag[i + 1..i + 1 + len];
        i += len + 2;
        attrs.push(Attr {
            name,
            value,
            span: start..i,
        });
    }
}

/// The text of every `<style>` element (CDATA markers removed).
fn style_sheets(xml: &str) -> String {
    let mut css = String::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<style") {
        let Some(open_end) = rest[start..].find('>') else {
            break;
        };
        let body = start + open_end + 1;
        let Some(len) = rest[body..].find("</style") else {
            break;
        };
        let text = &rest[body..body + len];
        css.push_str(&text.replace("<![CDATA[", "").replace("]]>", ""));
        css.push('\n');
        rest = &rest[body + len..];
    }
    css
}

/// `xml` with the `<style>` rules copied into `style` attributes; `None` when there is nothing to
/// do (no sheet, not UTF-8).
pub fn inline(xml: &[u8]) -> Option<Vec<u8>> {
    let xml = std::str::from_utf8(xml).ok()?;
    let rules = parse_rules(&style_sheets(xml));
    if rules.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(xml.len() + xml.len() / 4);
    let mut i = 0;
    while let Some(lt) = xml[i..].find('<').map(|p| i + p) {
        out.push_str(&xml[i..lt]);
        let rest = &xml[lt..];
        // Comments, CDATA, declarations, processing instructions and end tags are copied as is.
        let skip_to = if rest.starts_with("<!--") {
            rest.find("-->").map(|e| e + 3)
        } else if rest.starts_with("<![CDATA[") {
            rest.find("]]>").map(|e| e + 3)
        } else if rest.starts_with("<!") || rest.starts_with("<?") || rest.starts_with("</") {
            rest.find('>').map(|e| e + 1)
        } else {
            None
        };
        if let Some(len) = skip_to {
            out.push_str(&rest[..len]);
            i = lt + len;
            continue;
        }
        let Some(end) = tag_end(rest) else {
            out.push_str(rest);
            return Some(out.into_bytes());
        };
        out.push_str(&styled_tag(&rest[1..end], &rules));
        i = lt + end;
    }
    out.push_str(&xml[i..]);
    Some(out.into_bytes())
}

/// Index of the `>` closing the start tag at the beginning of `s` (quotes respected).
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

/// `<` + the tag text with a `style` attribute holding the matching declarations (the element's
/// own `style` last, so it keeps precedence).
fn styled_tag(tag: &str, rules: &[Rule]) -> String {
    let name_len = tag
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(tag.len());
    let name = &tag[..name_len];
    let local_name = name.rsplit(':').next().unwrap_or(name);
    let attrs = attributes(tag);
    let find = |n: &str| attrs.iter().find(|a| a.name == n);
    let classes: Vec<&str> =
        find("class").map_or(Vec::new(), |a| a.value.split_whitespace().collect());
    let id = find("id").map(|a| a.value);

    let mut matched: Vec<(u32, usize, &str)> = Vec::new();
    for (order, rule) in rules.iter().enumerate() {
        let best = rule
            .selectors
            .iter()
            .filter(|s| s.matches(local_name, &classes, id))
            .map(Selector::specificity)
            .max();
        if let Some(specificity) = best {
            matched.push((specificity, order, rule.declarations.as_str()));
        }
    }
    if matched.is_empty() {
        return format!("<{tag}");
    }
    matched.sort();
    let mut style: Vec<&str> = matched.iter().map(|m| m.2.trim_end_matches(';')).collect();
    let own = find("style");
    if let Some(own) = own {
        style.push(own.value);
    }
    // Written in double quotes: a value that was in single ones may hold them (font names).
    let style = style.join(";").replace('"', "'");
    match own {
        Some(own) => format!(
            "<{}style=\"{}\"{}",
            &tag[..own.span.start],
            style,
            &tag[own.span.end..]
        ),
        None => format!("<{name} style=\"{style}\"{}", &tag[name_len..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(xml: &str) -> String {
        String::from_utf8(inline(xml.as_bytes()).unwrap()).unwrap()
    }

    #[test]
    fn class_rules_become_style_attributes() {
        let xml = r#"<svg><style type="text/css">
            .st0{fill:#F5F5F5;} /* comment */
            .a, rect.b {stroke:red}
            @media print { .st0 { fill: black } }
            g .deep { fill: blue }
        </style><path class="st0" d="M0 0"/><rect class="b"/><circle class="a st0" style="opacity:.5"/></svg>"#;
        let out = run(xml);
        assert!(
            out.contains(r#"<path style="fill:#F5F5F5" class="st0" d="M0 0"/>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<rect style="stroke:red" class="b"/>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<circle class="a st0" style="fill:#F5F5F5;stroke:red;opacity:.5"/>"#),
            "{out}"
        );
        assert!(!out.contains("fill: blue\""));
    }

    #[test]
    fn id_beats_class_beats_tag() {
        let xml = r#"<svg><style><![CDATA[#x{fill:red} .c{fill:green} rect{fill:blue}]]></style><rect id="x" class="c"/></svg>"#;
        let out = run(xml);
        assert!(
            out.contains(r#"<rect style="fill:blue;fill:green;fill:red" id="x" class="c"/>"#),
            "{out}"
        );
    }

    #[test]
    fn own_style_in_single_quotes_stays_well_formed() {
        let xml =
            r#"<svg><style>.t{fill:red}</style><g class="t" style='font-family:"Arial"'/></svg>"#;
        assert!(
            run(xml).contains(r#"<g class="t" style="fill:red;font-family:'Arial'"/>"#),
            "{}",
            run(xml)
        );
    }

    #[test]
    fn nothing_to_do_without_a_sheet() {
        assert!(inline(b"<svg><rect class='a'/></svg>").is_none());
    }
}
