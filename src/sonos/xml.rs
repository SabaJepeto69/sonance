//! Just enough XML for Sonos: its documents are small, flat and predictable,
//! so a few string scanners beat pulling in a full parser.

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

pub fn unescape(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

/// Finds the next `<name` that is really that element (not `<nameSuffix`).
fn find_open(xml: &str, name: &str, from: usize) -> Option<usize> {
    let pat = format!("<{name}");
    let mut pos = from;
    while let Some(i) = xml[pos..].find(&pat) {
        let at = pos + i;
        match xml[at + pat.len()..].chars().next() {
            Some(' ' | '>' | '/' | '\n' | '\t' | '\r') => return Some(at),
            _ => pos = at + pat.len(),
        }
    }
    None
}

/// Every `<name ...>...</name>` (or self-closing) element, as raw text.
pub fn elements<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let close = format!("</{name}>");
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(start) = find_open(xml, name, pos) {
        let Some(gt) = xml[start..].find('>') else { break };
        let open_end = start + gt + 1;
        if xml[..open_end].ends_with("/>") {
            out.push(&xml[start..open_end]);
            pos = open_end;
        } else if let Some(c) = xml[open_end..].find(&close) {
            let end = open_end + c + close.len();
            out.push(&xml[start..end]);
            pos = end;
        } else {
            break;
        }
    }
    out
}

/// Raw inner text of the first `name` element.
pub fn tag_raw<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let el = *elements(xml, name).first()?;
    if el.ends_with("/>") && !el.contains(&format!("</{name}>")) {
        return Some("");
    }
    let inner_start = el.find('>')? + 1;
    let inner_end = el.len() - name.len() - 3;
    Some(&el[inner_start..inner_end])
}

/// Unescaped inner text of the first `name` element.
pub fn tag(xml: &str, name: &str) -> Option<String> {
    tag_raw(xml, name).map(unescape)
}

/// Unescaped attribute from an element's opening tag.
pub fn attr(el: &str, name: &str) -> Option<String> {
    let open = &el[..el.find('>').unwrap_or(el.len())];
    let pat = format!(" {name}=\"");
    let i = open.find(&pat)? + pat.len();
    let j = open[i..].find('"')?;
    Some(unescape(&open[i..i + j]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans() {
        let x = r#"<a><ZoneGroups><ZoneGroup ID="x&amp;y"><M n="1"/><M n="2"></M></ZoneGroup></ZoneGroups><t>a&lt;b</t><e/></a>"#;
        let g = elements(x, "ZoneGroup");
        assert_eq!(g.len(), 1);
        assert_eq!(attr(g[0], "ID").unwrap(), "x&y");
        assert_eq!(elements(g[0], "M").len(), 2);
        assert_eq!(tag(x, "t").unwrap(), "a<b");
        assert_eq!(tag(x, "e").unwrap(), "");
        assert!(tag(x, "zz").is_none());
    }
}
