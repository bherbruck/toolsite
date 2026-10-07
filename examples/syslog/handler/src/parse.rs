//! Syslog messages, as RFC 5424 and RFC 3164 (the older BSD format) put
//! them in a datagram. Senders follow either one loosely, so each part is
//! optional: what does not parse stays in the message, and a datagram with
//! no `<PRI>` at all is kept whole with no severity.

pub struct Entry {
    /// 0 kernel, 1 user, ... 23 local7.
    pub facility: Option<i64>,
    /// 0 emergency ... 7 debug. None when the datagram had no `<PRI>`.
    pub severity: Option<i64>,
    pub host: Option<String>,
    pub app: Option<String>,
    pub message: String,
}

type Fields = (Option<String>, Option<String>, String);

pub fn parse(datagram: &[u8]) -> Entry {
    let text = String::from_utf8_lossy(datagram);
    let text = text.trim_end_matches(['\r', '\n', '\0']);
    let Some((pri, rest)) = priority(text) else {
        return Entry { facility: None, severity: None, host: None, app: None, message: text.to_string() };
    };
    let (host, app, message) = rfc5424(rest).or_else(|| rfc3164(rest)).unwrap_or((None, None, rest.to_string()));
    Entry { facility: Some(pri / 8), severity: Some(pri % 8), host, app, message }
}

/// `<PRI>`: one to three digits, at most 191, which is facility * 8 +
/// severity.
fn priority(text: &str) -> Option<(i64, &str)> {
    let (digits, rest) = text.strip_prefix('<')?.split_once('>')?;
    if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pri: i64 = digits.parse().ok()?;
    (pri <= 191).then_some((pri, rest))
}

/// RFC 5424: `1 TIMESTAMP HOSTNAME APP-NAME PROCID MSGID STRUCTURED-DATA
/// MSG`, with `-` for a field the sender left out. The structured data is
/// skipped.
fn rfc5424(rest: &str) -> Option<Fields> {
    let mut fields = rest.strip_prefix("1 ")?.splitn(6, ' ');
    let (_timestamp, host, app, _procid, _msgid) =
        (fields.next()?, fields.next()?, fields.next()?, fields.next()?, fields.next()?);
    let message = skip_structured_data(fields.next()?)?;
    let message = message.strip_prefix(' ').unwrap_or(message);
    let message = message.strip_prefix('\u{feff}').unwrap_or(message);
    let given = |s: &str| (s != "-").then(|| s.to_string());
    Some((given(host), given(app), message.to_string()))
}

/// What follows the structured data: `-`, or one or more `[id k="v"]`
/// elements, where a value may hold `\]` and `\"`.
fn skip_structured_data(text: &str) -> Option<&str> {
    if let Some(rest) = text.strip_prefix('-') {
        return Some(rest);
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    while bytes.get(i) == Some(&b'[') {
        let mut quoted = false;
        loop {
            i += 1;
            match bytes.get(i)? {
                b'\\' if quoted => i += 1,
                b'"' => quoted = !quoted,
                b']' if !quoted => break,
                _ => {}
            }
        }
        i += 1;
    }
    (i > 0).then(|| &text[i..])
}

/// RFC 3164: `Mmm dd hh:mm:ss HOSTNAME TAG: MSG`. Many senders leave out
/// the host name, so a first word that looks like a tag is taken as one.
fn rfc3164(rest: &str) -> Option<Fields> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    // The timestamp is always 15 characters: "Oct  7 09:05:00".
    let stamp = rest.get(..15)?;
    if !MONTHS.iter().any(|m| stamp.starts_with(m)) || stamp.as_bytes()[9] != b':' {
        return None;
    }
    let rest = rest[15..].strip_prefix(' ')?;
    let (first, after) = rest.split_once(' ').unwrap_or((rest, ""));
    let (host, rest) = if first.ends_with(':') || first.contains('[') { (None, rest) } else { (Some(first.to_string()), after) };
    // The tag ends at '[' (a process id) or ':'.
    let (app, message) = match rest.split_once(": ") {
        Some((tag, message)) if !tag.is_empty() && !tag.contains(' ') => {
            (Some(tag.split('[').next().unwrap_or(tag).to_string()), message)
        }
        _ => (None, rest),
    };
    Some((host, app, message.to_string()))
}
