{# The planner reads an issue and writes the contract every later stage is
   measured against. Everything downstream sees this, not the raw issue. #}
An issue was filed on `{{ repo }}`. Turn it into a build order that a small
model can execute without guessing.

## Issue #{{ issue.number }}: {{ issue.title }}

{{ issue.body }}

{% if issue.labels %}Labels: {{ issue.labels | join(", ") }}{% endif %}

## Repository

Default branch: `{{ base_branch }}`

Files (truncated):
```
{{ file_tree }}
```

{% if readme %}
Project README (first {{ readme_limit }} characters):
```
{{ readme }}
```
{% endif %}

## What to produce

Write the plan as JSON with this shape:

```json
{
  "language": "detected project language/stack (e.g. 'Rust', 'Python (pytest)', 'Node.js (TypeScript)', 'Go', 'Plain / Multi-purpose')",
  "goal": "one paragraph, in the repository's own vocabulary, describing what done looks like",
  "acceptance": ["checkable statement", "..."],
  "constraints": ["things the change must not do"],
  "allowed_paths": ["glob patterns the patch may touch"],
  "relevant_files": ["paths worth reading before writing anything"],
  "test_plan": ["how the change gets proven"],
  "setup_commands": ["shell commands to install dependencies, e.g. 'npm install', or leave empty [] if none"],
  "build_commands": ["shell commands to compile/build, e.g. 'cargo build', or leave empty [] if none"],
  "test_commands": ["shell commands to run automated tests, e.g. 'pytest', 'cargo test', 'npm test', or leave empty [] if none"],
  "lint_commands": ["shell commands to lint/format check, or leave empty [] if none"],
  "notes": "anything the workers need that does not fit above",
  "difficulty": 3
}
```

Rules that matter:

- `language`: inspect the file tree (e.g. `Cargo.toml` -> Rust, `package.json` -> Node.js, `pyproject.toml`/`requirements.txt` -> Python, `go.mod` -> Go) and identify the language and tools.
- `setup_commands`, `build_commands`, `test_commands`, `lint_commands`: specify executable shell commands appropriate for this project's stack. If the repository has no test suite or is empty/text, leave the list empty `[]`.
- `acceptance` items are graded literally by a reviewer that has not read the
  issue. Write them so a yes or no answer is possible. "Cache entries are
  dropped when the underlying row is updated" is gradable. "Caching works
  properly" is not.
- `allowed_paths` is enforced by machine, not suggested. A patch touching
  anything outside it is rejected before a reviewer ever sees it. Be generous
  enough that the real fix fits, tight enough that an unrelated refactor does
  not. Use `**` freely. Leave the list empty only when you genuinely cannot
  predict the area.
- `relevant_files` must be paths that exist in the tree above.
- `difficulty` is 1 for a typo and 5 for something that changes how the system
  is put together.
- If the issue is too vague to act on, say so in `notes` and set `difficulty`
  to 5, rather than inventing requirements.

Answer with the JSON object and nothing else.
