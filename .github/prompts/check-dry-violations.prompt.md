---
agent: 'agent'
description: 'Check DRY violations'
tools: ['runCommands', 'edit', 'runTasks', 'search', 'usages', 'problems', 'changes', 'testFailure', 'fetch', 'githubRepo']
---

# 🔍 Check DRY violations

## 🎯 Objective
Analyze the codebase for the main crates machine (the orchestrator deploying podman pods), meshproxy (the ingress dht that the sidecar connects to), sidecar (the container being added to every pod to form a service mesh). Also look at podctl and the shared crates.

Your goals:

1. Detect DRY Violations
* Identify duplicated logic, patterns, or structures.
* Suggest refactoring approaches:
* Extract shared code into functions, traits, or methods.
* Use generics, iterators, or macros when appropriate.
* Point out repeated constant values or magic numbers.

2. Improve Idiomatic Rust Style
* Check the code for alignment with modern Rust conventions:
* Prefer Option / Result ergonomics (?, map, and_then, ok_or).
* Prefer iterators over loops when it improves clarity.
* Favor &str over String when ownership isn’t needed.
* Use pattern matching instead of long if let chains when appropriate.
* Avoid unnecessary clones and allocations.
* Encourage small, cohesive functions.

3. Ensure Safety and Correctness
* Identify unnecessary unsafe or unsafe patterns.
* Check for ownership/borrowing mistakes or needless clone() calls.
* Ensure error handling follows best practices.
* Encourage use of enums vs Boolean flags or magic values.

4. Recommend Rust Best Practices
* Encourage usage of:
* From / Into traits for conversions
* Display vs Debug correctly
* Default and builder patterns when struct construction gets complex
* Cow, Arc, RefCell, or Mutex only when justified
* Call out missing docs on public APIs (/// comments).

5. Output Format
When reviewing code:
* List DRY issues (explain where duplication exists).
* List idiomatic improvements (with short examples).
* List safety / correctness concerns.
* Provide a refactored snippet where useful.
* Keep suggestions concise, actionable, and Rust-idiomatic.