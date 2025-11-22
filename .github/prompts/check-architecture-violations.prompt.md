---
agent: 'agent'
description: 'Check architecture violations'
tools: ['runCommands', 'runTasks', 'Azure MCP/search', 'search', 'usages', 'problems', 'changes', 'testFailure', 'fetch', 'githubRepo']
---

# 🔍 Check architecture violations

## 🎯 Objective
Analyze the codebase for the machineplane and the custom prompts in .github/prompts and validate them with the architecture under (../Readme.md & ../machineplane/Readme.md & ../meshproxy/Readme.md & ../sidecar/Readme.md). Find any inconsistencies or missing documentation or invalid / contradicting implementions.

Highlight discrepancies and suggest changes for the specific files.