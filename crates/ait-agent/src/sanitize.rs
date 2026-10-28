//! The reply-sanitization pure seam (Spec R04, user stories 5, 6, 28):
//! model text → command bytes. Cooked-mode line discipline makes a
//! newline an execution, so the hard guarantee — no `\n`/`\r` in the
//! output — is enforced at this testable seam rather than by
//! convention, and the frontend can stream the result into the input
//! line knowing it can never self-execute (§5.4).

/// Collapse a model reply to one command line: drop code-fence lines
/// and a leading `$ ` prompt marker, trim, then keep only the text
/// before the first `\n` or `\r`. Whatever a chatty or fenced reply
/// carries, the returned bytes hold no line break — the never-execute
/// guarantee behind user story 5.
pub fn sanitize_reply(reply: &str) -> String {
    let mut text = reply.trim();
    // An opening fence line is ``` plus an optional language tag.
    if text.starts_with("```") {
        text = match text.find('\n') {
            Some(end) => text[end + 1..].trim_start(),
            None => "",
        };
    }
    // A closing fence is a trailing ``` on a line of its own — a bare
    // ``` suffix glued to text ("ls```") is not a fence.
    if let Some(body) = text.strip_suffix("```") {
        if body.is_empty() || body.ends_with('\n') {
            text = body.trim_end();
        }
    }
    let text = text.trim_start();
    let text = text.strip_prefix("$ ").unwrap_or(text);
    let end = text.find(['\n', '\r']).unwrap_or(text.len());
    text[..end].trim().to_owned()
}
