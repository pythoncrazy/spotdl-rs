//! Text from HTML pages.

/// The text between `start` and `end`, without tags, `<br>` as newlines, and excluded `<div>`s dropped.
pub(crate) fn html(page: &str, start: &str, end: &str) -> Option<String> {
    let region = page.split_once(start)?.1.split(end).next()?;
    let (mut skip, mut out) = (0, String::new());
    for (i, piece) in region.split('<').enumerate() {
        let (tag, text) = match i {
            0 => ("", piece.split_once('>').map_or(piece, |x| x.1)),
            _ => piece.split_once('>').unwrap_or((piece, "")),
        };
        skip = match tag {
            t if t.contains("data-exclude-from-selection") => skip + 1,
            t if skip > 0 && t.starts_with("div") => skip + 1,
            "/div" if skip > 0 => skip - 1,
            _ => skip,
        };
        if skip == 0 {
            out.push_str(if tag.starts_with("br") { "\n" } else { "" });
            out.extend(text.chars().filter(|c| !matches!(c, '\n' | '\r')));
        }
    }
    Some(entities(&out))
}

/// `text` with HTML entities decoded in one pass, so an escaped entity ("&amp;lt;") stays text ("&lt;"). Genius
/// escapes `<`, `>`, `&`, `"` and `'`; azlyrics also uses `&#39;`-style numbers and `&nbsp;`.
fn entities(text: &str) -> String {
    let (mut out, mut rest) = (String::with_capacity(text.len()), text);
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let entity = rest.split_once(';').filter(|(name, _)| name.len() <= 8).and_then(|(name, after)| {
            let char = match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                // A plain space: a no-break space in a tag looks the same but breaks word matching and search.
                "nbsp" => ' ',
                _ => {
                    let number = name.strip_prefix('#')?;
                    let code = match number.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => number.parse().ok()?,
                    };
                    char::from_u32(code)?
                }
            };
            Some((char, after))
        });
        if let Some((char, after)) = entity {
            out.push(char);
            rest = after;
        } else {
            out.push('&');
        }
    }
    out + rest
}
