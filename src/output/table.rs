//! Plain-text aligned table rendering for terminal output.
//!
//! Produces a clean, monospace, left-aligned table (not markdown). Column
//! widths are computed by Unicode scalar count (`chars().count()`), columns are
//! separated by at least two spaces, and the trailing padding of the final
//! column is trimmed so lines do not carry dangling whitespace. The output is
//! deterministic and panic-free, including for ragged rows.

/// Render `rows` under `headers` as an aligned, left-justified text table.
///
/// Each column's width is the maximum of the header length and the widest cell
/// in that column, measured in Unicode scalar values (`chars().count()`), not
/// bytes. Columns are separated by two spaces. The trailing whitespace of every
/// line is trimmed, so the final column is never padded. The returned string
/// ends with a trailing newline.
///
/// If a row has fewer cells than there are headers, the missing cells are
/// treated as empty strings. Extra cells beyond the header count are ignored
/// for width purposes but still rendered (so callers see all data); to keep
/// output well-formed they should match the header arity.
///
/// When `rows` is empty, the header row is still printed, followed by a
/// `(no results)` note line.
pub fn render(headers: &[&str], rows: &[Vec<String>]) -> String {
    let col_count = headers.len();

    // Compute column widths from headers and cells using char counts.
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < col_count {
                let len = cell.chars().count();
                if len > widths[i] {
                    widths[i] = len;
                }
            }
        }
    }

    let mut out = String::new();

    // Header row (built from &str cells).
    let header_cells: Vec<&str> = headers.to_vec();
    push_line(&mut out, &header_cells, &widths);

    if rows.is_empty() {
        out.push_str("(no results)");
        out.push('\n');
        return out;
    }

    for row in rows {
        let cells: Vec<&str> = (0..col_count.max(row.len()))
            .map(|i| row.get(i).map(String::as_str).unwrap_or(""))
            .collect();
        push_line(&mut out, &cells, &widths);
    }

    out
}

/// Append one rendered, right-trimmed line (terminated by `\n`) to `out`.
///
/// Cells are left-aligned and padded to `widths`; cells with no corresponding
/// width entry (ragged/extra columns) are emitted unpadded. Trailing whitespace
/// is removed before the newline so the final column carries no padding.
fn push_line(out: &mut String, cells: &[&str], widths: &[usize]) {
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        if let Some(&width) = widths.get(i) {
            let pad = width.saturating_sub(cell.chars().count());
            for _ in 0..pad {
                line.push(' ');
            }
        }
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_alignment() {
        let headers = ["Name", "Kind"];
        let rows = vec![
            vec!["Foo".to_string(), "class".to_string()],
            vec!["LongerName".to_string(), "fn".to_string()],
        ];
        let out = render(&headers, &rows);
        let expected = "\
Name        Kind
Foo         class
LongerName  fn
";
        assert_eq!(out, expected);
    }

    #[test]
    fn ends_with_newline() {
        let out = render(&["A"], &[vec!["x".to_string()]]);
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn no_trailing_whitespace_on_lines() {
        let headers = ["Name", "Kind"];
        let rows = vec![vec!["Foo".to_string(), "class".to_string()]];
        let out = render(&headers, &rows);
        for line in out.lines() {
            assert_eq!(line, line.trim_end(), "line had trailing space: {line:?}");
        }
    }

    #[test]
    fn final_column_not_padded() {
        // Header "Kind" is shorter than its cell; final col should not be padded
        // and the header line's trailing spaces must be trimmed.
        let headers = ["Name", "Kind"];
        let rows = vec![vec!["X".to_string(), "interface".to_string()]];
        let out = render(&headers, &rows);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "Name  Kind");
        assert_eq!(lines[1], "X     interface");
    }

    #[test]
    fn empty_rows_shows_note() {
        let out = render(&["Symbol", "Kind"], &[]);
        let expected = "\
Symbol  Kind
(no results)
";
        assert_eq!(out, expected);
    }

    #[test]
    fn ragged_row_missing_cells_treated_empty() {
        let headers = ["A", "B", "C"];
        let rows = vec![
            vec!["1".to_string()],
            vec!["x".to_string(), "y".to_string(), "z".to_string()],
        ];
        let out = render(&headers, &rows);
        let lines: Vec<&str> = out.lines().collect();
        // Row with only one cell: trailing empties trimmed away.
        assert_eq!(lines[1], "1");
        assert_eq!(lines[2], "x  y  z");
    }

    #[test]
    fn unicode_width_by_char_count() {
        // "café" is 4 chars but 5 bytes; width must use char count.
        let headers = ["X"];
        let rows = vec![vec!["café".to_string()], vec!["ab".to_string()]];
        let out = render(&headers, &rows);
        let lines: Vec<&str> = out.lines().collect();
        // Column width = 4 ("café"); "ab" padded to 4 but trimmed (last col).
        assert_eq!(lines[0], "X");
        assert_eq!(lines[1], "café");
        assert_eq!(lines[2], "ab");
    }

    #[test]
    fn min_two_space_separator() {
        let headers = ["AA", "BB"];
        let rows = vec![vec!["1".to_string(), "2".to_string()]];
        let out = render(&headers, &rows);
        // "AA" width 2, separated by 2 spaces from "BB".
        assert_eq!(out.lines().next().unwrap(), "AA  BB");
    }

    #[test]
    fn empty_headers_and_rows() {
        let out = render(&[], &[]);
        assert_eq!(out, "\n(no results)\n");
    }
}
