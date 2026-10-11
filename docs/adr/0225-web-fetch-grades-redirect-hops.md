# 0225. `web_fetch` grades every redirect hop against the session's rules

- Status: Accepted
- Date: 2026-10-11
- Amends: [ADR-0220](0220-web-fetch-ssrf-guard-and-clipboard-image-paste.md)
  (§1, "Rules grade the requested URL only")

## Context

ADR-0220 graded only the URL the model asked for. Redirect hops went through
the SSRF guard but were not checked against the mode's rules. That left two
gaps (#808):

- An allow-listed site with an open redirect could reach any public host.
- A host behind a `deny` rule could be reached by redirecting from an allowed
  one.

Grading happens in the executor's dispatch gate, before the tool runs. The
tool had no way to grade anything itself.

## Decision

1. **A call's grading context is reachable while it runs.** The executor
   wraps each graded dispatch in a `CallGrader` scope
   (`tool_runner::call_grader`), stored as a tokio task-local. This is the
   same mechanism `file_backup::with_exec_seq` already uses. The `CallGrader`
   owns everything dispatch grades with: the resolver, the ancestor chain,
   grants, the overlay entry and overlay deny, the ceiling and the mode.
   Dispatch and the grader share one grading function,
   `dispatch::grade::grade_call`, extracted from `dispatch` itself. A hop
   therefore grades *exactly* like a fresh call: same mode rules, same chain
   clamp, same grants, same overlay, same ceiling, and the same normalized-URL
   argument. `tool_runner::grade_in_scope(tool, input)` is the public entry
   point. It returns `None` outside an executor scope.
2. **The `Tool` trait is unchanged.** A task-local keeps the dozens of tools
   that never need this untouched. A per-call trait argument would have
   changed every implementation for one consumer.
3. **The provider gets a hop seam.** `WebFetcher::fetch_checked(url, cap,
   check_hop)` calls `check_hop` with each redirect target's normalized URL
   *before* requesting it. An `Err` stops the fetch. `fetch` is
   `fetch_checked` with a no-op check.
4. **Outcomes for each hop grade:**
   - `Allow`: the fetch follows the redirect.
   - `Deny`: the fetch stops with
     `redirect to <url> not followed: web_fetch of that URL is denied by mode
     <mode>`. This mirrors dispatch's own deny message. The resolver returns
     only a grade, so the matching rule is not named.
   - `Ask`: the fetch stops and tells the model the target needs approval and
     that it can call `web_fetch` on that URL directly. That direct call goes
     through the ordinary approval flow. Parking an approval in the middle of
     a fetch is rejected: it would hold a connection and the converter budget
     open while waiting for a human, and the direct retry already provides
     the same decision.
5. **What hops skip.** Hooks and the escape-root gate apply only to the
   dispatched call, not to its hops. Neither has meaning for a URL.
6. **Outside an executor.** When a tool runs directly with no executor (for
   example, an embedder's own loop), no grader is in scope. Hops are followed,
   still under the SSRF guard. The initial URL was not graded there either,
   so nothing is lost.

## Consequences

- An open redirect on an allowed site can no longer reach a host the rules
  exclude, and a host-level `deny` holds across redirects.
- In `research`/`plan`/`auto`, which prompt for every fetch, an approved
  fetch that redirects to a different URL stops at the hop. Approving the
  hop's URL, or a host-prefix rule covering it, is what lets it through.
  `build` (class-allowed `exec`) is unaffected.
- ADR-0220's caveat about host-wildcard rules (`*` also matches `/`) still
  stands. Write rules as a literal `scheme://host/` prefix.
