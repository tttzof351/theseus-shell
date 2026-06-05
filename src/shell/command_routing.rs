use std::{env, path::Path};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::feature_flags;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandRoute {
    Agent,
    Shell,
}

pub(super) fn classify_command(command: &str, working_dir: Option<&Path>) -> CommandRoute {
    if is_agent_natural_language_with_cwd(command, working_dir) {
        CommandRoute::Agent
    } else {
        CommandRoute::Shell
    }
}

fn is_agent_natural_language_with_cwd(command: &str, working_dir: Option<&Path>) -> bool {
    if command.starts_with('/') {
        return false;
    }

    if command
        .chars()
        .any(|ch| ch.is_alphabetic() && !ch.is_ascii())
    {
        return !looks_like_shell_command(command);
    }

    feature_flags::ENGLISH_AGENT_COMMAND_ROUTING
        && looks_like_english_agent_prompt(command, working_dir)
}

fn looks_like_shell_command(command: &str) -> bool {
    let Some(first_word) = command.split_whitespace().next() else {
        return false;
    };

    first_word
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | '~'))
}

fn looks_like_english_agent_prompt(command: &str, working_dir: Option<&Path>) -> bool {
    if contains_shell_syntax(command) || !command.chars().any(|ch| ch.is_ascii_alphabetic()) {
        return false;
    }

    let tokens = command.split_whitespace().collect::<Vec<_>>();
    let lead_index = english_agent_lead_word_index(&tokens);
    let is_short_direct_lead_command = lead_index
        .map(|index| tokens.len().saturating_sub(index) <= 2)
        .unwrap_or(false);

    if lead_index
        .map(|index| tokens.len().saturating_sub(index) < 2)
        .unwrap_or_else(|| tokens.len() < 2)
    {
        return false;
    }

    if tokens
        .iter()
        .skip(lead_index.map_or(1, |index| index + 1))
        .any(|token| looks_like_shell_argument(token, working_dir, is_short_direct_lead_command))
    {
        return false;
    }

    if !tokens
        .iter()
        .all(|token| normalized_english_word(token).is_some())
    {
        return false;
    }

    lead_index.is_some() || first_word_is_not_path_command(&tokens)
}

fn contains_shell_syntax(command: &str) -> bool {
    command.chars().any(|ch| {
        matches!(
            ch,
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '{' | '}' | '\\'
        )
    })
}

fn is_english_agent_lead_word(word: &str) -> bool {
    matches!(
        word,
        "add"
            | "analyze"
            | "change"
            | "compare"
            | "create"
            | "delete"
            | "explain"
            | "find"
            | "fix"
            | "inspect"
            | "list"
            | "read"
            | "remove"
            | "search"
            | "show"
            | "summarize"
            | "tell"
            | "update"
            | "what"
            | "where"
            | "why"
    )
}

fn english_agent_lead_word_index(tokens: &[&str]) -> Option<usize> {
    let mut index = 0;
    while let Some(word) = tokens
        .get(index)
        .and_then(|token| normalized_english_word(token))
    {
        if !is_conversational_prefix(&word) {
            break;
        }
        index += 1;
    }

    let word = tokens
        .get(index)
        .and_then(|token| normalized_english_word(token))?;

    if is_english_agent_lead_word(&word) {
        return Some(index);
    }

    if is_modal_question_word(&word)
        && tokens
            .get(index + 1)
            .and_then(|token| normalized_english_word(token))
            .as_deref()
            == Some("you")
    {
        return Some(index);
    }

    None
}

fn normalized_english_word(token: &str) -> Option<String> {
    let token = token.trim_matches(|ch| matches!(ch, '"' | '\'' | ',' | '.' | '!' | '?' | ':'));
    if token.chars().any(|ch| ch.is_ascii_alphanumeric()) {
        Some(token.to_ascii_lowercase())
    } else {
        None
    }
}

fn is_conversational_prefix(word: &str) -> bool {
    matches!(word, "ok" | "okay" | "hey" | "hi" | "hello")
}

fn is_modal_question_word(word: &str) -> bool {
    matches!(word, "can" | "could" | "would" | "should")
}

fn first_word_is_not_path_command(tokens: &[&str]) -> bool {
    let Some(first_word) = tokens
        .first()
        .and_then(|token| normalized_english_word(token))
    else {
        return false;
    };

    !path_command_exists(&first_word)
}

fn path_command_exists(command: &str) -> bool {
    if command.contains('/') {
        return is_executable_path(Path::new(command));
    }

    let Some(path) = env::var_os("PATH") else {
        return false;
    };

    env::split_paths(&path).any(|dir| is_executable_path(&dir.join(command)))
}

fn is_executable_path(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };

    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn looks_like_shell_argument(
    token: &str,
    working_dir: Option<&Path>,
    check_existing_cwd_path: bool,
) -> bool {
    let token = token.trim_matches(|ch| matches!(ch, '"' | '\''));
    if token.is_empty() {
        return false;
    }

    if token.starts_with('-')
        || token.starts_with('.')
        || token.starts_with('/')
        || token.starts_with('~')
        || token.contains('/')
        || token
            .chars()
            .any(|ch| matches!(ch, '*' | '[' | ']' | '$' | '`' | '=' | ':'))
        || has_shell_question_mark(token)
    {
        return true;
    }

    check_existing_cwd_path
        && working_dir
            .map(|dir| dir.join(token).exists())
            .unwrap_or(false)
}

fn has_shell_question_mark(token: &str) -> bool {
    if !token.contains('?') {
        return false;
    }

    let Some(word) = token.strip_suffix('?') else {
        return true;
    };

    word.contains('?')
        || word.is_empty()
        || !word
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn routes_non_english_input_to_agent() {
        assert_eq!(classify_command("что умеешь?", None), CommandRoute::Agent);
        assert_eq!(
            classify_command("покажи три больших файла", None),
            CommandRoute::Agent
        );
    }

    #[test]
    fn routes_non_english_path_to_shell() {
        assert_eq!(classify_command("cat файл.txt", None), CommandRoute::Shell);
    }

    #[test]
    fn routes_english_agent_prompts_to_agent() {
        assert_eq!(
            classify_command("find biggest file", None),
            CommandRoute::Agent
        );
        assert_eq!(
            classify_command("ok, could you find mp3 files in this dir?", None),
            CommandRoute::Agent
        );
        assert_eq!(
            classify_command("theseusunknownword files is here?", None),
            CommandRoute::Agent
        );
    }

    #[test]
    fn routes_shell_markers_to_shell() {
        assert_eq!(
            classify_command(r#"find . -name "*.jpg""#, None),
            CommandRoute::Shell
        );
        assert_eq!(
            classify_command(r#"find . -name "file?.mp3""#, None),
            CommandRoute::Shell
        );
        assert_eq!(classify_command("/history", None), CommandRoute::Shell);
    }

    #[test]
    fn routes_known_path_command_to_shell() {
        assert_eq!(classify_command("ls downloads", None), CommandRoute::Shell);
    }

    #[test]
    fn routes_existing_path_argument_to_shell() {
        let temp_dir = env::temp_dir();
        let existing_dir = temp_dir.join(format!("theseus-shell-find-test-{}", std::process::id()));
        std::fs::create_dir_all(&existing_dir).unwrap();

        let route = classify_command(
            &format!(
                "find {}",
                existing_dir.file_name().unwrap().to_string_lossy()
            ),
            Some(&PathBuf::from(&temp_dir)),
        );

        assert_eq!(route, CommandRoute::Shell);
        std::fs::remove_dir_all(existing_dir).unwrap();
    }

    // === Edge cases: modal questions ===
    //
    // The heuristic supports all four modal words (can/could/would/should)
    // when followed by "you". Existing tests only cover "could".

    #[test]
    fn routes_modal_can_you_to_agent() {
        assert_eq!(
            classify_command("can you find biggest file", None),
            CommandRoute::Agent
        );
    }

    #[test]
    fn routes_modal_would_you_to_agent() {
        assert_eq!(
            classify_command("would you list all files", None),
            CommandRoute::Agent
        );
    }

    #[test]
    fn routes_modal_should_you_to_agent() {
        assert_eq!(
            classify_command("should you delete this file", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: chained conversational prefixes ===
    //
    // In english_agent_lead_word_index, the while loop skips any number
    // of conversational prefixes in a row. Cover chains of 2-3.

    #[test]
    fn routes_chained_conversational_prefixes_to_agent() {
        assert_eq!(
            classify_command("ok hey what is the time", None),
            CommandRoute::Agent
        );
        assert_eq!(
            classify_command("hey hi hello find biggest", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: apostrophe in a token ===
    //
    // normalized_english_word does not strip apostrophes (they are not in
    // its trim set), and the token still passes the "has ASCII letter"
    // check. Apostrophes should not break the agent branch.

    #[test]
    fn routes_apostrophe_token_to_agent() {
        assert_eq!(
            classify_command("what's the time", None),
            CommandRoute::Agent
        );
        assert_eq!(
            classify_command("show me what's in here", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: path-like tokens in various forms ===
    //
    // looks_like_shell_argument should catch: ., ./, /, ~, '/' inside a
    // token, glob characters. If the user passes a path to the agent,
    // it is clearly shell context.

    #[test]
    fn routes_dotfile_path_to_shell() {
        assert_eq!(classify_command("show .env", None), CommandRoute::Shell);
    }

    #[test]
    fn routes_relative_path_to_shell() {
        assert_eq!(classify_command("list ./build", None), CommandRoute::Shell);
    }

    #[test]
    fn routes_tilde_path_to_shell() {
        assert_eq!(
            classify_command("read ~/notes.txt", None),
            CommandRoute::Shell
        );
    }

    #[test]
    fn routes_inline_path_token_to_shell() {
        // `foo/bar` — contains '/', caught by looks_like_shell_argument.
        assert_eq!(classify_command("show foo/bar", None), CommandRoute::Shell);
    }

    #[test]
    fn routes_glob_token_to_shell() {
        // Any glob character in a token → shell.
        assert_eq!(classify_command("find *.txt", None), CommandRoute::Shell);
        assert_eq!(
            classify_command("list [abc]*.log", None),
            CommandRoute::Shell
        );
        assert_eq!(classify_command("show $HOME", None), CommandRoute::Shell);
        assert_eq!(classify_command("list `pwd`", None), CommandRoute::Shell);
        assert_eq!(
            classify_command("show key=value", None),
            CommandRoute::Shell
        );
    }

    // === Edge cases: token with a flag prefix ===

    #[test]
    fn routes_flag_token_to_shell() {
        assert_eq!(classify_command("list -la", None), CommandRoute::Shell);
        assert_eq!(
            classify_command("find --name foo", None),
            CommandRoute::Shell
        );
    }

    // === Edge cases: bare lead-agent word with no arguments ===
    //
    // A single token "find" — tokens.len()=1, lead_index is found, but
    // tokens.len().saturating_sub(0) = 1 < 2 → rejected.
    // Fallback: first_word "find" is in PATH → first_word_is_not_path_command
    // returns false → lead_index.is_some() || first_word ... = false || false
    // = false → Shell.

    #[test]
    fn routes_bare_lead_agent_word_to_shell() {
        assert_eq!(classify_command("find", None), CommandRoute::Shell);
        assert_eq!(classify_command("show", None), CommandRoute::Shell);
        assert_eq!(classify_command("summarize", None), CommandRoute::Shell);
    }

    // === Edge cases: lead + exactly one following token ===
    //
    // "find me" — tokens.len()=2, lead_index=0, tokens.len()-0=2, not <2,
    // skip(1) is empty → false. all: both Some. lead_index.is_some()=true
    // → Agent.
    //
    // "find me" with only one token after the lead (which it is) — only
    // "me" is forwarded to the agent. This is a debatable decision, but
    // the current behaviour is: Agent. Pin it down so a future refactor
    // does not silently flip it.

    #[test]
    fn routes_lead_with_single_followup_to_agent() {
        assert_eq!(classify_command("find me", None), CommandRoute::Agent);
        assert_eq!(
            classify_command("show something", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: non-lead-agent first word ===
    //
    // english_agent_lead_word_index returns None for words outside its
    // list, and the fallback checks "is it in PATH?". If the word is
    // not in PATH, first_word_is_not_path_command=true, and the phrase
    // goes to Agent. This is intentional (already covered), but we
    // also exercise chains of conversational prefixes leading to a
    // non-lead word.

    #[test]
    fn routes_unknown_lead_with_question_to_agent() {
        assert_eq!(
            classify_command("blah files is here?", None),
            CommandRoute::Agent
        );
        assert_eq!(
            classify_command("hey blah is up?", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: shell syntax (pipe/redirect/etc) ===
    //
    // contains_shell_syntax catches: | & ; < > ( ) { } \.
    // Any of these must send the command to Shell.

    #[test]
    fn routes_shell_syntax_to_shell() {
        assert_eq!(
            classify_command("find . | grep foo", None),
            CommandRoute::Shell
        );
        assert_eq!(
            classify_command("echo foo > out.txt", None),
            CommandRoute::Shell
        );
        assert_eq!(classify_command("cat < in.txt", None), CommandRoute::Shell);
        assert_eq!(classify_command("ls; pwd", None), CommandRoute::Shell);
        assert_eq!(
            classify_command("echo a && echo b", None),
            CommandRoute::Shell
        );
        assert_eq!(classify_command("echo (a)", None), CommandRoute::Shell);
        assert_eq!(classify_command("echo {a,b}", None), CommandRoute::Shell);
    }

    // === Edge cases: empty or near-empty command ===
    //
    // "blah" — 1 token, no lead, tokens.len()=1 < 2 → looks_like_english_
    // agent_prompt immediately returns false → Shell. This is intentional:
    // a short single token is most likely a program name.
    //
    // "blah!" — tokens=["blah!"], normalized → "blah" → Some.
    // english_agent_lead → None. tokens.len()=1 → false → Shell.
    // Pin it down: a single token always goes to Shell.

    #[test]
    fn routes_single_unknown_word_to_shell() {
        assert_eq!(classify_command("blah", None), CommandRoute::Shell);
    }

    // === Edge cases: punctuation attached to words ===
    //
    // normalized_english_word strips: " ' , . ! ? :
    // "find!" → "find" (lead). "find?" → "find". "find..." → "find".
    // "find," → "find".
    //
    // Comma after a conversational prefix: "ok, what is the time"
    // → trim strips ',' → "ok" → conversational → skip → "what" → lead → Agent.

    #[test]
    fn routes_punctuation_around_words_to_agent() {
        assert_eq!(
            classify_command("find!", None),
            CommandRoute::Shell,
            "bare 'find' should still go to Shell (sanity check on test setup)"
        );
        assert_eq!(classify_command("find! biggest", None), CommandRoute::Agent);
        assert_eq!(
            classify_command("ok, what is the time", None),
            CommandRoute::Agent
        );
        assert_eq!(
            classify_command("ok. what is the time", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: ENGLISH_AGENT_COMMAND_ROUTING flag turned off ===
    //
    // This scenario is currently unreachable without recompiling (the flag
    // is a const bool in feature_flags.rs). Keep the test commented out so
    // it can be activated quickly if the flag is moved to a cfg.

    // #[cfg(not(ENGLISH_AGENT_COMMAND_ROUTING))]
    // #[test]
    // fn routes_english_prompts_to_shell_when_flag_off() {
    //     assert_eq!(classify_command("find biggest file", None), CommandRoute::Shell);
    //     assert_eq!(classify_command("could you list files", None), CommandRoute::Shell);
    // }

    // === Edge cases: existing cwd file as an argument to an agent prompt ===
    //
    // If "notes" exists in cwd and the user types "show notes",
    // looks_like_shell_argument("notes") returns true (dir.join.exists()).
    // The command is then routed to Shell. This is expected: if the user
    // has a file named "notes" in cwd, "show notes" is likely `cat notes`
    // or similar. Pin it down: an existing cwd file → Shell.

    #[test]
    fn routes_existing_cwd_file_to_shell() {
        let temp_dir = env::temp_dir();
        let existing_file =
            temp_dir.join(format!("theseus-shell-notes-{}.txt", std::process::id()));
        std::fs::write(&existing_file, b"hello").unwrap();

        let route = classify_command(
            &format!(
                "show {}",
                existing_file.file_name().unwrap().to_string_lossy()
            ),
            Some(&PathBuf::from(&temp_dir)),
        );

        assert_eq!(route, CommandRoute::Shell);
        std::fs::remove_file(existing_file).unwrap();
    }

    // === Edge cases: cwd file does NOT exist, argument looks like a word ===
    //
    // "show something" — looks_like_shell_argument("something") catches
    // nothing, dir.join("something").exists()=false → false → Agent.
    //
    // "show" itself is a lead-agent word → Agent.

    #[test]
    fn routes_nonexistent_cwd_word_to_agent() {
        assert_eq!(
            classify_command("show supercalifragilistic", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: shell commands whose names overlap with lead-agent words ===
    //
    // `find` is both in the lead-agent list and in PATH. The current
    // heuristic gives lead-agent priority: "find biggest file" → Agent,
    // not Shell. This is intentional (already covered). Additionally:
    // after the lead, arguments are NOT checked against PATH — only
    // against existence in cwd.

    #[test]
    fn routes_find_with_existing_cwd_dir_to_shell() {
        // `find downloads` — find is the lead, "downloads" —
        // dir.join.exists()=true → looks_like_shell_argument=true → Shell.
        // Same outcome as for "ls downloads", but routed via the lead-agent
        // branch.
        let temp_dir = env::temp_dir();
        let existing_dir = temp_dir.join(format!("theseus-shell-dl-{}", std::process::id()));
        std::fs::create_dir_all(&existing_dir).unwrap();

        let route = classify_command(
            &format!(
                "find {}",
                existing_dir.file_name().unwrap().to_string_lossy()
            ),
            Some(&PathBuf::from(&temp_dir)),
        );

        assert_eq!(route, CommandRoute::Shell);
        std::fs::remove_dir_all(existing_dir).unwrap();
    }

    // === Edge cases: `!` inside a token (NOT at the end) ===
    //
    // normalized_english_word strips `!` ONLY from the end of the token.
    // Internally, it stays. looks_like_shell_argument does not catch `!`,
    // so the heuristic lets the token through. This is a debatable spot,
    // but we pin down the current behaviour: the command goes to Agent
    // (because "best!" normalizes to "best" — has letters → Some).

    #[test]
    fn routes_token_with_internal_punctuation_to_agent() {
        // "best!" — trim strips trailing '!' → "best".
        // "show best! file" — all tokens pass → Agent.
        assert_eq!(
            classify_command("show best! file", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: numeric token in a prompt ===
    //
    // Numeric words are common in natural-language prompts like
    // "show the 10 largest files". They should not force the command
    // into the shell path.

    #[test]
    fn routes_numeric_token_to_agent() {
        assert_eq!(
            classify_command("find 3 biggest files", None),
            CommandRoute::Agent
        );
        assert_eq!(
            classify_command("show 2 biggest", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: hexadecimal token ===
    //
    // "0x1" contains the ASCII letter 'x', so it passes
    // normalized_english_word. It is not a lead-agent, modal, or
    // conversational word. Fallback: "0x1" is not in PATH →
    // first_word_is_not_path_command=true →
    // lead_index.is_some()=false || true = true → Agent. Reasonable.

    #[test]
    fn routes_hex_prefixed_token_to_agent() {
        assert_eq!(
            classify_command("0x1 find biggest", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: typical unix commands that do not match lead-agent words ===
    //
    // "git status", "docker ps", "npm install" — "git"/"docker"/"npm"
    // are in PATH → first_word_is_not_path_command=false → Shell.

    #[test]
    fn routes_git_status_to_shell() {
        // "git" is normally in PATH; "status" is normally not a file in cwd.
        assert_eq!(classify_command("git status", None), CommandRoute::Shell);
    }

    // === Edge cases: shell comments and `!` history ===
    //
    // "#" is not in contains_shell_syntax, and not in looks_like_shell_argument.
    // It also has no alphanumeric characters, so it still rejects the
    // natural-language branch. This avoids treating shell comments as an
    // agent prompt.

    #[test]
    fn hash_token_falls_back_to_shell_known_bug() {
        assert_eq!(
            classify_command("find biggest file # comment", None),
            CommandRoute::Shell
        );
    }

    // === Edge cases: conversational prefix + non-lead-agent word + question mark ===
    //
    // "hey blah is up?" — hey (conv), blah (not lead, not modal, not conv).
    // english_agent_lead_word_index: index=0, "hey" is conv → skip, index=1.
    // tokens.get(1)="blah" → normalized_english_word → "blah" →
    // is_english_agent_lead_word? no. is_modal_question_word? no.
    // → None. tokens.len()=4, not <2. skip(1): "blah","is","up?" —
    //   "up?" → has_shell_question_mark? contains('?') yes.
    //   strip_suffix('?') → "up" → word.is_empty()=false →
    //   word.contains('?')=false → all alphanumeric|_|- : "up" — yes → false.
    //   has_shell_qmark=false. "blah","is" — false. → any=false.
    //   all: every token has ASCII letters → all Some.
    //   lead_index.is_some()=false || first_word_is_not_path_command
    //   (for "hey" → path_command_exists("hey") → usually false → true) → true.
    //   → Agent.

    #[test]
    fn routes_hey_unknown_word_with_question_to_agent() {
        assert_eq!(
            classify_command("hey blah is up?", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: numeric token as "1" at the start ===
    //
    // Leading numeric prompts are uncommon, but if the phrase has enough
    // natural-language context and no shell syntax, route it to the agent.

    #[test]
    fn routes_leading_numeric_token_to_agent() {
        assert_eq!(
            classify_command("1 find biggest file", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: modal "can you" with minimal context ===
    //
    // "can you" (exactly 2 tokens) — modal+you, lead_index=0,
    // tokens.len()=2, not < 2, skip(1) is empty, all ok, lead is set
    // → Agent. This may be an incomplete request, but the heuristic
    // still forwards it to the agent.

    #[test]
    fn routes_minimal_can_you_to_agent() {
        assert_eq!(classify_command("can you", None), CommandRoute::Agent);
        assert_eq!(classify_command("can you find", None), CommandRoute::Agent);
    }

    // === Edge cases: lead-agent after a conversational prefix ===
    //
    // "hey show me biggest" — hey (conv) skip, show (lead) → Some(1).
    // tokens.len()-1=3 >= 2, skip(2): me, biggest — not args, all ok
    // → Agent.

    #[test]
    fn routes_lead_after_conversational_to_agent() {
        assert_eq!(
            classify_command("hey show me biggest", None),
            CommandRoute::Agent
        );
        assert_eq!(
            classify_command("ok find biggest file", None),
            CommandRoute::Agent
        );
        assert_eq!(
            classify_command("hi list all files", None),
            CommandRoute::Agent
        );
    }

    // === Edge cases: lead-agent "what" + interrogative words ===
    //
    // "what is up doc" — what (lead) → Some(0), skip(1): is,up,doc.
    // Nothing looks like a shell argument, all ok, lead → Agent.

    #[test]
    fn routes_what_question_to_agent() {
        assert_eq!(
            classify_command("what is up doc", None),
            CommandRoute::Agent
        );
        assert_eq!(
            classify_command("what time is it", None),
            CommandRoute::Agent
        );
    }

    #[test]
    fn routes_long_prompt_with_number_and_existing_cwd_word_to_agent() {
        let temp_dir = env::temp_dir();
        let existing_dir = temp_dir.join(format!("theseus-shell-target-{}", std::process::id()));
        std::fs::create_dir_all(&existing_dir).unwrap();

        let route = classify_command(
            &format!(
                "show me the 10 largest files in this repository excluding {}",
                existing_dir.file_name().unwrap().to_string_lossy()
            ),
            Some(&PathBuf::from(&temp_dir)),
        );

        assert_eq!(route, CommandRoute::Agent);
        std::fs::remove_dir_all(existing_dir).unwrap();
    }
}
