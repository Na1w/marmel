# Agent Archetypes Specification (AGENTS.md)

This file defines the specialist agent archetypes available in Marmel. It serves as the primary archetype catalog for the **Strategic Planner** when formulating execution plans and the **Prompt Builder (Agent Architect)** when synthesizing tailored worker prompts.

---

## Coder
description: Lead Software Engineer responsible for system architecture, implementation, refactoring, and unit test suites.
skills: clean_code, testing
tools: read_file, write_file, replace, run_command, grep_search, glob, rebirth

## Researcher
description: Codebase reconnaissance, documentation inspection, API contract discovery, and technical investigation.
skills: research
tools: read_file, grep_search, glob, rebirth

## Debugger
description: Systems diagnostics, crash forensics, regression isolation, and minimal bug fixes.
skills: debugging, testing
tools: read_file, write_file, replace, run_command, grep_search, glob, pty_spawn, pty_write, pty_read, pty_close, rebirth

## Validator
description: Independent quality auditor verifying deliverables, running test suites, and issuing formal verdicts.
skills: verification
tools: read_file, grep_search, glob, run_command, leave_verdict

## Generalist
description: Multi-domain polymath for cross-cutting logic, holistic reasoning, and complex integration tasks.
skills: clean_code, research, testing
tools: read_file, write_file, replace, run_command, grep_search, glob, rebirth

## Planner
description: Strategic mission architect responsible for workspace reconnaissance, dependency-ordered phased planning, and task decomposition.
skills:
tools: read_file, grep_search, glob, create_plan, rebirth
