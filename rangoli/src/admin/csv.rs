//! A small RFC 4180 CSV reader for admin imports: quoted fields, doubled quotes, line breaks
//! inside quotes, LF or CRLF. Spreadsheets' formula guard (`'=...`, added by the export) is
//! undone, so an export imports back unchanged.

/// Rows of cells, or the 1-based line where the text stopped making sense.
pub(crate) fn parse(text: &str) -> Result<Vec<Vec<String>>, usize> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text); // Excel's byte-order mark
    let (mut rows, mut row, mut cell) = (vec![], vec![], String::new());
    let (mut quoted, mut line, mut chars) = (false, 1, text.chars().peekable());
    let mut cell_started_quoted = false;
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (true, c) => {
                if c == '\n' {
                    line += 1;
                }
                cell.push(c);
            }
            (false, '"') if cell.is_empty() && !cell_started_quoted => {
                quoted = true;
                cell_started_quoted = true;
            }
            (false, '"') => return Err(line), // a quote in the middle of an unquoted cell
            (false, ',') => {
                row.push(unguard(std::mem::take(&mut cell)));
                cell_started_quoted = false;
            }
            (false, '\r') if chars.peek() == Some(&'\n') => {}
            (false, '\n') | (false, '\r') => {
                row.push(unguard(std::mem::take(&mut cell)));
                cell_started_quoted = false;
                if !(row.len() == 1 && row[0].is_empty()) {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
                line += 1;
            }
            (false, c) => cell.push(c),
        }
    }
    if quoted {
        return Err(line);
    }
    if !cell.is_empty() || !row.is_empty() || cell_started_quoted {
        row.push(unguard(cell));
        rows.push(row);
    }
    Ok(rows)
}

/// `'=SUM(A1)` back to `=SUM(A1)`: the export's guard against formula injection.
fn unguard(cell: String) -> String {
    match cell.strip_prefix('\'') {
        Some(rest) if rest.starts_with(['=', '+', '-', '@', '\t', '\r']) => rest.to_string(),
        _ => cell,
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn rfc_4180() {
        let rows = parse("id,title\r\n1,\"Plan, \"\"quoted\"\"\nnext\"\r\n2,'=SUM(A1)\n\n3,plain").unwrap();
        assert_eq!(rows, [vec!["id", "title"], vec!["1", "Plan, \"quoted\"\nnext"], vec!["2", "=SUM(A1)"], vec!["3", "plain"]]);
        assert_eq!(parse("\u{feff}a,b\n1,2\n").unwrap(), [vec!["a", "b"], vec!["1", "2"]], "BOM and trailing newline");
        assert_eq!(parse("a,\"\"\n").unwrap(), [vec!["a", ""]], "an empty quoted cell");
        assert_eq!(parse("a,b\n1,\"unterminated\n"), Err(3));
        assert_eq!(parse("a\nx\"y\n"), Err(2), "a stray quote");
        assert_eq!(parse("'it's fine'").unwrap(), [vec!["'it's fine'"]], "only formula guards are undone");
    }
}
