use comfy_table::{presets::UTF8_FULL, Table};
use serde::Serialize;
use std::io::{self, Write};

#[derive(Debug, Clone, Default, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Table,
    Csv,
    Json,
}

pub fn print_rows<T: Serialize>(
    rows: &[T],
    headers: &[&str],
    to_row: impl Fn(&T) -> Vec<String>,
    format: OutputFormat,
) {
    if let Err(e) = write_rows(rows, headers, to_row, format, &mut std::io::stdout()) {
        if e.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("Error writing output: {e}");
        }
    }
}

pub fn write_rows<T: Serialize>(
    rows: &[T],
    headers: &[&str],
    to_row: impl Fn(&T) -> Vec<String>,
    format: OutputFormat,
    out: &mut impl Write,
) -> io::Result<()> {
    match format {
        OutputFormat::Table => {
            let mut table = Table::new();
            table.load_preset(UTF8_FULL);
            table.set_header(headers.to_vec());
            for row in rows {
                table.add_row(to_row(row));
            }
            writeln!(out, "{}", table)?;
        }
        OutputFormat::Csv => {
            writeln!(out, "{}", headers.join(","))?;
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
                writeln!(out, "{}", escaped.join(","))?;
            }
        }
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(rows).unwrap_or_else(|_| "[]".to_string());
            writeln!(out, "{}", json)?;
        }
    }
    Ok(())
}
