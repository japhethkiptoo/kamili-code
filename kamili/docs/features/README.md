# Feature specs

One markdown file per feature: `docs/features/<feature>.md`, where `<feature>` uses letters, digits, `-` or `_`. Kamili's own specs live here too (dogfooding).

## Format

```markdown
---
status: draft
---
# Feature title

One-paragraph goal.

## Requirements
- testable statement

## Out of Scope
## Acceptance Criteria
## Open Questions

## Architecture           (written by the Architect)
### Approach
### Components
### Data and interfaces
### Decisions
### Risks
### Testing

## Change Log
## API Contracts
## Ops Notes
```

- Front matter is `key: value` lines. `status` (`draft` or `approved`) is required. Other keys are kept as they are.
- Sections are split on `## ` headings. Headings inside code fences are ignored.
- Only an `approved` spec can be built (`--force` overrides this).

## Who may edit what

| Editor | Sections |
|---|---|
| Analyst (spec session) | the whole spec except `Architecture`; you approve it before it is written |
| Architect (spec session) | `Architecture` only; you approve it with the rest of the draft |
| Planner (build) | everything except `Architecture` and the Documentor's sections |
| Documentor (build) | `Change Log`, `API Contracts`, `Ops Notes` |
| Worker (build) | no spec files at all |

These rules are enforced in the engine's `spec` module and the build pipeline, not in the prompts.
