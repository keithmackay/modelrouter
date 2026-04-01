use comfy_table::{presets::UTF8_FULL, Table};
use serde::Serialize;
use std::io::Write;

pub enum OutputFormat {
    Table,
    Csv,
    Json,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Self {
        match s {
            "csv" => Self::Csv,
            "json" => Self::Json,
            _ => Self::Table,
        }
    }
}

pub fn print_rows<T: Serialize>(
    rows: &[T],
    headers: &[&str],
    to_row: impl Fn(&T) -> Vec<String>,
    format: OutputFormat,
) {
    write_rows(rows, headers, to_row, format, &mut std::io::stdout());
}

pub fn write_rows<T: Serialize>(
    rows: &[T],
    headers: &[&str],
    to_row: impl Fn(&T) -> Vec<String>,
    format: OutputFormat,
    out: &mut impl Write,
) {
    match format {
        OutputFormat::Table => {
            let mut table = Table::new();
            table.load_preset(UTF8_FULL);
            table.set_header(headers.to_vec());
            for row in rows {
                table.add_row(to_row(row));
            }
            writeln!(out, "{}", table).unwrap();
        }
        OutputFormat::Csv => {
            writeln!(out, "{}", headers.join(",")).unwrap();
            for row in rows {
                let fields = to_row(row);
                let escaped: Vec<String> = fields
                    .into_iter()
                    .map(|f| {
                        if f.contains(',') || f.contains('"') || f.contains('\n') {
                            format!("\"{}\"", f.replace('"', "\"\""))
                        } else {
                            f
                        }
                    })
                    .collect();
                writeln!(out, "{}", escaped.join(",")).unwrap();
            }
        }
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string());
            writeln!(out, "{}", json).unwrap();
        }
    }
}
