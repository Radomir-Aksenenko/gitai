{# One reviewer per surviving attempt. Everything it sees has already cleared
   the gate, so build and test status are settled and not up for discussion. #}
Review one candidate patch for `{{ repo }}`. It has already passed the build,
the tests and the scope check, so judge what those cannot: whether it does the
right thing, and whether it is code the repository should keep.

## Goal

{{ spec.goal }}

## Acceptance criteria

{% for a in spec.acceptance %}{{ loop.index }}. {{ a }}
{% endfor %}

{% if spec.constraints %}
## Constraints

{% for c in spec.constraints %}- {{ c }}
{% endfor %}
{% endif %}

## Gate result

```
{{ gate_summary }}
```

Lines: +{{ gate.insertions }} / -{{ gate.deletions }} across {{ gate.changed_files | length }} file(s)

## Patch

```diff
{{ diff }}
```

## What to produce

JSON only:

```json
{
  "approved": true,
  "score": 0,
  "summary": "one paragraph",
  "blocking": ["reasons this cannot merge"],
  "suggestions": ["improvements that are not blockers"]
}
```

Grade against the numbered criteria above, one at a time, and say in `summary`
which are met. Then look for the things a passing test suite hides:

- behaviour the patch changed that nobody asked for
- an error path that is now silently swallowed
- a test that was weakened or deleted to make something pass
- a fix applied at the call site when the defect is in the callee
- values that should be configuration and are hardcoded

`score` is 0 to 100 and is only ever compared against other attempts at the
same task, so use the range. A patch that satisfies every criterion cleanly is
above 80. One that works but leaves a mess is around 50. One that misses a
criterion is below 30.

`blocking` non-empty means `approved` must be false. An empty `blocking` list
with `approved` false is a contradiction, so do not produce one.

**Language**: Write `summary`, `blocking`, and `suggestions` in the **SAME LANGUAGE as the Goal/Issue** (e.g., Russian if the goal is in Russian, English if in English).

Answer with the JSON object and nothing else.
