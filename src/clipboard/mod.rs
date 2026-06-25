//! Clipboard helpers.
//!
//! Phase 0 ships only stubs. The Windows-backed implementation (using the
//! `clipboard-win` / `arboard` family, placing both HTML and TSV on the
//! clipboard for Excel-friendly table paste) arrives in a later phase.

/// Copy plain text to the clipboard.
#[allow(dead_code)]
pub fn copy_text(_text: &str) -> anyhow::Result<()> {
    anyhow::bail!("not implemented yet")
}

/// Copy a table so it pastes as a grid in Excel. Accepts a table in markdown
/// or HTML; places both HTML and TSV on the clipboard.
#[allow(dead_code)]
pub fn copy_table(_table_markdown_or_html: &str) -> anyhow::Result<()> {
    anyhow::bail!("not implemented yet")
}
