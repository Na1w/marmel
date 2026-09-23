//! Code-pattern heuristics for text repetition detection (REQ-HARN-003).

pub(crate) fn is_code_pattern(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Delimiters common in programming syntax, array literals, and struct initializers
    if trimmed.contains(',')
        || trimmed.contains(';')
        || trimmed.contains('(')
        || trimmed.contains(')')
        || trimmed.contains('[')
        || trimmed.contains(']')
        || trimmed.contains('{')
        || trimmed.contains('}')
        || trimmed.contains("->")
        || trimmed.contains("::")
        || trimmed.contains("=>")
        || trimmed.contains('=')
        || trimmed.contains("0x")
    {
        return true;
    }
    // Common primitive literals or numeric sequences
    matches!(
        trimmed,
        "true" | "false" | "null" | "None" | "Some" | "Ok" | "Err" | "nil" | "undefined"
    ) || trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '_')
}

pub(crate) fn is_code_line(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Markdown code block delimiters
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        return true;
    }

    // Comments, attributes, macros, preprocessor directives
    if trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with('#')
        || trimmed.starts_with('@')
    {
        return true;
    }

    // Typical code line terminators and structural brackets
    if trimmed.ends_with(';')
        || trimmed.ends_with('{')
        || trimmed.ends_with('}')
        || trimmed.ends_with(',')
        || trimmed.ends_with(')')
        || trimmed.ends_with(']')
        || trimmed.ends_with(':')
        || trimmed.ends_with('\\')
    {
        return true;
    }

    // Contains code operators / punctuation
    if trimmed.contains("::")
        || trimmed.contains("->")
        || trimmed.contains("=>")
        || trimmed.contains("==")
        || trimmed.contains("!=")
        || trimmed.contains("<=")
        || trimmed.contains(">=")
        || trimmed.contains("+=")
        || trimmed.contains("-=")
        || trimmed.contains("*=")
        || trimmed.contains("&&")
        || trimmed.contains("||")
        || trimmed.contains("()")
        || trimmed.contains("[]")
        || trimmed.contains("{}")
        || trimmed.contains("println!")
        || trimmed.contains("assert!")
        || trimmed.contains("assert_eq!")
    {
        return true;
    }

    // Leading programming language keyword
    let first_word = trimmed
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or("");
    matches!(
        first_word,
        "fn" | "pub"
            | "let"
            | "mut"
            | "const"
            | "static"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "type"
            | "use"
            | "mod"
            | "return"
            | "if"
            | "else"
            | "match"
            | "for"
            | "while"
            | "loop"
            | "break"
            | "continue"
            | "def"
            | "class"
            | "import"
            | "from"
            | "var"
            | "val"
            | "function"
            | "export"
            | "public"
            | "private"
            | "protected"
            | "case"
            | "default"
            | "switch"
            | "try"
            | "catch"
            | "finally"
            | "throw"
            | "async"
            | "await"
            | "package"
            | "include"
            | "SELECT"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
            | "WHERE"
            | "FROM"
            | "CREATE"
    )
}

pub(crate) fn is_code_word(w: &str) -> bool {
    let lower = w.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "fn" | "pub"
            | "let"
            | "mut"
            | "const"
            | "static"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "type"
            | "use"
            | "mod"
            | "return"
            | "if"
            | "else"
            | "match"
            | "for"
            | "while"
            | "loop"
            | "break"
            | "continue"
            | "def"
            | "class"
            | "import"
            | "from"
            | "var"
            | "val"
            | "function"
            | "export"
            | "public"
            | "private"
            | "protected"
            | "case"
            | "default"
            | "switch"
            | "try"
            | "catch"
            | "finally"
            | "throw"
            | "async"
            | "await"
            | "self"
            | "super"
            | "crate"
            | "true"
            | "false"
            | "none"
            | "some"
            | "ok"
            | "err"
            | "nil"
            | "null"
            | "int"
            | "i32"
            | "i64"
            | "u32"
            | "u64"
            | "usize"
            | "isize"
            | "str"
            | "string"
            | "vec"
            | "bool"
            | "void"
            | "char"
            | "float"
            | "double"
            | "assert"
            | "asserteq"
            | "println"
            | "printf"
            | "stdout"
            | "stderr"
    )
}

pub(crate) fn is_markdown_divider(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed
        .chars()
        .all(|c| c == '-' || c == '=' || c == '*' || c == '`' || c == '#' || c == '_')
}

// ---------------------------------------------------------------------------
// Orphan tool-message pruning (Part 7 checklist item 5)
// ---------------------------------------------------------------------------
