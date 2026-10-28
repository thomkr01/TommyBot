//! Discord markdown: the toolbar's text edits and the preview renderer.

/// Wraps the UTF-16 selection `a..b` of `v` in `open`/`close`, or unwraps it if it already is (Discord's Ctrl+B).
/// Returns the new text and the new selection.
pub fn wrap(v: &str, a: usize, b: usize, open: &str, close: &str) -> (String, usize, usize) {
    let u: Vec<u16> = v.encode_utf16().collect();
    let (o, c): (Vec<u16>, Vec<u16>) = (open.encode_utf16().collect(), close.encode_utf16().collect());
    let b = b.min(u.len());
    let a = a.min(b);
    let (out, shift) = if u[..a].ends_with(&o) && u[b..].starts_with(&c) {
        ([&u[..a - o.len()], &u[a..b], &u[b + c.len()..]].concat(), -(o.len() as isize))
    } else {
        ([&u[..a], &o[..], &u[a..b], &c[..], &u[b..]].concat(), o.len() as isize)
    };
    (String::from_utf16_lossy(&out), a.saturating_add_signed(shift), b.saturating_add_signed(shift))
}

/// Toggles `p` at the start of the line holding the cursor.
pub fn line(v: &str, a: usize, b: usize, p: &str) -> (String, usize, usize) {
    let u: Vec<u16> = v.encode_utf16().collect();
    let pu: Vec<u16> = p.encode_utf16().collect();
    let b = b.min(u.len());
    let a = a.min(b);
    let s = u[..a].iter().rposition(|&c| c == b'\n' as u16).map_or(0, |i| i + 1);
    let (out, shift) = if u[s..].starts_with(&pu) {
        ([&u[..s], &u[s + pu.len()..]].concat(), -(pu.len() as isize))
    } else {
        ([&u[..s], &pu[..], &u[s..]].concat(), pu.len() as isize)
    };
    (String::from_utf16_lossy(&out), a.saturating_add_signed(shift).max(s), b.saturating_add_signed(shift).max(s))
}

pub fn esc(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }

/// Turns `mk`text`mk` pairs into `<tag>`; an unmatched marker stays as text.
fn pairs(s: &str, mk: &str, tag: &str) -> String {
    let (mut out, mut rest) = (String::new(), s);
    while let Some(a) = rest.find(mk) {
        let after = &rest[a + mk.len()..];
        let Some(b) = after.find(mk).filter(|&b| b > 0) else { break };
        out += &format!("{}<{tag}>{}</{}>", &rest[..a], &after[..b], tag.split(' ').next().unwrap_or(tag));
        rest = &after[b + mk.len()..];
    }
    out + rest
}

// shortcut: no nesting rules (`***x***`, `snake_case` italics); swap in a real parser if previews look wrong.
fn emph(s: &str) -> String {
    [("**", "b"), ("__", "u"), ("~~", "s"), ("||", "span class=\"spoiler\""), ("*", "i"), ("_", "i")]
        .iter().fold(s.to_string(), |s, (mk, tag)| pairs(&s, mk, tag))
}

/// Discord's inline markdown as HTML; `s` must already be escaped.
pub fn inline(s: &str) -> String {
    // code spans and links are stashed first so the emphasis passes leave their insides (and URLs) alone
    let (mut out, mut kept, mut rest) = (String::new(), Vec::new(), s);
    while let Some(i) = rest.find(['`', '[']) {
        let t = &rest[i..];
        let hit = if let Some(c) = t.strip_prefix('`') {
            c.find('`').filter(|&n| n > 0).map(|n| (n + 2, format!("<code>{}</code>", &c[..n])))
        } else {
            t.find("](").and_then(|m| t[m + 2..].find(')').map(|e| (m, &t[m + 2..m + 2 + e])))
                .filter(|(_, u)| u.starts_with("https://") || u.starts_with("http://"))
                .map(|(m, u)| (m + 3 + u.len(), format!("<a href=\"{u}\" target=\"_blank\" rel=\"noopener\">{}</a>", emph(&t[1..m]))))
        };
        out += &rest[..i];
        match hit {
            Some((n, html)) => { out += &format!("\u{1}{}\u{1}", kept.len()); kept.push(html); rest = &t[n..]; }
            None => { out += &t[..1]; rest = &t[1..]; }
        }
    }
    let mut out = emph(&(out + rest));
    for (k, h) in kept.iter().enumerate() { out = out.replace(&format!("\u{1}{k}\u{1}"), h); }
    out
}

/// The markdown Discord renders in embed descriptions and field values, as escaped preview HTML.
pub fn md(s: &str) -> String {
    let e = esc(s);
    let parts: Vec<&str> = e.split("```").collect();
    parts.iter().enumerate().map(|(i, part)| {
        if i % 2 == 1 && i + 1 < parts.len() {
            // a first line without spaces is the language tag
            let body = part.split_once('\n').filter(|(l, _)| !l.contains(' ')).map_or(*part, |(_, b)| b);
            return format!("<pre>{}</pre>", body.trim_end_matches('\n'));
        }
        let part = if i % 2 == 1 { format!("```{part}") } else { part.strip_prefix('\n').filter(|_| i > 0).unwrap_or(part).to_string() };
        if part.is_empty() { return part }
        part.split('\n').map(|l| {
            let (cls, l) = [("# ", "h1"), ("## ", "h2"), ("### ", "h3"), ("-# ", "sub"), ("&gt; ", "q"), ("- ", "li"), ("* ", "li")]
                .iter().find_map(|(p, c)| l.strip_prefix(p).map(|r| (*c, r))).unwrap_or(("", l));
            format!("<div class=\"{cls}\">{}</div>", if l.is_empty() { "<br>".into() } else { inline(l) })
        }).collect::<String>()
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_toggles_like_discord() {
        assert_eq!(wrap("hi you", 3, 6, "**", "**"), ("hi **you**".into(), 5, 8));
        assert_eq!(wrap("hi **you**", 5, 8, "**", "**"), ("hi you".into(), 3, 6));
        // offsets are UTF-16, like the browser's selection
        assert_eq!(wrap("😀 a", 3, 4, "_", "_"), ("😀 _a_".into(), 4, 5));
        assert_eq!(line("a\nb", 2, 3, "> "), ("a\n> b".into(), 4, 5));
        assert_eq!(line("a\n> b", 4, 5, "> "), ("a\nb".into(), 2, 3));
    }

    #[test]
    fn md_renders_discord_markdown() {
        assert_eq!(inline("**b** *i* __u__ ~~s~~ ||x||"), "<b>b</b> <i>i</i> <u>u</u> <s>s</s> <span class=\"spoiler\">x</span>");
        assert_eq!(inline("`**a**` [x_y](https://e.com/a_b_c)"), "<code>**a**</code> <a href=\"https://e.com/a_b_c\" target=\"_blank\" rel=\"noopener\">x_y</a>");
        assert_eq!(inline("[x](javascript:alert(1))"), "[x](javascript:alert(1))");
        assert_eq!(md("<b>\n> q"), "<div class=\"\">&lt;b&gt;</div><div class=\"q\">q</div>");
        assert_eq!(md("```rs\nlet *a*;\n```"), "<pre>let *a*;</pre>");
        assert_eq!(md("## H"), "<div class=\"h2\">H</div>");
    }
}
