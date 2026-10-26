# Anthropic PR code review (GitHub Actions)

This repository runs an automated Claude code review on pull requests opened
from branches in the same repository. The workflow posts a PR comment and
exposes a CI check you can mark as required.

## Setup

1. Create an [Anthropic API key](https://console.anthropic.com/) with access to
   the Messages API.
2. In GitHub: **Settings → Secrets and variables → Actions → New repository
   secret**
   - Name: `ANTHROPIC_API_KEY`
   - Value: your API key (never commit it to the repo).
3. If Anthropic returns an error about **workspace id**, add another repository
   secret:
   - Name: `ANTHROPIC_WORKSPACE_ID`
   - Value: workspace ID from [Anthropic Console](https://console.anthropic.com/)
     (organization/workspace settings). Alternatively, create an API key that is
     already scoped to one workspace.
4. Open or update a pull request; the **Anthropic PR review** workflow runs on
   `opened`, `synchronize`, and `reopened`.

## Required check (branch protection)

To block merges when Claude returns **FAIL**:

1. **Settings → Branches → Branch protection rules** (for `main` or your
   default branch).
2. Enable **Require status checks to pass before merging**.
3. Search for and select **`anthropic-review`** (job name in
   `.github/workflows/anthropic-review.yml`).

The check **passes** on verdict PASS and **fails** on FAIL. The workflow does
not approve or dismiss reviews; it only comments and sets job status.

## Model

The model ID is pinned in the workflow `env` as `ANTHROPIC_MODEL` (currently
`claude-sonnet-5-5`). Change that variable to switch models without
editing the review script.

## Fork and cross-repo limitations

- The workflow uses `pull_request` (not `pull_request_target`) and runs only
  when `head` repo equals the base repo.
- Pull requests from forks **skip** this job (GitHub treats a skipped required
  check as passing). Forks cannot use the parent repo’s Actions secrets safely
  with this design; do not rely on this check for fork PRs.
- Contributors with write access should use branches on the main repository for
  PRs that need this check.

## What Claude checks

The prompt follows [AGENTS.md](../AGENTS.md) themes: correctness, tests,
security on ingest/HTTP and on-disk formats, no silent format breaks, minimal
scope, and items from the security-audit skill checklist.

Large diffs are truncated at **100k bytes**; the model is instructed to return **FAIL**
if it cannot fully assess a truncated change.

## Troubleshooting

| Symptom | Likely cause |
| -------- | ------------- |
| Job skipped | PR is from a fork or external head repo |
| Missing secret error | Add `ANTHROPIC_API_KEY` in Actions secrets |
| HTTP 400 workspace id | Add `ANTHROPIC_WORKSPACE_ID` or use a workspace-scoped key |
| HTTP 404 model | Update `ANTHROPIC_MODEL` in the workflow to a valid API id (e.g. `claude-sonnet-5-5`) |
| API HTTP error | Key invalid, billing, or model ID unavailable |
| Parse error | Model returned non-JSON; re-run the workflow |

Do not print or log the API key in workflow logs.
