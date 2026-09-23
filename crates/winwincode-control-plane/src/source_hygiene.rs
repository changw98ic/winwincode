// SPDX-License-Identifier: Apache-2.0
//!
//! Product-side source hygiene (judge 2026-09-23): extract → sanitize → validate → write.
//! Never ship truncated / prose-laden / escaped-dump code.

/// Strip markdown fences and analysis prose around a source body.
#[must_use]
pub fn sanitize_source(content: &str) -> String {
    let mut text = content.replace("\r\n", "\n");
    for fence in [
        "```python\n",
        "```rust\n",
        "```go\n",
        "```cpp\n",
        "```csharp\n",
        "```typescript\n",
        "```ts\n",
        "```zig\n",
        "```c\n",
        "```json\n",
        "```",
    ] {
        text = text.replace(fence, "");
    }
    if text.contains("\\n") && !text.contains('\n') {
        text = text
            .replace("\\n", "\n")
            .replace("\\t", "\t")
            .replace("\\\"", "\"");
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut start = 0usize;
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let looks_code = [
            "use ",
            "import ",
            "from ",
            "package ",
            "fn ",
            "pub ",
            "def ",
            "class ",
            "struct ",
            "impl ",
            "#include",
            "using ",
            "namespace ",
            "const ",
            "let ",
            "func ",
            "function ",
            "export ",
            "//",
            "#!",
            "#[",
            "module ",
            "type ",
            "public ",
            "static ",
        ]
        .iter()
        .any(|p| trimmed.starts_with(p));
        if looks_code {
            start = idx;
            break;
        }
    }
    let text = lines[start..].join("\n");
    let lines: Vec<&str> = text.lines().collect();
    let mut end = lines.len();
    while end > 0 {
        let trimmed = lines[end - 1].trim();
        let prose = trimmed.is_empty()
            || trimmed.starts_with("Hope")
            || trimmed.starts_with("Note:")
            || trimmed.starts_with("Here ")
            || trimmed.starts_with("This ")
            || trimmed.ends_with('?')
            || trimmed.contains("希望")
            || trimmed.contains("如上");
        let looks_code = trimmed.ends_with('{')
            || trimmed.ends_with('}')
            || trimmed.ends_with(';')
            || trimmed.ends_with(':')
            || trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.contains("fn ")
            || trimmed.contains("def ")
            || trimmed.contains("function ")
            || trimmed.contains("func ")
            || trimmed.contains("return ");
        if prose && !looks_code {
            end -= 1;
        } else {
            break;
        }
    }
    lines[..end].join("\n").trim_end().to_owned()
}

/// Heuristic gate before write: reject ellipsis dumps and escaped-only payloads.
#[must_use]
pub fn looks_like_real_source(content: &str) -> bool {
    if content.trim().len() < 200 {
        return false;
    }
    if content.matches("...").count() >= 3 && content.lines().count() < 40 {
        return false;
    }
    if content.contains("省略") || content.contains("omitted for brevity") {
        return false;
    }
    if content.matches("\\n").count() > 5 && !content.contains('\n') {
        return false;
    }
    // TypeScript `function` does not contain `func ` (trailing space).
    [
        "fn ", "def ", "func ", "function ", "class ", "struct ", "impl ", "public ",
        "#include", "using ", "export ", "const ", "let ", "interface ", "type ",
        "module ", "pub ", "package ", "import ", "namespace ", "void ", "int ", "std::",
    ]
    .iter()
    .any(|p| content.contains(p))
}

/// JSONL protocol body for an illegal JSON line (PROTOCOL.md).
#[must_use]
pub fn protocol_invalid_json_response() -> &'static str {
    "{\"error\":\"INVALID_JSON\"}"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_prose_and_fences() {
        let raw = "Here is the solution:\n```python\ndef main():\n    print(1)\n    x = 2\n    y = 3\n    return x+y\n```\nHope this helps!";
        let clean = sanitize_source(raw);
        assert!(clean.contains("def main():"));
        assert!(!clean.contains("```"));
        assert!(!clean.contains("Hope this helps"));
    }

    #[test]
    fn rejects_truncated_dumps() {
        assert!(!looks_like_real_source("code omitted... ... ..."));
        assert!(!looks_like_real_source("分析文字\n...\n...\n"));
    }

    #[test]
    fn unescapes_json_string_body() {
        let raw = "def main():\\n    return 0\\n";
        let clean = sanitize_source(raw);
        assert!(clean.contains('\n'));
    }

    #[test]
    fn protocol_body_is_invalid_json_object() {
        assert_eq!(protocol_invalid_json_response(), "{\"error\":\"INVALID_JSON\"}");
    }

    #[test]
    fn accepts_function_style_typescript() {
        let src = concat!(
            "export function handle(line) {\n",
            "  try { JSON.parse(line); } catch (e) { return bad(); }\n",
            "  return line;\n",
            "}\n",
            "export function main() { handle('x'); handle('y'); handle('z'); }\n",
            "function bad() { return 1; }\n",
            "// padding padding padding padding padding padding padding\n",
        );
        assert!(src.len() >= 200);
        assert!(looks_like_real_source(src), "ts function style must pass hygiene");
    }
}
