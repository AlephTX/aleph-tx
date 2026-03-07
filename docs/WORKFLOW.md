# Development Workflow

> Process guidelines for working on AlephTX. Not auto-loaded - consult when starting new tasks.

## Problem-Solving Process (Search → Plan → Action)

We follow a structured **Search → Plan → Action** workflow. Each phase transition requires confirmation from the collaborator.

1. **Search Phase**: Identify and read all relevant code and files. Summarize findings and build an index.
2. **Plan Phase** (after collaborator confirmation): Create a high-level abstract design. Keep changes minimal, concise, and robust. Plans may be revised multiple times based on collaborator feedback.
3. **Todo Discussion** (after collaborator confirmation): Discuss todo items - prioritize what to do and what to skip.
4. **Action Phase**: Execute each todo item, review after completion, and summarize.

## Code Summarization

1. Before starting any task, identify all code files to read. Create concise but thorough index documents (e.g., `xxx.py` -> `xxx.py.md`).
2. When reading code, create Mermaid diagrams for:
   - Internal class interaction diagrams
   - Inheritance hierarchies
   - Module-level architecture diagrams

## Task Execution Steps

1. **Identify** all relevant code and files for the problem.
2. **Deep-read** the code, trace call chains and dependencies.
3. **Create** a detailed todolist based on analysis.
4. **Execute** each todo item.
5. **Review** each completed todo - ensure code is clean and robust.
6. **Summarize** each completed todo.
7. **Final summary** of the entire task, ending with a timestamp in `{YYYY.MM.DD.HH}` format. Create README files in each directory explaining purpose, features, usage, and testing.

---

## Claude Code Project Management Rules

### Directory Structure (MUST follow)

`task_name` is provided by the user. If not provided, Claude Code derives a name from the task content.

```
@CLAUDECODE/tasks/{task_name}/          # Root directory for each task
@CLAUDECODE/tasks/{task_name}/todos/    # Todolist files
@CLAUDECODE/tasks/{task_name}/traces/   # Execution trace files for the task
@CLAUDECODE/tasks/{task_name}/tests/    # Test files created during task execution
@CLAUDECODE/tasks/{task_name}/docs/     # Summary documentation
@CLAUDECODE/tasks/{task_name}/others/   # Uncategorized files
```

### File Naming Convention

All filenames MUST use English names to avoid encoding issues in terminals that don't support CJK characters.

### Trace Management

1. Maintain trace files for task state tracking.
2. Save trace content to the `traces/` directory under the task.
3. All files within a single session are saved to the same directory.

### Test Code Management

1. You may write tests during problem-solving, but MUST clean up afterward.
2. NEVER create test files in the project root - always use `@CLAUDECODE/tasks/{task_name}/tests/`.
3. Before deleting test files, confirm with the user whether they can be removed.
