---
id: verification
name: Formal Quality Verification & Auditing
description: Independent quality auditing, code inspection, compliance verification, and formal verdict submission.
suggested_tools:
  - read_file
  - grep_search
  - glob
  - leave_verdict
---

## Formal Quality Verification & Auditing
- **INDEPENDENT VERIFICATION:** Inspect actual file changes and test outputs directly on disk. Never approve based purely on agent claims or chat summaries.
- **READ-ONLY AUDITING:** Verify code correctness, architectural hygiene, absence of stubs, and spec conformance without modifying source files.
- **FORMAL VERDICT SUBMISSION:**
  - Submit the final audit verdict strictly using the `leave_verdict` tool.
  - If all acceptance criteria and quality bars are met: `leave_verdict(verdict="APPROVED", comments="Deliverable verified.")`
  - If defects, regressions, or missing criteria are found: `leave_verdict(verdict="REJECTED", comments="<concrete actionable defects>")`
