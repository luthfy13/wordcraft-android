//! What the panic hook writes to logcat, as the desktop app's (`apps/wordcraft/src/logging.rs`): where it
//! happened, the message without the text it quotes (that can be the user's writing), and the backtrace.

/// Replaces text a panic message quotes between backticks with `…`: std quotes the string it was slicing
/// (`byte index 5 is not a char boundary; … of `<the string>``). An unpaired backtick leaves the rest out.
pub fn redact_quoted(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for (i, part) in message.split('`').enumerate() {
        if i % 2 == 1 {
            out.push_str("`…");
        } else {
            if i > 0 {
                out.push('`');
            }
            out.push_str(part);
        }
    }
    out
}

/// The first line of the report: `panic at <file:line>: <redacted message>`.
pub fn headline(location: Option<&str>, message: Option<&str>) -> String {
    let message = message.map_or_else(|| "(no message)".to_owned(), redact_quoted);
    format!("panic at {}: {message}", location.unwrap_or("an unknown place"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_text_is_redacted() {
        assert_eq!(
            redact_quoted("byte index 5 is not a char boundary; it is inside 'é' of `Dear Ann`"),
            "byte index 5 is not a char boundary; it is inside 'é' of `…`"
        );
        assert_eq!(redact_quoted("no quotes"), "no quotes");
        assert_eq!(redact_quoted("unpaired `secret"), "unpaired `…");
        assert_eq!(headline(Some("src/a.rs:1:2"), Some("boom")), "panic at src/a.rs:1:2: boom");
        assert_eq!(headline(None, None), "panic at an unknown place: (no message)");
    }
}
