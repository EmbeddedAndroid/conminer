//! The Android profile's shell prompt, as consoles actually print it.

use conminer_core::framer::profile::PromptKind;
use conminer_core::framer::ProfileSet;

/// mksh shows the last command's exit status before the prompt when it was
/// non-zero. Seen on an MTP console: run_command refused the board with
/// UNKNOWN_PROMPT as soon as one command had failed.
#[test]
fn the_prompt_is_known_with_and_without_an_exit_status() {
    let profiles = ProfileSet::builtin().unwrap();
    let a = profiles.get("android").unwrap();
    for prompt in [
        "console:/ $ ",
        "console:/ # ",
        "1|console:/ $ ",
        "130|console:/ # ",
    ] {
        let p = a
            .prompt_match(prompt)
            .unwrap_or_else(|| panic!("{prompt:?} must be a known prompt"));
        assert_eq!(p.kind, PromptKind::Shell, "{prompt:?}");
    }
    // A line that merely ends in `$` inside output is not a prompt.
    assert!(a.prompt_match("cost: 5|10 $ total").is_none());
}
