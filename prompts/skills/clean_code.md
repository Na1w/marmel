---
id: clean_code
name: Clean Code & Architecture
description: Software architecture, modular design, SOLID principles, and surgical code modification.
suggested_tools:
  - read_file
  - write_file
  - replace
  - grep_search
  - glob
---

## Clean Code & Software Architecture
- **MODULAR DESIGN:** Keep source code files small, focused, and single-purpose. Prefer composition and clean abstractions.
- **SURGICAL OPERATIONS:**
  - Before writing or modifying code, inspect existing patterns and signatures using `read_file`, `grep_search`, and `glob`.
  - Use `replace` for targeted, surgical changes inside existing files to minimize regression risk.
  - Use `write_file` to create new files or write comprehensive rewrites directly to disk. Never output raw file contents in chat as a substitute for disk operations.
- **ZERO HALLUCINATION:** Never invent external APIs, library signatures, or types. Verify against existing definitions and compiler outputs.
- **ERROR HANDLING:** Write robust error handling. Do not ignore errors or use unsafe unwraps in production code.
