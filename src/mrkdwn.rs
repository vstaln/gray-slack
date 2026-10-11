//! Markdown -> Slack mrkdwn, and the 4k text chunker.
//!
//! Slack mrkdwn is not CommonMark: `*bold*`, `_italic_`, `~strike~`,
//! `<url|label>` links, `• ` bullets, no headings, no pipe tables. The
//! converter is line-oriented and fenced-code aware: ``` blocks pass
//! through untouched (a table-looking line inside code stays literal).

/// Per-message text budget (`slack_send`, replies): the spec's 4k chunk.
pub const CHUNK: usize = 4000;

/// One table row: starts and ends with `|`, has at least one inner cell.
fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.ends_with('|') && t.len() > 1 && t[1..].contains('|')
}

/// `**x**`/`__x__` -> `*x*`; leftover `*x*`/`*x*` singles -> `_x_`;
/// `~~x~~` -> `~x~`; `[label](url)` -> `<url|label>`. Code spans `…`
/// pass through untouched.
fn inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    // Peek `k` ahead.
    let at = |i: usize| -> char {
        if i < n {
            chars[i]
        } else {
            '\0'
        }
    };
    while i < n {
        let c = chars[i];
        if c == '`' {
            // Code span: copy through the closing backtick run.
            let mut run = 1;
            while at(i + run) == '`' {
                run += 1;
            }
            let fence: String = chars[i..i + run].iter().collect();
            let mut j = i + run;
            while j + run <= n && chars[j..j + run] != chars[i..i + run] {
                j += 1;
            }
            let end = if j + run <= n { j + run } else { n };
            for ch in &chars[i..end.min(n)] {
                out.push(*ch);
            }
            let _ = fence;
            i = end.min(n);
            continue;
        }
        if (c == '*' && at(i + 1) == '*') || (c == '_' && at(i + 1) == '_') {
            // **bold** / __bold__ -> *bold*
            let mark: String = chars[i..i + 2].iter().collect();
            if let Some(close) = find(&chars, i + 2, &mark) {
                out.push('*');
                out.push_str(&inline(&chars[i + 2..close].iter().collect::<String>()));
                out.push('*');
                i = close + 2;
                continue;
            }
        }
        if c == '~' && at(i + 1) == '~' {
            if let Some(close) = find(&chars, i + 2, "~~") {
                out.push('~');
                out.push_str(&inline(&chars[i + 2..close].iter().collect::<String>()));
                out.push('~');
                i = close + 2;
                continue;
            }
        }
        if c == '*' && at(i + 1) != ' ' && at(i + 1) != '\0' {
            // *italic* -> _italic_ (bold already consumed above)
            if let Some(close) = find(&chars, i + 1, "*") {
                if close > i + 1 {
                    out.push('_');
                    out.push_str(&inline(&chars[i + 1..close].iter().collect::<String>()));
                    out.push('_');
                    i = close + 1;
                    continue;
                }
            }
        }
        if c == '[' {
            // [label](url) -> <url|label>
            if let Some(close) = find(&chars, i + 1, "]") {
                if at(close + 1) == '(' {
                    if let Some(paren) = find(&chars, close + 2, ")") {
                        let label: String = chars[i + 1..close].iter().collect();
                        let url: String = chars[close + 2..paren].iter().collect();
                        if !url.trim().is_empty() {
                            out.push('<');
                            out.push_str(url.trim());
                            out.push('|');
                            out.push_str(&inline(&label));
                            out.push('>');
                            i = paren + 1;
                            continue;
                        }
                    }
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn find(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let n = chars.len();
    let needle: Vec<char> = needle.chars().collect();
    let m = needle.len();
    let mut i = from;
    while i + m <= n {
        if chars[i..i + m] == needle[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Whole-document conversion, fenced-code aware.
pub fn convert(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut in_code = false;
    let mut table: Vec<String> = Vec::new();
    let flush_table = |out: &mut String, table: &mut Vec<String>| {
        if table.is_empty() {
            return;
        }
        // Slack renders pipes literally; a monospace block keeps the grid.
        let sep = table.iter().position(|l| {
            l.trim()
                .chars()
                .all(|c| c == '|' || c == '-' || c == ':' || c == ' ')
        });
        let mut rows: Vec<&String> = table.iter().collect();
        if let Some(i) = sep {
            rows.remove(i);
        }
        out.push_str("```\n");
        for r in rows {
            let t = r.trim().trim_matches('|');
            let cells: Vec<&str> = t.split('|').map(str::trim).collect();
            out.push_str(&cells.join("  |  "));
            out.push('\n');
        }
        out.push_str("```\n");
        table.clear();
    };
    for line in markdown.split('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            flush_table(&mut out, &mut table);
            in_code = !in_code;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_code {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if is_table_row(line) {
            table.push(line.to_string());
            continue;
        }
        flush_table(&mut out, &mut table);
        if trimmed.starts_with('#') {
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            if trimmed.chars().nth(hashes) == Some(' ') {
                let text = &trimmed[hashes + 1..];
                out.push('*');
                out.push_str(&inline(text));
                out.push_str("*\n");
                continue;
            }
        }
        if matches!(trimmed, "---" | "***" | "___") {
            out.push_str("───────────────────\n");
            continue;
        }
        // Unordered bullets: "- " or "* " at up to 8 spaces indent.
        let indent_len = line.len() - trimmed.len();
        let mut line_out = String::new();
        let mut body = trimmed;
        if indent_len <= 8 {
            if let Some(rest) = trimmed.strip_prefix("- ") {
                line_out.push_str(&line[..indent_len]);
                line_out.push_str("• ");
                body = rest;
            } else if let Some(rest) = trimmed.strip_prefix("* ") {
                line_out.push_str(&line[..indent_len]);
                line_out.push_str("• ");
                body = rest;
            }
        }
        line_out.push_str(&inline(body));
        out.push_str(&line_out);
        out.push('\n');
    }
    flush_table(&mut out, &mut table);
    // Trailing newline the loop always adds: trim to the input's own
    // trailing-newline presence.
    if !markdown.ends_with('\n') {
        while out.ends_with('\n') {
            out.pop();
        }
    }
    out
}

/// Split mrkdwn into <= CHUNK-byte messages at newline/space boundaries,
/// keeping ``` fences balanced across the split.
pub fn chunk(text: &str) -> Vec<String> {
    if text.len() <= CHUNK {
        return if text.trim().is_empty() {
            Vec::new()
        } else {
            vec![text.to_string()]
        };
    }
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut reopen = false;
    while start < text.len() {
        let mut part = String::new();
        if reopen {
            part.push_str("```\n");
            reopen = false;
        }
        let limit = CHUNK - part.len();
        let remaining = &text[start..];
        let mut end = remaining.len().min(limit);
        while end > 0 && !remaining.is_char_boundary(end) {
            end -= 1;
        }
        if end < remaining.len() {
            if let Some(nl) = remaining[..end].rfind('\n') {
                if nl > limit / 3 {
                    end = nl + 1;
                }
            } else if let Some(sp) = remaining[..end].rfind(' ') {
                if sp > limit / 3 {
                    end = sp + 1;
                }
            }
        }
        if end == 0 {
            break;
        }
        part.push_str(&remaining[..end]);
        // Keep code fences balanced: an open ``` closes this part and
        // reopens the next.
        if part.matches("```").count() % 2 == 1 {
            part.push_str("\n```");
            reopen = true;
        }
        parts.push(part);
        start += end;
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_and_links() {
        assert_eq!(convert("a **bold** b"), "a *bold* b");
        assert_eq!(
            convert("[gray](https://gray.dev)"),
            "<https://gray.dev|gray>"
        );
        assert_eq!(convert("## Title"), "*Title*");
        assert_eq!(convert("- one\n- two"), "• one\n• two");
        assert_eq!(convert("~~gone~~"), "~gone~");
    }

    #[test]
    fn code_is_untouched() {
        let src = "```\n**not bold** | a | b |\n```\n**yes**";
        let out = convert(src);
        assert!(out.contains("**not bold**"));
        assert!(out.ends_with("*yes*\n") || out.ends_with("*yes*"));
    }

    #[test]
    fn chunk_balances_fences() {
        let body = format!("```\n{}\n", "x".repeat(CHUNK * 2));
        let parts = chunk(&body);
        assert!(parts.len() >= 2);
        for p in &parts {
            assert_eq!(p.matches("```").count() % 2, 0);
        }
    }
}
