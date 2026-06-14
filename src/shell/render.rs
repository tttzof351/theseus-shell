use std::{io, io::Write};

use super::markdown_preprocessor::preprocess_markdown;
use crate::common::{output::CommandOutput, terminal_output};

pub(super) fn print_command_output(output: &CommandOutput) -> io::Result<()> {
    terminal_output::with_stdout(|stdout| {
        if !output.streamed {
            stdout.write_all(&output.transcript)?;
        }
        stdout.flush()
    })
}

pub(super) fn render_markdown(text: &str) -> String {
    let text = preprocess_markdown(text);
    ensure_trailing_newline(markdown_skin().term_text(&text).to_string())
}

fn markdown_skin() -> termimad::MadSkin {
    let mut skin = termimad::MadSkin::default();
    skin.inline_code.object_style.background_color = None;
    skin.code_block.compound_style.object_style.background_color = None;
    skin
}

fn ensure_trailing_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_rendering_does_not_use_background_colors() {
        let rendered = render_markdown("`code`");

        assert!(!rendered.contains("\x1b[40m"));
        assert!(!rendered.contains("\x1b[48;"));
    }
}
