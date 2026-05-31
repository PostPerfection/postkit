//! Shell completion generation for CLI tools.
//!
//! Generates bash, zsh, and fish completion scripts using clap_complete.

/// Supported shell types for completion generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

impl Shell {
    /// Parse shell name from CLI input string.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            "powershell" | "ps" => Some(Self::PowerShell),
            _ => None,
        }
    }

    /// Return the shell name as used by clap_complete.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::PowerShell => "powershell",
        }
    }
}

/// Generate shell completion script content for a given binary name.
///
/// This produces a basic completion script that covers subcommands and flags.
/// For full integration with clap_complete, CLI tools should call
/// `clap_complete::generate()` directly with their `Command` struct.
pub fn generate_completion_hint(shell: Shell, bin_name: &str) -> String {
    match shell {
        Shell::Bash => format!(
            r#"# bash completion for {bin}
_{bin}_completions() {{
    local cur prev opts
    COMPREPLY=()
    cur="${{COMP_WORDS[COMP_CWORD]}}"
    prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    opts=$({bin} --help 2>/dev/null | grep -oP '^\s+\K\S+' | head -50)
    COMPREPLY=( $(compgen -W "$opts" -- "$cur") )
}}
complete -F _{bin}_completions {bin}
"#,
            bin = bin_name
        ),
        Shell::Zsh => format!(
            r#"#compdef {bin}
_arguments '*:filename:_files'
"#,
            bin = bin_name
        ),
        Shell::Fish => format!(
            r#"# fish completions for {bin}
complete -c {bin} -f
complete -c {bin} -a '(command {bin} --help 2>/dev/null | string match -r "^\s+\S+")'
"#,
            bin = bin_name
        ),
        Shell::PowerShell => format!(
            r#"# PowerShell completion for {bin}
Register-ArgumentCompleter -Native -CommandName '{bin}' -ScriptBlock {{
    param($wordToComplete, $commandAst, $cursorPosition)
    & {bin} --help 2>$null | ForEach-Object {{
        if ($_ -match '^\s+(\S+)') {{
            $matches[1]
        }}
    }} | Where-Object {{ $_ -like "$wordToComplete*" }} | ForEach-Object {{
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }}
}}
"#,
            bin = bin_name
        ),
    }
}
